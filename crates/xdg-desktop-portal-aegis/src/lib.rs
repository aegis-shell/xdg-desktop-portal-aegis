//! `xdg-desktop-portal-aegis`: the portal backend for the Aegis compositor.
//!
//! A standalone D-Bus-activated process that bridges the freedesktop portal
//! backend interfaces to the compositor's scoped IPC. Outward it
//! serves `org.freedesktop.impl.portal.Settings` v1,
//! `org.freedesktop.impl.portal.Screenshot` v3,
//! `org.freedesktop.impl.portal.ScreenCast` v6,
//! `org.freedesktop.impl.portal.Secret` v1,
//! `org.freedesktop.impl.portal.Lockdown`,
//! `org.freedesktop.impl.portal.FileChooser`,
//! `org.freedesktop.impl.portal.Email`,
//! and `org.freedesktop.impl.portal.Account` at
//! `/org/freedesktop/portal/desktop` under the well-known name
//! `org.freedesktop.impl.portal.desktop.aegis`. Secret is backed by an
//! encrypted at-rest vault. FileChooser launches one portal-owned optics
//! (iris/lens) prompter process; no file data crosses compositor IPC. For
//! compositor-owned resources the backend is an ordinary scoped IPC client:
//! pixels come from `CaptureOutput` under the built-in
//! `aegis-portal` named scope with a sealed-memfd blob transfer
//! transport, screencast frames arrive through the same scope's output-frame
//! stream and are republished as a PipeWire producer stream. Account consent,
//! the file chooser, and password-mode vault unlock are Portal-owned UI and
//! do not cross compositor IPC. No Wayland capture protocol is added anywhere.
//!
//! The process uses zbus's blocking API on the session bus and plain
//! `std::thread` workers without an async runtime. Method dispatch runs on
//! zbus's internal
//! executor; the compositor IPC round-trip (which blocks for up to one frame)
//! happens on a dedicated capture worker so a slow capture never stalls the
//! bus, and every screencast runs its own PipeWire main loop on a dedicated
//! cast thread.

mod account;
mod cast;
mod email;
mod file_chooser;
mod files;
mod ipc;
mod lockdown;
mod prompter;
mod screencast;
mod screenshot;
mod session;
mod settings;

use std::sync::{Arc, Mutex, mpsc};

use aegis_portal_prompter::{PromptResult, PrompterRequest, SecretRequest};
use aegis_portal_runtime::RequestTracker;
use aegis_portal_secret::{PromptResponse, SecretError, SecretPrompter, SecretService};
use screencast::{CastJob, ScreenCastIface};
use screenshot::{CaptureJob, ScreenshotIface};
use session::SessionRegistry;
use settings::{SettingsIface, SettingsStore};

/// The well-known bus name the portal frontend resolves through the
/// `aegis.portal` file's `DBusName`.
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.aegis";
/// The object path every portal backend serves its interfaces at.
pub const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";

/// Errors that prevent the backend from coming up at all.
#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    /// No `$XDG_RUNTIME_DIR`, so the compositor IPC socket cannot be located.
    #[error("$XDG_RUNTIME_DIR is unset; cannot locate aegis.sock")]
    NoRuntimeDir,
    /// Session-bus or object-server setup failed.
    #[error("D-Bus setup failed: {0}")]
    Bus(#[from] zbus::Error),
    /// The advertised Secret interface cannot be backed safely. Refuse a
    /// misleading partial startup so D-Bus activation reports the fault.
    #[error("secret vault setup failed: {0}")]
    Secret(#[from] SecretError),
    /// An essential long-lived worker could not be created. Starting under
    /// the advertised name would otherwise expose a permanently stale or
    /// non-responsive interface.
    #[error("worker setup failed: {0}")]
    Worker(#[source] std::io::Error),
}

/// Process adapter kept at the composition root so Secret storage depends on
/// only a narrow prompt capability, not toolkit or compositor IPC.
struct PortalSecretPrompter;

impl SecretPrompter for PortalSecretPrompter {
    fn prompt_secret(
        &self,
        title: &str,
        reason: Option<&str>,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<PromptResponse, String> {
        let result = prompter::invoke(
            PrompterRequest::secret(SecretRequest {
                title: title.to_owned(),
                reason: reason.map(str::to_owned),
            }),
            Some(cancelled),
        )
        .map_err(|error| error.to_string())?;
        match result {
            PromptResult::Secret(mut response) => match response.take_value() {
                Some(value) => Ok(PromptResponse::Secret(value)),
                None => Ok(PromptResponse::Cancelled),
            },
            _ => Err("secret prompter returned the wrong response kind".to_owned()),
        }
    }
}

/// Run the backend: serve all interfaces on the session bus and spawn the
/// capture and screencast workers. The process is D-Bus-activated,
/// stays resident while the bus is connected, and exits for reactivation
/// when that connection fails.
pub fn run() -> Result<(), PortalError> {
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").ok_or(PortalError::NoRuntimeDir)?;
    let socket = std::path::PathBuf::from(runtime_dir).join("aegis.sock");

    // Global PipeWire setup for every future cast thread (idempotent).
    pipewire::init();

    let conn = zbus::blocking::connection::Builder::session()?
        .build()
        .map_err(PortalError::Bus)?;

    let tracker = Arc::new(Mutex::new(RequestTracker::default()));
    let sessions = Arc::new(Mutex::new(SessionRegistry::default()));
    const MAX_QUEUED_REQUESTS: usize = 128;
    let (jobs, rx) = mpsc::sync_channel::<CaptureJob>(MAX_QUEUED_REQUESTS);
    let (cast_jobs, cast_rx) = mpsc::sync_channel::<CastJob>(MAX_QUEUED_REQUESTS);
    let (file_chooser_jobs, file_chooser_rx) =
        mpsc::sync_channel::<file_chooser::FileChooserJob>(MAX_QUEUED_REQUESTS);
    let (account_jobs, account_rx) = mpsc::sync_channel::<account::AccountJob>(MAX_QUEUED_REQUESTS);
    let settings_store = SettingsStore::default();
    settings::prime_store(&socket, &settings_store);

    // Secret is declared in aegis.portal, so its storage is part of the
    // service's startup contract. Never acquire the bus name with that
    // advertised interface missing.
    let secret_service = SecretService::initialize(Arc::new(PortalSecretPrompter))?;

    // Serve before requesting the name so no call can arrive at a name we own
    // but do not serve yet (same ordering as the SNI tray watcher).
    conn.object_server()
        .at(DESKTOP_PATH, SettingsIface::new(settings_store.clone()))?;
    conn.object_server().at(
        DESKTOP_PATH,
        ScreenshotIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs,
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        ScreenCastIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            sessions: Arc::clone(&sessions),
            jobs: cast_jobs.clone(),
        },
    )?;
    // Stateless sandbox-policy query surface; no worker, no IPC.
    conn.object_server()
        .at(DESKTOP_PATH, lockdown::LockdownIface::default())?;
    conn.object_server().at(
        DESKTOP_PATH,
        file_chooser::FileChooserIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: file_chooser_jobs.clone(),
        },
    )?;
    // Email hand-off is fire-and-forget (no worker, no IPC).
    conn.object_server().at(
        DESKTOP_PATH,
        email::EmailIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        account::AccountIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: account_jobs.clone(),
        },
    )?;

    secret_service.register_portal(&conn, Arc::clone(&tracker), DESKTOP_PATH)?;
    secret_service.start_pam_watcher();

    let worker_tracker = Arc::clone(&tracker);
    let worker_socket = socket.clone();
    std::thread::Builder::new()
        .name("aegis-portal-capture".to_string())
        .spawn(move || screenshot::capture_worker(rx, worker_tracker, worker_socket))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!("spawn capture worker: {e}")))
        })?;

    let cast_worker_conn = conn.clone();
    let cast_worker_tracker = Arc::clone(&tracker);
    let cast_worker_socket = socket.clone();
    std::thread::Builder::new()
        .name("aegis-portal-screencast".to_string())
        .spawn(move || {
            screencast::cast_worker(
                cast_rx,
                cast_jobs,
                cast_worker_conn,
                cast_worker_tracker,
                sessions,
                cast_worker_socket,
            )
        })
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!(
                "spawn screencast worker: {e}"
            )))
        })?;

    // FileChooser dispatches one supervised UI task/process per request and
    // never shares the compositor capture worker.
    let file_chooser_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-file-chooser".to_string())
        .spawn(move || file_chooser::file_chooser_worker(file_chooser_rx, file_chooser_tracker))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!(
                "spawn file chooser worker: {e}"
            )))
        })?;

    let account_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-account".to_string())
        .spawn(move || account::account_worker(account_rx, account_tracker))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!("spawn account worker: {e}")))
        })?;

    settings::spawn_watcher(conn.clone(), socket, settings_store).map_err(PortalError::Worker)?;

    conn.request_name(BUS_NAME)?;

    log::info!(
        "portal: serving Settings+Screenshot+ScreenCast+Secret+Lockdown+FileChooser+Email+Account as {BUS_NAME}"
    );

    // Keep the main thread tied to the bus connection. A disconnected
    // backend must exit so D-Bus activation can start a fresh process.
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        conn.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus.Peer"),
            "Ping",
            &(),
        )?;
    }
}

#[cfg(test)]
mod integration_metadata_tests {
    const PORTAL_FILE: &str =
        include_str!("../../../contrib/xdg-desktop-portal/portals/aegis.portal");
    const PORTALS_CONF: &str =
        include_str!("../../../contrib/xdg-desktop-portal/aegis-portals.conf");
    const DBUS_SERVICE: &str = include_str!(
        "../../../contrib/dbus-1/services/org.freedesktop.impl.portal.desktop.aegis.service.in"
    );

    #[test]
    fn capability_file_advertises_exactly_the_served_interfaces() {
        let interfaces = PORTAL_FILE
            .lines()
            .find_map(|line| line.strip_prefix("Interfaces="))
            .expect("portal metadata must declare Interfaces");
        let advertised: Vec<_> = interfaces
            .split(';')
            .filter(|entry| !entry.is_empty())
            .collect();
        assert_eq!(
            advertised,
            [
                "org.freedesktop.impl.portal.Settings",
                "org.freedesktop.impl.portal.Screenshot",
                "org.freedesktop.impl.portal.ScreenCast",
                "org.freedesktop.impl.portal.Secret",
                "org.freedesktop.impl.portal.Lockdown",
                "org.freedesktop.impl.portal.FileChooser",
                "org.freedesktop.impl.portal.Email",
                "org.freedesktop.impl.portal.Account",
            ]
        );
    }

    #[test]
    fn aegis_is_the_default_with_gtk_as_the_safety_net() {
        assert!(
            PORTALS_CONF.lines().any(|line| line == "default=aegis;gtk"),
            "the routing default is Aegis with the GTK safety net"
        );
        for interface in [
            "Settings",
            "Screenshot",
            "ScreenCast",
            "Secret",
            "Lockdown",
            "FileChooser",
            "Email",
            "Account",
        ] {
            let route = format!("org.freedesktop.impl.portal.{interface}=aegis");
            assert!(
                PORTALS_CONF.lines().any(|line| line.starts_with(&route)),
                "missing explicit Aegis route for {interface}"
            );
        }
        assert!(!PORTALS_CONF.contains("Background=aegis"));
        for interface in [
            "Inhibit",
            "AppChooser",
            "Notification",
            "DynamicLauncher",
            "Wallpaper",
        ] {
            assert!(
                PORTALS_CONF
                    .lines()
                    .any(|line| { line == format!("org.freedesktop.impl.portal.{interface}=gtk") }),
                "{interface} must be explicitly delegated to the complete GTK backend"
            );
        }
    }

    #[test]
    fn activation_uses_the_private_packaged_executable() {
        assert!(
            DBUS_SERVICE
                .lines()
                .any(|line| line == "Exec=@libexecdir@/xdg-desktop-portal-aegis")
        );
    }
}
