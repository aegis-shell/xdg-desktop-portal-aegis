//! `org.freedesktop.impl.portal.Request` objects and their cancellation
//! bookkeeping.
//!
//! The portal frontend passes the exact object path in each backend method's
//! `handle` argument. The backend exports one object at that path for the
//! duration of the method call and removes it before returning the
//! `(response, results)` reply. `Close` records the path in the shared
//! tracker; workers check it before and after interactive work so a racing
//! cancellation answers with response code 1.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use zbus::zvariant::Value;

pub mod sync;

pub type PortalResults = std::collections::HashMap<String, Value<'static>>;
pub type PortalResponse = (u32, PortalResults);
pub type ResponseSender = async_channel::Sender<PortalResponse>;

/// Cancellation state shared between every served `Request` object and the
/// capture worker.
#[derive(Default)]
pub struct RequestTracker {
    closed: HashSet<String>,
}

impl RequestTracker {
    /// Whether `Close` arrived for this request path.
    pub fn was_closed(&self, path: &str) -> bool {
        self.closed.contains(path)
    }

    /// Drop all state for a finished request.
    fn forget(&mut self, path: &str) {
        self.closed.remove(path);
    }
}

/// The served request object. The portal spec gives it only `Close`.
struct RequestIface {
    path: String,
    tracker: Arc<Mutex<RequestTracker>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Request")]
impl RequestIface {
    async fn close(&self) -> zbus::fdo::Result<()> {
        log::info!("portal: request {} closed by client", self.path);
        sync::lock(&self.tracker, "request tracker")
            .closed
            .insert(self.path.clone());
        Ok(())
    }
}

/// Export the backend request object at the exact path supplied by the portal
/// frontend. A duplicate handle is a protocol error rather than an
/// opportunity to share cancellation state between calls.
pub async fn register(
    conn: &zbus::Connection,
    tracker: &Arc<Mutex<RequestTracker>>,
    path: &str,
) -> zbus::fdo::Result<()> {
    let inserted = conn
        .object_server()
        .at(
            path,
            RequestIface {
                path: path.to_string(),
                tracker: Arc::clone(tracker),
            },
        )
        .await
        .map_err(zbus::fdo::Error::from)?;
    if !inserted {
        return Err(zbus::fdo::Error::Failed(format!(
            "request handle {path} is already active"
        )));
    }
    Ok(())
}

/// Remove a finished request object and its cancellation marker.
pub async fn finish(conn: &zbus::Connection, tracker: &Arc<Mutex<RequestTracker>>, path: &str) {
    if let Err(error) = conn.object_server().remove::<RequestIface, _>(path).await {
        log::warn!("portal: could not remove request {path}: {error}");
    }
    sync::lock(tracker, "request tracker").forget(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead as _, BufReader};
    use std::process::{Child, Command, Stdio};

    #[test]
    fn tracker_records_and_forgets_closes() {
        let mut tracker = RequestTracker::default();
        assert!(!tracker.was_closed("/r/1"));
        tracker.closed.insert("/r/1".to_string());
        assert!(tracker.was_closed("/r/1"));
        tracker.forget("/r/1");
        assert!(!tracker.was_closed("/r/1"));
    }

    #[test]
    fn close_marks_the_request_closed() {
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        let iface = RequestIface {
            path: "/r/1".to_string(),
            tracker: Arc::clone(&tracker),
        };
        zbus::block_on(iface.close()).expect("Close answers Ok");
        assert!(sync::lock(&tracker, "test tracker").was_closed("/r/1"));
    }

    /// A private session bus (a spawned `dbus-daemon`), mirroring the
    /// daemon's end-to-end test fixture; killed on drop. `None` when
    /// dbus-daemon is not installed (the bus-dependent tests skip).
    struct PrivateBus {
        address: String,
        child: Child,
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn private_bus() -> Option<PrivateBus> {
        let mut child = Command::new("dbus-daemon")
            .args(["--session", "--nofork", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let stdout = child.stdout.take()?;
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).ok()?;
        let address = line.trim().to_string();
        (!address.is_empty()).then_some(PrivateBus { address, child })
    }

    fn connect(bus: &PrivateBus) -> zbus::Connection {
        zbus::block_on(async {
            zbus::connection::Builder::address(bus.address.as_str())?
                .build()
                .await
        })
        .expect("connect to the private bus")
    }

    #[test]
    fn duplicate_register_at_the_same_handle_is_an_error() {
        let Some(bus) = private_bus() else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        let conn = connect(&bus);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        zbus::block_on(register(&conn, &tracker, "/r/dup")).expect("first register succeeds");
        let error = zbus::block_on(register(&conn, &tracker, "/r/dup"))
            .expect_err("a duplicate handle is a protocol error");
        assert!(
            error.to_string().contains("already active"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn finish_removes_the_object_and_forgets_the_close_marker() {
        let Some(bus) = private_bus() else {
            eprintln!("skipping: dbus-daemon is not installed");
            return;
        };
        let conn = connect(&bus);
        let tracker = Arc::new(Mutex::new(RequestTracker::default()));
        zbus::block_on(register(&conn, &tracker, "/r/fin")).expect("register succeeds");
        sync::lock(&tracker, "test tracker")
            .closed
            .insert("/r/fin".to_string());
        zbus::block_on(finish(&conn, &tracker, "/r/fin"));
        assert!(
            !sync::lock(&tracker, "test tracker").was_closed("/r/fin"),
            "finish drops the cancellation marker"
        );
        // The object is gone from the server, so the handle is free again.
        zbus::block_on(register(&conn, &tracker, "/r/fin")).expect("the handle is reusable");
    }
}
