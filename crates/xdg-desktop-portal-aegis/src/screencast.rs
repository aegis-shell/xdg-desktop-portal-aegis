//! `org.freedesktop.impl.portal.ScreenCast` v6.
//!
//! The portal frontend supplies Request and Session object paths to the
//! backend. `CreateSession` exports the Session object, `SelectSources`
//! requires a user-confirmed compositor picker, and `Start` spawns the cast
//! thread (compositor frame stream → PipeWire producer). Blocking
//! picker and PipeWire work stays on the screencast worker; D-Bus methods
//! asynchronously await the backend `(response, results)` tuple.
//!
//! This backend advertises monitor sources only, one stream per session, and
//! cursor mode Hidden. Client source-type masks that offer window alongside
//! monitor (OBS's unified screen capture sends both bits) are accepted and
//! served as monitor, per the `types`-as-acceptable-set contract. Selection
//! always requires an explicit compositor
//! confirmation identifying the requesting application. Aegis IPC's legacy window stream is deliberately not
//! reachable here because it crops the composed output and can therefore
//! contain pixels from an occluding window. Persistence requests are accepted
//! but conservatively reduced to `persist_mode = 0`; restore data is treated
//! as unavailable and therefore causes a normal fresh confirmation, as the
//! version-4 contract permits. Version 5's `mapping_id` stream property is
//! optional and omitted because no RemoteDesktop coordinate mapping exists.
//! Version 6's stable PipeWire `object.serial` is resolved from the registry
//! and returned as `pipewire-serial`; Start fails rather than claim v6 without
//! that stable identifier.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use zbus::zvariant::{Array, Dict, ObjectPath, Structure, StructureBuilder, Value};

use crate::cast;
use crate::session::{CastSource, SessionIface, SessionRegistry};
use aegis_portal_runtime::{PortalResponse, RequestTracker, ResponseSender};

const SESSION_IFACE: &str = "org.freedesktop.impl.portal.Session";

/// Version 6 adds the stable `pipewire-serial` stream property.
pub(crate) const SCREENCAST_VERSION: u32 = 6;
/// `AvailableSourceTypes` bit: monitor.
const SOURCE_TYPE_MONITOR: u32 = 1;
/// `AvailableCursorModes`: Hidden only. No cursor metadata is produced.
const CURSOR_MODES: u32 = 1;
/// Waiting for the PipeWire negotiation longer than this is a failure.
const START_TIMEOUT: Duration = Duration::from_secs(10);
/// One job handed from the bus methods to the screencast worker.
pub(crate) enum CastJob {
    SelectSources {
        request_path: String,
        session_path: String,
        app_id: String,
        source_types: u32,
        cursor_mode: u32,
        persist_mode: u32,
        reply: ResponseSender,
    },
    Start {
        request_path: String,
        session_path: String,
        app_id: String,
        reply: ResponseSender,
    },
    /// The client called `Session.Close`.
    CloseSession { session_path: String },
    /// The compositor ended the stream (scope revoked, lease lapsed,
    /// disconnect); reported by the cast thread.
    SessionEnded { session_path: String },
}

/// Options parsed out of the `SelectSources` argument.
pub(crate) struct SelectOptions {
    /// Requested source-type mask; must intersect what we offer.
    pub(crate) source_types: u32,
    pub(crate) cursor_mode: u32,
    pub(crate) persist_mode: u32,
}

/// Parse `SelectSources` options. Unknown keys are ignored per spec.
pub(crate) fn parse_select_options(options: &HashMap<String, Value<'_>>) -> SelectOptions {
    let source_types = options
        .get("types")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(SOURCE_TYPE_MONITOR);
    let cursor_mode = options
        .get("cursor_mode")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(1);
    let persist_mode = options
        .get("persist_mode")
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0);
    SelectOptions {
        source_types,
        cursor_mode,
        persist_mode,
    }
}

/// Why a `SelectSources` option set cannot be served, as a D-Bus message.
/// `types` is the set the client accepts; the backend may serve any subset,
/// so the mask only needs to intersect what we offer (monitor). OBS's unified
/// screen-capture source always offers monitor|window and breaks on a strict
/// equality check.
fn validate_select(options: &SelectOptions) -> Result<(), String> {
    if options.source_types & SOURCE_TYPE_MONITOR == 0 {
        return Err("only monitor sources are supported".to_string());
    }
    if options.cursor_mode != CURSOR_MODES {
        return Err(format!(
            "cursor_mode {} is not supported (Hidden only)",
            options.cursor_mode
        ));
    }
    if options.persist_mode > 2 {
        return Err(format!(
            "persist_mode {} is not defined by the ScreenCast contract",
            options.persist_mode
        ));
    }
    Ok(())
}

/// The served ScreenCast interface. Methods register request/session
/// objects and enqueue; the worker does everything blocking.
pub(crate) struct ScreenCastIface {
    pub(crate) conn: zbus::Connection,
    pub(crate) tracker: Arc<Mutex<RequestTracker>>,
    pub(crate) sessions: Arc<Mutex<SessionRegistry>>,
    pub(crate) jobs: mpsc::SyncSender<CastJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCastIface {
    async fn create_session(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let request_path = handle.as_str().to_string();
        let session_path = session_handle.as_str().to_string();
        aegis_portal_runtime::register(&self.conn, &self.tracker, &request_path).await?;

        if self.tracker.lock().unwrap().was_closed(&request_path) {
            aegis_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((1, HashMap::new()));
        }
        let insert_result = { self.sessions.lock().unwrap().insert(&session_path, app_id) };
        if let Err(error) = insert_result {
            log::warn!("portal: CreateSession refused: {error}");
            aegis_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((2, HashMap::new()));
        }
        let inserted = self
            .conn
            .object_server()
            .at(
                session_path.as_str(),
                SessionIface {
                    path: session_path.clone(),
                    jobs: self.jobs.clone(),
                },
            )
            .await;
        let inserted = match inserted {
            Ok(inserted) => inserted,
            Err(error) => {
                let _ = self.sessions.lock().unwrap().remove(&session_path);
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
                return Err(zbus::fdo::Error::from(error));
            }
        };
        if !inserted {
            let _ = self.sessions.lock().unwrap().remove(&session_path);
            aegis_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
            return Ok((2, HashMap::new()));
        }
        aegis_portal_runtime::finish(&self.conn, &self.tracker, &request_path).await;
        log::info!("portal: screencast session {session_path} created for '{app_id}'");
        Ok((0, HashMap::new()))
    }

    async fn select_sources(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let session_path = session_handle.as_str().to_string();
        if !self.sessions.lock().unwrap().contains(&session_path) {
            return Err(zbus::fdo::Error::Failed(format!(
                "unknown session {session_path}"
            )));
        }
        let options = parse_select_options(&options);
        validate_select(&options).map_err(zbus::fdo::Error::InvalidArgs)?;

        let path = handle.as_str().to_string();
        log::debug!("portal: SelectSources for '{app_id}' on {session_path} at {path}");

        aegis_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        let queued = self.enqueue(CastJob::SelectSources {
            request_path: path.clone(),
            session_path,
            app_id: app_id.to_string(),
            source_types: options.source_types,
            cursor_mode: options.cursor_mode,
            persist_mode: options.persist_mode,
            reply,
        });
        match queued {
            Ok(true) => {}
            Ok(false) => {
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Ok((2, HashMap::new()));
            }
            Err(error) => {
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Err(error);
            }
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("screencast worker dropped its response".to_string())
        });
        aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    async fn start(
        &self,
        handle: ObjectPath<'_>,
        session_handle: ObjectPath<'_>,
        app_id: &str,
        _parent_window: &str,
        _options: HashMap<String, Value<'_>>,
    ) -> zbus::fdo::Result<PortalResponse> {
        let session_path = session_handle.as_str().to_string();
        if !self.sessions.lock().unwrap().contains(&session_path) {
            return Err(zbus::fdo::Error::Failed(format!(
                "unknown session {session_path}"
            )));
        }
        let path = handle.as_str().to_string();
        log::debug!("portal: Start for '{app_id}' on {session_path} at {path}");

        aegis_portal_runtime::register(&self.conn, &self.tracker, &path).await?;
        let (reply, response) = async_channel::bounded(1);
        let queued = self.enqueue(CastJob::Start {
            request_path: path.clone(),
            session_path,
            app_id: app_id.to_string(),
            reply,
        });
        match queued {
            Ok(true) => {}
            Ok(false) => {
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Ok((2, HashMap::new()));
            }
            Err(error) => {
                aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
                return Err(error);
            }
        }
        let result = response.recv().await.map_err(|_| {
            zbus::fdo::Error::Failed("screencast worker dropped its response".to_string())
        });
        aegis_portal_runtime::finish(&self.conn, &self.tracker, &path).await;
        result
    }

    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        SOURCE_TYPE_MONITOR
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        CURSOR_MODES
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        SCREENCAST_VERSION
    }
}

impl ScreenCastIface {
    /// `Ok(false)` is bounded backpressure, reported as portal response 2;
    /// disconnection remains a D-Bus service failure.
    fn enqueue(&self, job: CastJob) -> zbus::fdo::Result<bool> {
        match self.jobs.try_send(job) {
            Ok(()) => Ok(true),
            Err(mpsc::TrySendError::Full(_)) => {
                log::warn!("portal: refusing ScreenCast request: worker queue is full");
                Ok(false)
            }
            Err(mpsc::TrySendError::Disconnected(_)) => Err(zbus::fdo::Error::Failed(
                "screencast worker is gone".to_string(),
            )),
        }
    }
}

/// Dispatch blocking selections and PipeWire negotiations independently.
/// Session close/end events stay on this dispatcher and therefore remain
/// responsive even while another application has a confirmation open.
pub(crate) fn cast_worker(
    rx: mpsc::Receiver<CastJob>,
    jobs: mpsc::SyncSender<CastJob>,
    conn: zbus::blocking::Connection,
    tracker: Arc<Mutex<RequestTracker>>,
    sessions: Arc<Mutex<SessionRegistry>>,
    socket: PathBuf,
) {
    const MAX_ACTIVE_CAST_REQUESTS: usize = 32;
    struct ActiveGuard(Arc<std::sync::atomic::AtomicUsize>);
    impl Drop for ActiveGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        }
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    while let Ok(job) = rx.recv() {
        match job {
            CastJob::SelectSources {
                request_path,
                session_path,
                app_id,
                source_types,
                cursor_mode,
                persist_mode,
                reply,
            } => {
                if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    >= MAX_ACTIVE_CAST_REQUESTS
                {
                    active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    let _ = reply.send_blocking((2, HashMap::new()));
                    continue;
                }
                let task_tracker = Arc::clone(&tracker);
                let task_sessions = Arc::clone(&sessions);
                let task_socket = socket.clone();
                let active_guard = ActiveGuard(Arc::clone(&active));
                let spawn_failure_reply = reply.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("aegis-portal-select-sources".to_owned())
                    .spawn(move || {
                        let _active = active_guard;
                        let mut picker = crate::ipc::PortalCapture::new(task_socket);
                        let code = select_sources(
                            &task_tracker,
                            &task_sessions,
                            &mut picker,
                            &request_path,
                            &session_path,
                            &app_id,
                            source_types,
                            cursor_mode,
                            persist_mode,
                        );
                        log::debug!("portal: SelectSources for '{app_id}' → response {code}");
                        let _ = reply.send_blocking((code, HashMap::new()));
                    })
                {
                    log::error!("portal: could not spawn SelectSources task: {error}");
                    let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
                }
            }
            CastJob::Start {
                request_path,
                session_path,
                app_id,
                reply,
            } => {
                if active.fetch_add(1, std::sync::atomic::Ordering::AcqRel)
                    >= MAX_ACTIVE_CAST_REQUESTS
                {
                    active.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
                    let _ = reply.send_blocking((2, HashMap::new()));
                    continue;
                }
                let task_tracker = Arc::clone(&tracker);
                let task_sessions = Arc::clone(&sessions);
                let task_jobs = jobs.clone();
                let task_socket = socket.clone();
                let active_guard = ActiveGuard(Arc::clone(&active));
                let spawn_failure_reply = reply.clone();
                if let Err(error) = std::thread::Builder::new()
                    .name("aegis-portal-start-cast".to_owned())
                    .spawn(move || {
                        let _active = active_guard;
                        let response = start_cast(
                            &task_tracker,
                            &task_sessions,
                            &task_jobs,
                            &task_socket,
                            &request_path,
                            &session_path,
                            &app_id,
                        );
                        log::debug!("portal: Start for '{app_id}' → response {}", response.0);
                        let _ = reply.send_blocking(response);
                    })
                {
                    log::error!("portal: could not spawn Start task: {error}");
                    let _ = spawn_failure_reply.send_blocking((2, HashMap::new()));
                }
            }
            CastJob::CloseSession { session_path } | CastJob::SessionEnded { session_path } => {
                close_session(&conn, &sessions, &session_path);
            }
        }
    }
}

/// Arm the session. Response codes: 0 ok, 1 cancelled, 2 refused.
#[allow(clippy::too_many_arguments)]
fn select_sources(
    tracker: &Arc<Mutex<RequestTracker>>,
    sessions: &Arc<Mutex<SessionRegistry>>,
    picker: &mut crate::ipc::PortalCapture,
    request_path: &str,
    session_path: &str,
    app_id: &str,
    source_types: u32,
    _cursor_mode: u32,
    persist_mode: u32,
) -> u32 {
    if tracker.lock().unwrap().was_closed(request_path) {
        return 1;
    }
    let source = match pick_source(picker, source_types, app_id) {
        Ok(source) => source,
        Err(code) => return code,
    };
    if tracker.lock().unwrap().was_closed(request_path) {
        return 1;
    }
    match sessions
        .lock()
        .unwrap()
        .mark_sources_selected(session_path, app_id, source, persist_mode)
    {
        Ok(()) => 0,
        Err(error) => {
            log::warn!("portal: SelectSources refused: {error}");
            2
        }
    }
}

/// Decide the session's monitor source through an explicit user action.
fn pick_source(
    picker: &mut crate::ipc::PortalCapture,
    source_types: u32,
    app_id: &str,
) -> Result<CastSource, u32> {
    if source_types & SOURCE_TYPE_MONITOR == 0 {
        return Err(2);
    }
    match picker.pick_confirm(
        "Share Your Screen".to_string(),
        format!("Allow {app_id} to view the current monitor?"),
        Some("Share".to_string()),
    ) {
        Ok(aegis_portal_ipc::ConfirmPickResult::Confirmed) => Ok(CastSource::Monitor),
        Ok(aegis_portal_ipc::ConfirmPickResult::Cancelled) => Err(1),
        Err(error) => {
            log::warn!("portal: monitor sharing confirmation failed: {error}");
            Err(2)
        }
    }
}

/// Spawn the cast and report the negotiated stream. Response codes follow
/// the portal spec (0 ok, 1 cancelled, 2 error).
#[allow(clippy::too_many_arguments)]
fn start_cast(
    tracker: &Arc<Mutex<RequestTracker>>,
    sessions: &Arc<Mutex<SessionRegistry>>,
    jobs: &mpsc::SyncSender<CastJob>,
    socket: &std::path::Path,
    request_path: &str,
    session_path: &str,
    app_id: &str,
) -> (u32, HashMap<String, Value<'static>>) {
    if tracker.lock().unwrap().was_closed(request_path) {
        return (1, HashMap::new());
    }
    let (source, requested_persist_mode) = {
        let mut sessions = sessions.lock().unwrap();
        match sessions.reserve_start(session_path, app_id) {
            Ok(selection) => selection,
            Err(error) => {
                log::warn!("portal: Start refused: {error}");
                return (2, HashMap::new());
            }
        }
    };
    // The cast thread reports compositor-side stream ends back to this
    // worker through a clone of the worker's own job channel.
    let handle = match cast::spawn(socket.to_path_buf(), session_path.to_string(), jobs.clone()) {
        Ok(handle) => handle,
        Err(error) => {
            sessions.lock().unwrap().clear_start(session_path);
            log::warn!("portal: could not spawn cast for {session_path}: {error}");
            return (2, HashMap::new());
        }
    };
    match handle.started.recv_timeout(START_TIMEOUT) {
        Ok(Ok(started)) => {
            // A Close racing the negotiation wins over a started cast.
            if tracker.lock().unwrap().was_closed(request_path) {
                drop(handle.stop);
                let _ = handle.thread.join();
                sessions.lock().unwrap().clear_start(session_path);
                return (1, HashMap::new());
            }
            if let Err((stop, thread)) =
                sessions
                    .lock()
                    .unwrap()
                    .mark_started(session_path, handle.stop, handle.thread)
            {
                drop(stop);
                let _ = thread.join();
                return (1, HashMap::new());
            }
            log::info!(
                "portal: cast for {session_path} live as pipewire node {} serial {} ({}x{}, {:?})",
                started.node_id,
                started.serial,
                started.width,
                started.height,
                source
            );
            let mut results = HashMap::from([(
                "streams".to_string(),
                streams_value(
                    started.node_id,
                    started.serial,
                    SOURCE_TYPE_MONITOR,
                    (0, 0),
                    (started.width as i32, started.height as i32),
                ),
            )]);
            if requested_persist_mode != 0 {
                // Omitting this would make the frontend assume the requested
                // nonzero mode was granted. Report the safe reduction.
                results.insert("persist_mode".to_string(), Value::from(0_u32));
            }
            (0, results)
        }
        Ok(Err(error)) => {
            log::warn!("portal: cast for {session_path} failed: {error}");
            drop(handle.stop);
            let _ = handle.thread.join();
            sessions.lock().unwrap().clear_start(session_path);
            (2, HashMap::new())
        }
        Err(_) => {
            log::warn!("portal: cast for {session_path} timed out during negotiation");
            drop(handle.stop);
            let _ = handle.thread.join();
            sessions.lock().unwrap().clear_start(session_path);
            (2, HashMap::new())
        }
    }
}

/// Stop the cast (if any), emit `Closed`, and remove the session object.
fn close_session(
    conn: &zbus::blocking::Connection,
    sessions: &Arc<Mutex<SessionRegistry>>,
    session_path: &str,
) {
    let Some(session) = sessions.lock().unwrap().remove(session_path) else {
        return;
    };
    crate::session::stop_cast(session);
    log::debug!("portal: screencast session {session_path} closed");
    if let Err(error) = conn.emit_signal(None::<&str>, session_path, SESSION_IFACE, "Closed", &()) {
        log::warn!("portal: could not emit Closed for {session_path}: {error}");
    }
    if let Err(error) = conn.object_server().remove::<SessionIface, _>(session_path) {
        log::warn!("portal: could not remove {session_path}: {error}");
    }
}

/// The `streams` result: `a(ua{sv})` with the PipeWire node id, the
/// source's position and size in compositor coordinates, and its source
/// type (monitor or window).
fn streams_value(
    node_id: u32,
    pipewire_serial: u64,
    source_type: u32,
    position: (i32, i32),
    size: (i32, i32),
) -> Value<'static> {
    let properties: HashMap<String, Value> = HashMap::from([
        (
            "position".to_string(),
            Value::Structure(Structure::from(position)),
        ),
        ("size".to_string(), Value::Structure(Structure::from(size))),
        ("source_type".to_string(), Value::U32(source_type)),
        ("pipewire-serial".to_string(), Value::U64(pipewire_serial)),
    ]);
    // `append_field` keeps each field's dynamic signature, so the structure
    // types as `(ua{sv})`; `Structure::from` would route the fields through
    // `Value::new` and wrap them as variants (`(vv)`).
    let stream = StructureBuilder::new()
        .append_field(Value::U32(node_id))
        .append_field(Value::Dict(Dict::from(properties)))
        .build()
        .expect("non-empty structure");
    // The array must carry the element signature `(ua{sv})` — building it
    // from a `Vec<Value>` would type it as `av`, which the frontend cannot
    // deserialize as the spec's `a(ua{sv})`.
    let mut streams =
        Array::new(&zbus::zvariant::Signature::try_from("(ua{sv})").expect("valid signature"));
    streams
        .append(Value::Structure(stream))
        .expect("stream element matches");
    Value::Array(streams)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, Value<'static>)]) -> HashMap<String, Value<'static>> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn select_options_default_to_monitor_with_hidden_cursor() {
        let parsed = parse_select_options(&HashMap::new());
        assert_eq!(parsed.source_types, SOURCE_TYPE_MONITOR);
        assert_eq!(parsed.cursor_mode, 1);
        assert_eq!(parsed.persist_mode, 0);
        assert!(validate_select(&parsed).is_ok());
    }

    #[test]
    fn select_options_accept_monitor_and_window_mix() {
        // Clients such as OBS's unified screen capture offer every type they
        // can take; serving the monitor subset is the contract.
        let parsed = parse_select_options(&options(&[("types", Value::from(0b11u32))]));
        assert!(validate_select(&parsed).is_ok());
    }

    #[test]
    fn select_options_accept_only_hidden_cursor_mode() {
        for unsupported in [0u32, 2, 4, 5] {
            let parsed =
                parse_select_options(&options(&[("cursor_mode", Value::from(unsupported))]));
            assert!(validate_select(&parsed).is_err());
        }
    }

    #[test]
    fn select_options_refuse_window_only_sources() {
        let window_only = parse_select_options(&options(&[("types", Value::from(0b10u32))]));
        assert_eq!(window_only.source_types, 2);
        assert!(validate_select(&window_only).is_err());
    }

    #[test]
    fn select_options_refuse_unknown_source_bits() {
        let unknown = parse_select_options(&options(&[("types", Value::from(0b100u32))]));
        assert!(validate_select(&unknown).is_err());
    }

    #[test]
    fn screencast_version_is_6() {
        assert_eq!(SCREENCAST_VERSION, 6);
    }

    #[test]
    fn persist_modes_are_parsed_and_unknown_values_refused() {
        for mode in 0_u32..=2 {
            let parsed = parse_select_options(&options(&[("persist_mode", Value::from(mode))]));
            assert_eq!(parsed.persist_mode, mode);
            assert!(validate_select(&parsed).is_ok());
        }
        let parsed = parse_select_options(&options(&[("persist_mode", Value::from(3_u32))]));
        assert!(validate_select(&parsed).is_err());
    }

    #[test]
    fn select_options_ignore_wrong_types() {
        let parsed = parse_select_options(&options(&[("types", Value::from("monitor"))]));
        assert!(validate_select(&parsed).is_ok());
    }

    #[test]
    fn streams_value_has_portal_shape() {
        let value = streams_value(42, 9001, SOURCE_TYPE_MONITOR, (0, 0), (1920, 1080));
        let Value::Array(array) = &value else {
            panic!("streams must be an array");
        };
        assert_eq!(array.len(), 1);
        // Signature: a(ua{sv}) — the frontend's deserialize expects exactly
        // this shape, so pin it.
        assert_eq!(
            value.value_signature().to_string(),
            "a(ua{sv})",
            "streams signature"
        );
        let stream: Value = array.get(0).expect("read").expect("one stream");
        let Value::Structure(stream) = stream else {
            panic!("stream element must be a structure");
        };
        let Value::Dict(properties) = &stream.fields()[1] else {
            panic!("stream properties must be a dict");
        };
        assert!(properties.iter().any(|(key, value)| {
            matches!(key, Value::Str(key) if key.as_str() == "pipewire-serial")
                && matches!(value, Value::Value(value) if **value == Value::U64(9001))
        }));
    }
}
