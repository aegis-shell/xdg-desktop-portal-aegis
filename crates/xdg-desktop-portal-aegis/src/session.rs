//! `org.freedesktop.impl.portal.Session` objects and the cast-session
//! registry behind the ScreenCast portal.
//!
//! One Session object per `CreateSession` call, registered at the exact
//! `session_handle` supplied by the portal frontend. `Close` hands the path to the
//! screencast worker, which stops the cast (if any), emits `Closed`, and
//! removes the object. A cast that ends from the compositor side
//! (`Event::StreamEnded`, disconnect) takes the same teardown path.

use std::collections::HashMap;
use std::os::unix::net::UnixStream;

const MAX_SESSIONS: usize = 128;
const MAX_LIVE_CASTS: usize = 16;

/// The source a session is armed with. Window capture is intentionally not
/// exposed until the compositor can render a toplevel independently of
/// overlapping windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastSource {
    Monitor,
}

/// State of one portal screencast session.
pub(crate) struct CastSession {
    /// Application identity supplied by the frontend at `CreateSession`.
    pub(crate) app_id: String,
    pub(crate) sources_selected: bool,
    /// The armed source; meaningful once `sources_selected` holds.
    pub(crate) source: CastSource,
    /// Persistence requested by the frontend. The current backend safely
    /// reduces nonzero modes to 0 and reports that reduction from Start.
    pub(crate) requested_persist_mode: u32,
    /// Reserved before spawning PipeWire negotiation so concurrent Start
    /// calls cannot create two producers for one session.
    pub(crate) starting: bool,
    /// Closing this end makes the cast thread's stop socket readable, which
    /// quits its PipeWire main loop. `None` until `Start` succeeds.
    pub(crate) stop: Option<UnixStream>,
    pub(crate) cast_thread: Option<std::thread::JoinHandle<()>>,
}

/// Live sessions keyed by object path.
#[derive(Default)]
pub(crate) struct SessionRegistry {
    sessions: HashMap<String, CastSession>,
}

impl SessionRegistry {
    /// Register a fresh session. Duplicate paths are refused so a hostile or
    /// buggy client cannot shadow another application's session.
    pub(crate) fn insert(&mut self, path: &str, app_id: &str) -> Result<(), String> {
        if self.sessions.contains_key(path) {
            return Err(format!("session {path} already exists"));
        }
        if self.sessions.len() >= MAX_SESSIONS {
            return Err(format!("session limit of {MAX_SESSIONS} reached"));
        }
        self.sessions.insert(
            path.to_string(),
            CastSession {
                app_id: app_id.to_string(),
                sources_selected: false,
                source: CastSource::Monitor,
                requested_persist_mode: 0,
                starting: false,
                stop: None,
                cast_thread: None,
            },
        );
        Ok(())
    }

    pub(crate) fn contains(&self, path: &str) -> bool {
        self.sessions.contains_key(path)
    }

    pub(crate) fn mark_sources_selected(
        &mut self,
        path: &str,
        app_id: &str,
        source: CastSource,
        requested_persist_mode: u32,
    ) -> Result<(), String> {
        let session = self
            .sessions
            .get_mut(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        if session.sources_selected {
            return Err(format!("session {path} already selected sources"));
        }
        if session.app_id != app_id {
            return Err(format!("session {path} belongs to another application"));
        }
        session.sources_selected = true;
        session.source = source;
        session.requested_persist_mode = requested_persist_mode;
        Ok(())
    }

    /// Validate the application owner and return the source and persistence
    /// request `Start` must apply.
    pub(crate) fn reserve_start(
        &mut self,
        path: &str,
        app_id: &str,
    ) -> Result<(CastSource, u32), String> {
        let session = self
            .sessions
            .get(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        if session.app_id != app_id {
            return Err(format!("session {path} belongs to another application"));
        }
        if !session.sources_selected {
            return Err(format!("session {path} has not selected sources"));
        }
        if session.starting || session.cast_thread.is_some() {
            return Err(format!("session {path} already started"));
        }
        let live = self
            .sessions
            .values()
            .filter(|session| session.starting || session.cast_thread.is_some())
            .count();
        if live >= MAX_LIVE_CASTS {
            return Err(format!("live cast limit of {MAX_LIVE_CASTS} reached"));
        }
        let session = self
            .sessions
            .get_mut(path)
            .ok_or_else(|| format!("unknown session {path}"))?;
        session.starting = true;
        Ok((session.source, session.requested_persist_mode))
    }

    pub(crate) fn clear_start(&mut self, path: &str) {
        if let Some(session) = self.sessions.get_mut(path) {
            session.starting = false;
        }
    }

    /// Install a negotiated cast only if the reserved session still exists.
    /// On a raced Session.Close, return ownership so the caller can stop and
    /// join the orphan producer before replying.
    pub(crate) fn mark_started(
        &mut self,
        path: &str,
        stop: UnixStream,
        cast_thread: std::thread::JoinHandle<()>,
    ) -> Result<(), (UnixStream, std::thread::JoinHandle<()>)> {
        if let Some(session) = self.sessions.get_mut(path) {
            session.starting = false;
            session.stop = Some(stop);
            session.cast_thread = Some(cast_thread);
            Ok(())
        } else {
            Err((stop, cast_thread))
        }
    }

    /// Detach a session from the registry. The caller stops/joins its cast
    /// after dropping the registry lock.
    pub(crate) fn remove(&mut self, path: &str) -> Option<CastSession> {
        self.sessions.remove(path)
    }
}

/// Stop and join a detached cast without holding the session-registry lock.
pub(crate) fn stop_cast(mut session: CastSession) {
    drop(session.stop.take());
    if let Some(thread) = session.cast_thread.take() {
        let _ = thread.join();
    }
}

/// The served session object. The portal spec gives it `Close` and the
/// `Closed` signal; the worker emits the signal after teardown so the
/// frontend observes a fully stopped session.
pub(crate) struct SessionIface {
    pub(crate) path: String,
    pub(crate) jobs: std::sync::mpsc::SyncSender<crate::screencast::CastJob>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl SessionIface {
    async fn close(&self) -> zbus::fdo::Result<()> {
        log::info!("portal: session {} closed by client", self.path);
        self.jobs
            .send(crate::screencast::CastJob::CloseSession {
                session_path: self.path.clone(),
            })
            .map_err(|_| zbus::fdo::Error::Failed("screencast worker is gone".to_string()))
    }

    #[zbus(property)]
    fn version(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(path: &str) -> SessionRegistry {
        let mut registry = SessionRegistry::default();
        registry.insert(path, "org.example.App").unwrap();
        registry
    }

    #[test]
    fn duplicate_session_paths_are_refused() {
        let mut registry = registry_with("/s/1");
        assert!(registry.contains("/s/1"));
        assert!(registry.insert("/s/1", "org.example.App").is_err());
        assert!(registry.insert("/s/2", "org.example.App").is_ok());
    }

    #[test]
    fn total_session_count_is_bounded() {
        let mut registry = SessionRegistry::default();
        for index in 0..MAX_SESSIONS {
            registry
                .insert(&format!("/s/{index}"), "org.example.App")
                .unwrap();
        }
        assert!(registry.insert("/s/overflow", "org.example.App").is_err());
    }

    #[test]
    fn live_cast_count_is_bounded() {
        let mut registry = SessionRegistry::default();
        for index in 0..=MAX_LIVE_CASTS {
            let path = format!("/s/{index}");
            registry.insert(&path, "org.example.App").unwrap();
            registry
                .mark_sources_selected(&path, "org.example.App", CastSource::Monitor, 0)
                .unwrap();
        }
        for index in 0..MAX_LIVE_CASTS {
            registry
                .reserve_start(&format!("/s/{index}"), "org.example.App")
                .unwrap();
        }
        assert!(
            registry
                .reserve_start(&format!("/s/{MAX_LIVE_CASTS}"), "org.example.App")
                .is_err()
        );
    }

    #[test]
    fn host_applications_with_an_empty_frontend_app_id_are_supported() {
        let mut registry = SessionRegistry::default();
        registry.insert("/s/host", "").unwrap();
        registry
            .mark_sources_selected("/s/host", "", CastSource::Monitor, 0)
            .unwrap();
        assert_eq!(
            registry.reserve_start("/s/host", ""),
            Ok((CastSource::Monitor, 0))
        );
    }

    #[test]
    fn start_requires_selected_sources_and_single_use() {
        let mut registry = registry_with("/s/1");
        assert!(registry.reserve_start("/s/1", "org.example.App").is_err());
        registry
            .mark_sources_selected("/s/1", "org.example.App", CastSource::Monitor, 0)
            .unwrap();
        assert_eq!(
            registry.reserve_start("/s/1", "org.example.App"),
            Ok((CastSource::Monitor, 0))
        );
        registry.clear_start("/s/1");
        assert!(registry.reserve_start("/s/1", "org.example.Other").is_err());
        assert!(
            registry
                .mark_sources_selected("/s/1", "org.example.App", CastSource::Monitor, 0)
                .is_err()
        );

        let (stop, _read) = UnixStream::pair().unwrap();
        let thread = std::thread::spawn(|| {});
        registry.reserve_start("/s/1", "org.example.App").unwrap();
        registry.mark_started("/s/1", stop, thread).unwrap();
        assert!(registry.reserve_start("/s/1", "org.example.App").is_err());
    }

    #[test]
    fn remove_stops_and_joins_the_cast() {
        let mut registry = registry_with("/s/1");
        registry
            .mark_sources_selected("/s/1", "org.example.App", CastSource::Monitor, 0)
            .unwrap();
        let (stop, read) = UnixStream::pair().unwrap();
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let thread_flag = std::sync::Arc::clone(&flag);
        let thread = std::thread::spawn(move || {
            let mut byte = [0u8; 1];
            use std::io::Read;
            let _ = (&read).read(&mut byte);
            thread_flag.store(true, std::sync::atomic::Ordering::Release);
        });
        registry.reserve_start("/s/1", "org.example.App").unwrap();
        registry.mark_started("/s/1", stop, thread).unwrap();
        let session = registry.remove("/s/1").expect("session was live");
        stop_cast(session);
        assert!(flag.load(std::sync::atomic::Ordering::Acquire));
        assert!(!registry.contains("/s/1"));
    }
}
