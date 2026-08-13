//! `org.freedesktop.impl.portal.Wallpaper` v1: set the desktop wallpaper.
//!
//! Wallpaper application is compositor-owned (the compositor draws the
//! outputs), so this is the one Portal interface that crosses the scoped
//! IPC boundary: the image is handed over through the protocol-26
//! `SetWallpaper` op as a sealed memfd ([`aegis_portal_ipc`]). **Runtime
//! application requires a compositor that implements protocol 26**; against
//! an older one the op fails cleanly and the portal answers 2.
//!
//! Flow: the URI must be `file://` (a portal backend does not fetch from
//! the network — remote URIs answer 2); the image is read with a 64 MiB
//! cap on the worker. With `show-preview=true` the existing `Confirm`
//! prompt asks to set the named file as the wallpaper (accept "_Set
//! Wallpaper"; cancellation answers 1) — a true visual preview awaits
//! image decoding in the lens stack, documented limitation. With
//! `show-preview=false`
//! the spec allows direct application, so no prompt is shown. `set-on`
//! maps to the wire placement (`background`/`lockscreen`/`both`;
//! missing/empty means `background`, unknown values answer 2).

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};

use aegis_portal_ipc::{MAX_WALLPAPER_BYTES, WallpaperPlacement};
use aegis_portal_prompter::{ConfirmRequest, ConfirmResponse, PromptResult, PrompterRequest};
use zbus::zvariant::{ObjectPath, Value};

use crate::prompter::{self, InvokeError};
use crate::{files, ipc};
use aegis_portal_runtime::{RequestTracker, ResponseSender};

/// One wallpaper request handed from the bus method to the worker.
pub(crate) enum WallpaperJob {
    Set {
        request_path: String,
        app_id: String,
        path: PathBuf,
        placement: WallpaperPlacement,
        show_preview: bool,
        parent_window: Option<String>,
        reply: ResponseSender,
    },
}

/// The served wallpaper interface. The method only validates and enqueues;
/// the slow work (image read, consent prompt, IPC) runs on the worker.
pub(crate) struct WallpaperIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) jobs: mpsc::SyncSender<WallpaperJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Wallpaper")]
impl WallpaperIface {
    async fn set_wallpaper_uri(
        &self,
        handle: ObjectPath<'_>,
        app_id: &str,
        parent_window: &str,
        uri: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<u32> {
        let path = handle.as_str().to_string();
        log::info!("portal: SetWallpaperURI for '{app_id}' at {path}: {uri}");

        let (image_path, placement, show_preview) = match parse_request(uri, &options) {
            Ok(parsed) => parsed,
            Err(error) => {
                log::warn!("portal: refusing SetWallpaperURI: {error}");
                return Ok(2);
            }
        };

        aegis_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        let queued = self.jobs.try_send(WallpaperJob::Set {
            request_path: path.clone(),
            app_id: app_id.to_string(),
            path: image_path,
            placement,
            show_preview,
            parent_window: (!parent_window.is_empty()).then(|| parent_window.to_owned()),
            reply,
        });
        match queued {
            Ok(()) => {}
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!("portal: refusing SetWallpaperURI: worker queue is full");
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Ok(2);
            }
            Err(mpsc::TrySendError::Disconnected(_)) => {
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Err(zbus::fdo::Error::Failed(
                    "wallpaper worker is gone".to_string(),
                ));
            }
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("wallpaper worker dropped its response".to_string())
        });
        aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result.map(|(response, _)| response)
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Validate the URI and options: a local file, the placement, the preview
/// flag.
fn parse_request(
    uri: &str,
    options: &HashMap<String, Value<'_>>,
) -> Result<(PathBuf, WallpaperPlacement, bool), String> {
    if uri.len() > 8 * 1024 {
        return Err("URI is oversized".to_string());
    }
    if !uri.starts_with("file://") {
        return Err(format!(
            "only file:// URIs can be wallpapers; a portal backend does not fetch {uri:?} over the network"
        ));
    }
    let path = files::path_from_file_uri(uri)
        .ok_or_else(|| "file URI does not name an absolute local path".to_string())?;

    let placement = match options
        .get("set-on")
        .and_then(|value| String::try_from(value).ok())
        .as_deref()
    {
        None | Some("") => WallpaperPlacement::Background,
        Some("background") => WallpaperPlacement::Background,
        Some("lockscreen") => WallpaperPlacement::Lockscreen,
        Some("both") => WallpaperPlacement::Both,
        Some(other) => return Err(format!("unknown set-on value {other:?}")),
    };
    let show_preview = options
        .get("show-preview")
        .and_then(|value| bool::try_from(value).ok())
        .unwrap_or(false);
    Ok((path, placement, show_preview))
}

/// Read the image, bounded and non-empty.
fn read_image(path: &Path) -> Result<Vec<u8>, String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("could not open {}: {error}", path.display()))?;
    let mut bytes = Vec::new();
    std::io::Read::take(file, MAX_WALLPAPER_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read {}: {error}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("{} is empty", path.display()));
    }
    if bytes.len() as u64 > MAX_WALLPAPER_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_WALLPAPER_BYTES}-byte wallpaper limit",
            path.display()
        ));
    }
    Ok(bytes)
}

/// Dispatch wallpaper requests independently so one open preview cannot
/// head-of-line block another application's request.
pub(crate) fn wallpaper_worker(
    rx: mpsc::Receiver<WallpaperJob>,
    tracker: Arc<Mutex<RequestTracker>>,
    socket: PathBuf,
) {
    const MAX_ACTIVE_WALLPAPER_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(WallpaperJob::Set {
        request_path,
        app_id,
        path,
        placement,
        show_preview,
        parent_window,
        reply,
    }) = rx.recv()
    {
        if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel) >= MAX_ACTIVE_WALLPAPER_REQUESTS
        {
            active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
            log::warn!("portal: refusing SetWallpaperURI request: concurrency limit reached");
            let _ = reply.send_blocking((2, HashMap::new()));
            continue;
        }
        let task_tracker = Arc::clone(&tracker);
        let task_socket = socket.clone();
        let active_guard = ActiveGuard(Arc::clone(&active));
        let spawn_failure_reply = reply.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("aegis-portal-wallpaper-task".to_owned())
            .spawn(move || {
                let _active = active_guard;
                let result = run_set(
                    &task_tracker,
                    &request_path,
                    &app_id,
                    &task_socket,
                    &path,
                    placement,
                    show_preview,
                    parent_window,
                );
                let _ = reply.send_blocking(result);
            })
        {
            log::error!("portal: could not spawn wallpaper task: {error}");
            let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
        }
    }
}

/// Execute one request: read the image, maybe ask for consent, then apply.
#[allow(clippy::too_many_arguments)]
fn run_set(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    socket: &Path,
    image_path: &Path,
    placement: WallpaperPlacement,
    show_preview: bool,
    parent_window: Option<String>,
) -> (u32, HashMap<String, Value<'static>>) {
    if tracker.lock().unwrap().was_closed(request_path) {
        return (1, HashMap::new());
    }
    let image = match read_image(image_path) {
        Ok(image) => image,
        Err(error) => {
            log::warn!("portal: SetWallpaperURI for '{app_id}' failed: {error}");
            return (2, HashMap::new());
        }
    };

    if show_preview {
        match preview_consent(tracker, request_path, app_id, image_path, parent_window) {
            Ok(true) => {}
            Ok(false) => return (1, HashMap::new()),
            Err(error) => {
                log::warn!("portal: wallpaper preview for '{app_id}' failed: {error}");
                return (2, HashMap::new());
            }
        }
    }
    // Request.Close wins a race with a completed prompt.
    if tracker.lock().unwrap().was_closed(request_path) {
        return (1, HashMap::new());
    }

    match ipc::set_wallpaper(socket, &image, placement) {
        Ok(()) => {
            log::info!("portal: SetWallpaperURI for '{app_id}' applied ({placement:?})");
            (0, HashMap::new())
        }
        Err(error) => {
            log::warn!(
                "portal: SetWallpaperURI for '{app_id}' failed (the compositor must speak protocol 26): {error}"
            );
            (2, HashMap::new())
        }
    }
}

/// The preview consent prompt: a textual stand-in until the lens stack
/// decodes images (see the module docs). `Ok(true)` confirms, `Ok(false)`
/// cancels.
fn preview_consent(
    tracker: &Arc<Mutex<RequestTracker>>,
    request_path: &str,
    app_id: &str,
    image_path: &Path,
    parent_window: Option<String>,
) -> Result<bool, String> {
    let name = image_path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| image_path.display().to_string());
    let prompt = ConfirmRequest {
        title: "Set Wallpaper".to_owned(),
        body: format!("The application '{app_id}' wants to set '{name}' as the wallpaper."),
        accept_label: Some("_Set Wallpaper".to_owned()),
        deny_label: None,
        modal: true,
        parent_window,
    };
    let cancelled = || tracker.lock().unwrap().was_closed(request_path);
    match prompter::invoke(PrompterRequest::confirm(prompt), Some(&cancelled)) {
        Ok(PromptResult::Confirm(ConfirmResponse::Confirmed)) => Ok(true),
        Ok(PromptResult::Confirm(ConfirmResponse::Cancelled)) | Err(InvokeError::Cancelled) => {
            Ok(false)
        }
        Ok(_) => Err("wallpaper prompter returned the wrong response kind".to_owned()),
        Err(InvokeError::Failed(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect()
    }

    #[test]
    fn only_local_file_uris_are_accepted() {
        let (path, placement, preview) =
            parse_request("file:///tmp/wall%20paper.png", &HashMap::new()).unwrap();
        assert_eq!(path, PathBuf::from("/tmp/wall paper.png"));
        assert_eq!(placement, WallpaperPlacement::Background);
        assert!(!preview);

        assert!(parse_request("https://example.com/wall.png", &HashMap::new()).is_err());
        assert!(parse_request("file://remote/share/w.png", &HashMap::new()).is_err());
        assert!(parse_request("relative.png", &HashMap::new()).is_err());
    }

    #[test]
    fn set_on_maps_to_the_wire_placement() {
        for (value, expected) in [
            ("background", WallpaperPlacement::Background),
            ("lockscreen", WallpaperPlacement::Lockscreen),
            ("both", WallpaperPlacement::Both),
        ] {
            let options = options(&[("set-on", Value::from(value))]);
            let (_, placement, _) = parse_request("file:///tmp/w.png", &options).unwrap();
            assert_eq!(placement, expected);
        }
        let bad_value = options(&[("set-on", Value::from("screensaver"))]);
        assert!(parse_request("file:///tmp/w.png", &bad_value).is_err());

        let with_preview = options(&[("show-preview", Value::from(true))]);
        let (_, _, preview) = parse_request("file:///tmp/w.png", &with_preview).unwrap();
        assert!(preview);
    }

    #[test]
    fn image_reads_are_bounded() {
        let dir = std::env::temp_dir().join(format!(
            "aegis-wallpaper-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let image = dir.join("w.png");
        std::fs::write(&image, b"\x89PNG fake").unwrap();
        assert_eq!(read_image(&image).unwrap(), b"\x89PNG fake");

        let empty = dir.join("empty.png");
        std::fs::write(&empty, b"").unwrap();
        assert!(read_image(&empty).is_err());
        assert!(read_image(&dir.join("missing.png")).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ipc_apply_round_trips_against_the_test_server() {
        use aegis_portal_ipc::testing::{Handler, Server};
        struct Applying;
        impl Handler for Applying {
            fn set_wallpaper(
                &self,
                _connection: u64,
                placement: WallpaperPlacement,
                image: Vec<u8>,
            ) -> Result<(), String> {
                assert_eq!(placement, WallpaperPlacement::Lockscreen);
                assert_eq!(image, b"wallpaper-bytes");
                Ok(())
            }
        }
        let socket = std::env::temp_dir().join(format!(
            "aegis-wallpaper-ipc-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let server = Server::start(&socket, std::sync::Arc::new(Applying)).unwrap();
        ipc::set_wallpaper(
            server.path(),
            b"wallpaper-bytes",
            WallpaperPlacement::Lockscreen,
        )
        .unwrap();
    }

    #[test]
    fn ipc_apply_fails_against_a_pre_26_compositor() {
        use aegis_portal_ipc::testing::{Handler, Server};
        struct Legacy;
        impl Handler for Legacy {}
        let socket = std::env::temp_dir().join(format!(
            "aegis-wallpaper-legacy-{}-{}.sock",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        // A protocol-25-only compositor: the op is refused before any bytes
        // cross, and the reconnect retry negotiates down just the same.
        let server = Server::start_legacy(&socket, std::sync::Arc::new(Legacy), 25).unwrap();
        let error = ipc::set_wallpaper(
            server.path(),
            b"wallpaper-bytes",
            WallpaperPlacement::Background,
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    }
}
