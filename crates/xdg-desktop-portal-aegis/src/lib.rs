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
//! `org.freedesktop.impl.portal.Account`,
//! `org.freedesktop.impl.portal.Access` v1,
//! and `org.freedesktop.impl.portal.AppChooser` v4,
//! and `org.freedesktop.impl.portal.OpenURI` v3,
//! and `org.freedesktop.impl.portal.Background` v1,
//! and `org.freedesktop.impl.portal.DynamicLauncher` v1,
//! and `org.freedesktop.impl.portal.Inhibit` v3,
//! and `org.freedesktop.impl.portal.Notification` v2,
//! and `org.freedesktop.impl.portal.Wallpaper` v1,
//! and `org.freedesktop.impl.portal.Print` at
//! `/org/freedesktop/portal/desktop` under the well-known name
//! `org.freedesktop.impl.portal.desktop.aegis`. Secret is backed by an
//! encrypted at-rest vault. FileChooser launches one portal-owned optics
//! (iris/lens) prompter process; no file data crosses compositor IPC. For
//! compositor-owned resources the backend is an ordinary scoped IPC client:
//! pixels come from `CaptureOutput` under the built-in
//! `aegis-portal` named scope with a sealed-memfd blob transfer
//! transport, screencast frames arrive through the same scope's output-frame
//! stream and are republished as a PipeWire producer stream. Account consent,
//! the file chooser, password-mode vault unlock, and the app chooser
//! (backed by the backend's own freedesktop desktop-file/mimeapps
//! resolution, which also resolves and launches OpenURI targets) are
//! Portal-owned UI and do not cross compositor IPC.
//! No Wayland capture protocol is added anywhere.
//!
//! The process uses zbus's blocking API on the session bus and plain
//! `std::thread` workers without an async runtime. Method dispatch runs on
//! zbus's internal
//! executor; the compositor IPC round-trip (which blocks for up to one frame)
//! happens on a dedicated capture worker so a slow capture never stalls the
//! bus, and every screencast runs its own PipeWire main loop on a dedicated
//! cast thread.

mod access;
mod account;
mod app_chooser;
mod apps;
mod background;
mod cast;
mod dynamic_launcher;
mod email;
mod file_chooser;
mod files;
mod inhibit;
mod ipc;
mod lockdown;
mod notification;
mod open_uri;
mod print;
mod prompter;
mod screencast;
mod screenshot;
mod session;
mod settings;
mod wallpaper;

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
    let (access_jobs, access_rx) = mpsc::sync_channel::<access::AccessJob>(MAX_QUEUED_REQUESTS);
    let (app_chooser_jobs, app_chooser_rx) =
        mpsc::sync_channel::<app_chooser::AppChooserJob>(MAX_QUEUED_REQUESTS);
    let (open_uri_jobs, open_uri_rx) =
        mpsc::sync_channel::<open_uri::OpenUriJob>(MAX_QUEUED_REQUESTS);
    let (background_jobs, background_rx) =
        mpsc::sync_channel::<background::BackgroundJob>(MAX_QUEUED_REQUESTS);
    let (dynamic_launcher_jobs, dynamic_launcher_rx) =
        mpsc::sync_channel::<dynamic_launcher::DynamicLauncherJob>(MAX_QUEUED_REQUESTS);
    let (wallpaper_jobs, wallpaper_rx) =
        mpsc::sync_channel::<wallpaper::WallpaperJob>(MAX_QUEUED_REQUESTS);
    let (print_jobs, print_rx) = mpsc::sync_channel::<print::PrintJob>(MAX_QUEUED_REQUESTS);
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
    conn.object_server().at(
        DESKTOP_PATH,
        access::AccessIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: access_jobs.clone(),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        app_chooser::AppChooserIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: app_chooser_jobs.clone(),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        open_uri::OpenUriIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: open_uri_jobs.clone(),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        background::BackgroundIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: background_jobs.clone(),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        dynamic_launcher::DynamicLauncherIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: dynamic_launcher_jobs.clone(),
        },
    )?;
    // Inhibit calls logind in-method (one fast system-bus round trip); no
    // worker, no compositor IPC.
    conn.object_server().at(
        DESKTOP_PATH,
        inhibit::InhibitIface::new(conn.inner().clone(), Arc::clone(&tracker)),
    )?;
    // Notification supervises the daemon-mode prompter itself (lazily
    // spawned on the first AddNotification); no worker, no Request objects.
    conn.object_server().at(
        DESKTOP_PATH,
        notification::NotificationIface::new(conn.clone()),
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        wallpaper::WallpaperIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: wallpaper_jobs.clone(),
        },
    )?;
    conn.object_server().at(
        DESKTOP_PATH,
        print::PrintIface {
            conn: conn.inner().clone(),
            tracker: Arc::clone(&tracker),
            jobs: print_jobs.clone(),
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

    let access_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-access".to_string())
        .spawn(move || access::access_worker(access_rx, access_tracker))
        .map_err(|e| PortalError::Bus(zbus::Error::Failure(format!("spawn access worker: {e}"))))?;

    // AppChooser uses the same per-request supervised prompter pattern as
    // Access; candidate resolution happens in the served method.
    let app_chooser_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-app-chooser".to_string())
        .spawn(move || app_chooser::app_chooser_worker(app_chooser_rx, app_chooser_tracker))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!(
                "spawn app chooser worker: {e}"
            )))
        })?;

    // OpenURI launches resolved applications itself and reuses the
    // AppChooser dialog when the user must pick one.
    let open_uri_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-open-uri".to_string())
        .spawn(move || open_uri::open_uri_worker(open_uri_rx, open_uri_tracker))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!("spawn open uri worker: {e}")))
        })?;

    // Background consent uses the same supervised confirmation prompt as
    // Access; the autostart entry write happens on the worker task.
    let background_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-background".to_string())
        .spawn(move || background::background_worker(background_rx, background_tracker))
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!(
                "spawn background worker: {e}"
            )))
        })?;

    // DynamicLauncher's backend surface is the install-confirmation dialog
    // only; the frontend performs the actual installation.
    let dynamic_launcher_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-dynamic-launcher".to_string())
        .spawn(move || {
            dynamic_launcher::dynamic_launcher_worker(dynamic_launcher_rx, dynamic_launcher_tracker)
        })
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!(
                "spawn dynamic launcher worker: {e}"
            )))
        })?;

    // Wallpaper crosses the compositor IPC (protocol 26) and does slow
    // image I/O, so it gets the standard per-request task worker.
    let wallpaper_tracker = Arc::clone(&tracker);
    let wallpaper_socket = socket.clone();
    std::thread::Builder::new()
        .name("aegis-portal-wallpaper".to_string())
        .spawn(move || {
            wallpaper::wallpaper_worker(wallpaper_rx, wallpaper_tracker, wallpaper_socket)
        })
        .map_err(|e| {
            PortalError::Bus(zbus::Error::Failure(format!("spawn wallpaper worker: {e}")))
        })?;

    // Print spools the document and hands it to the system `lp` client
    // (the email/xdg-email hand-off precedent).
    let print_tracker = Arc::clone(&tracker);
    std::thread::Builder::new()
        .name("aegis-portal-print".to_string())
        .spawn(move || print::print_worker(print_rx, print_tracker))
        .map_err(|e| PortalError::Bus(zbus::Error::Failure(format!("spawn print worker: {e}"))))?;

    settings::spawn_watcher(conn.clone(), socket, settings_store).map_err(PortalError::Worker)?;

    conn.request_name(BUS_NAME)?;

    log::info!(
        "portal: serving Settings+Screenshot+ScreenCast+Secret+Lockdown+FileChooser+Email+Account+Access+AppChooser+OpenURI+Background+DynamicLauncher+Inhibit+Notification+Wallpaper+Print as {BUS_NAME}"
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
                "org.freedesktop.impl.portal.Access",
                "org.freedesktop.impl.portal.AppChooser",
                "org.freedesktop.impl.portal.OpenURI",
                "org.freedesktop.impl.portal.Background",
                "org.freedesktop.impl.portal.DynamicLauncher",
                "org.freedesktop.impl.portal.Inhibit",
                "org.freedesktop.impl.portal.Notification",
                "org.freedesktop.impl.portal.Wallpaper",
                "org.freedesktop.impl.portal.Print",
            ]
        );
    }

    #[test]
    fn aegis_is_the_sole_backend_without_a_gtk_fallback() {
        assert!(
            PORTALS_CONF.lines().any(|line| line == "default=aegis"),
            "the routing default is Aegis alone"
        );
        assert!(
            !PORTALS_CONF.to_lowercase().contains("gtk"),
            "no GTK fallback route or comment remains in the routing config"
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
            "Access",
            "AppChooser",
            "OpenURI",
            "Background",
            "DynamicLauncher",
            "Inhibit",
            "Notification",
            "Wallpaper",
            "Print",
        ] {
            let route = format!("org.freedesktop.impl.portal.{interface}=aegis");
            assert!(
                PORTALS_CONF.lines().any(|line| line.starts_with(&route)),
                "missing explicit Aegis route for {interface}"
            );
        }
        // Every interface routes to Aegis alone; no fallback backend exists.
        for line in PORTALS_CONF.lines() {
            if line.starts_with("org.freedesktop.") {
                assert!(
                    line.ends_with("=aegis"),
                    "unexpected non-Aegis route: {line}"
                );
            }
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
