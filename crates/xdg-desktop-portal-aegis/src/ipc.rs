//! The inward half of the bridge: the narrow, Portal-owned Aegis IPC client.
//!
//! The portal connects under the built-in owner-only `aegis-portal` scope
//! (`aegis_portal_ipc::LOCAL_PORTAL_SCOPE`) with `control` and a time-bounded
//! lease. This repository's wire projection admits only capture, stream, and
//! target-picking operations used by compositor-owned portal interfaces. The
//! wrapper keeps each connection alive across idle periods by renewing the
//! lease at half its TTL, and reconnects once on any failure so a compositor
//! restart or an expired lease self-heals on the next screenshot instead of
//! killing the D-Bus service.

use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aegis_portal_ipc::{Client, ConnectionCapabilities, LOCAL_PORTAL_SCOPE};

/// Lease TTL requested at handshake and renewal; matches the reference
/// client's default (`LeaseRequest::default`).
const LEASE_TTL_MS: u64 = 900_000;
/// Ordinary compositor RPCs must not retain a portal worker forever if a
/// local peer accepts the socket and then stops responding.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Interactive compositor chrome is itself bounded at five minutes. Leave
/// a small transport margin so its typed cancellation/error can arrive.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(305);

/// Open the one privileged runtime boundary used by capture and streaming.
/// Refuse a handshake that did not grant both scoped control and a renewable
/// lease instead of waiting for the first sensitive operation to fail.
pub(crate) fn connect_compositor(socket: &Path, timeout: Duration) -> io::Result<Client> {
    let requested = ConnectionCapabilities {
        query: true,
        control: true,
        input: false,
        session: false,
        interaction_domain: false,
    };
    let client =
        Client::connect_scoped_with_timeout(socket, requested, LOCAL_PORTAL_SCOPE, timeout)?;
    if !client.caps().control || !client.lease().is_some_and(|lease| lease.renewable) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Aegis did not grant the Portal scope a renewable control lease",
        ));
    }
    client.set_io_timeout(Some(timeout))?;
    Ok(client)
}

/// A lazily connected, lease-renewing `CaptureOutput` client. One instance
/// lives on the capture worker thread; it is not `Sync` by design.
pub(crate) struct PortalCapture {
    socket: PathBuf,
    client: Option<Client>,
    renewed_at: Instant,
}

impl PortalCapture {
    pub(crate) fn new(socket: PathBuf) -> Self {
        Self {
            socket,
            client: None,
            renewed_at: Instant::now(),
        }
    }

    fn connect(&self) -> io::Result<Client> {
        connect_compositor(&self.socket, RPC_TIMEOUT)
    }

    /// Hand out a live client, renewing an ageing lease or reconnecting an
    /// expired/broken one.
    fn client(&mut self) -> io::Result<&mut Client> {
        if let Some(client) = self.client.as_ref() {
            client.set_io_timeout(Some(RPC_TIMEOUT))?;
        }
        if let Some(client) = self.client.as_mut()
            && self.renewed_at.elapsed() >= Duration::from_millis(LEASE_TTL_MS / 2)
        {
            match client.renew_lease(LEASE_TTL_MS) {
                Ok(_) => self.renewed_at = Instant::now(),
                Err(error) => {
                    // An expired lease cannot be renewed; reconnecting is the
                    // recovery path. A vanished scope refuses the new
                    // handshake and surfaces here as a normal error.
                    log::info!("portal: lease renewal failed ({error}); reconnecting IPC");
                    self.client = None;
                }
            }
        }
        if self.client.is_none() {
            self.client = Some(self.connect()?);
            self.renewed_at = Instant::now();
        }
        Ok(self.client.as_mut().expect("connected above"))
    }

    /// Capture the focused output as PNG bytes. One automatic reconnect +
    /// retry hides transient failures (compositor restart, raced lease
    /// expiry); persistent failures surface as errors to the caller.
    pub(crate) fn capture_png(&mut self) -> io::Result<Vec<u8>> {
        match self.client()?.capture_output() {
            Ok((_, _, png)) => Ok(png),
            Err(first) => {
                log::info!("portal: capture failed ({first}); reconnecting IPC");
                self.client = None;
                let (_, _, png) = self.client()?.capture_output()?;
                Ok(png)
            }
        }
    }

    /// Capture a region of the focused output as PNG bytes (compositor
    /// logical pixels), with the same reconnect + retry discipline as
    /// [`PortalCapture::capture_png`].
    pub(crate) fn capture_region_png(
        &mut self,
        region: aegis_portal_ipc::Rect,
    ) -> io::Result<Vec<u8>> {
        match self.client()?.capture_output_region(Some(region)) {
            Ok((_, _, png)) => Ok(png),
            Err(first) => {
                log::info!("portal: region capture failed ({first}); reconnecting IPC");
                self.client = None;
                let (_, _, png) = self.client()?.capture_output_region(Some(region))?;
                Ok(png)
            }
        }
    }

    /// Run one interactive pick through compositor chrome. Blocks
    /// until the user confirms or cancels, so this can take far longer than
    /// any other call. No automatic retry: a reconnect would orphan the
    /// user-facing picker, and the compositor bounds the wait itself.
    pub(crate) fn pick(
        &mut self,
        kind: aegis_portal_ipc::PickKind,
    ) -> io::Result<aegis_portal_ipc::PickResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.pick_target(kind)
    }

    /// Ask the user a yes/no consent question through compositor chrome
    /// (portal consent dialogs). Same blocking, no-retry discipline as
    /// [`PortalCapture::pick`].
    pub(crate) fn pick_confirm(
        &mut self,
        title: String,
        body: String,
        accept_label: Option<String>,
    ) -> io::Result<aegis_portal_ipc::ConfirmPickResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.pick_confirm(title, body, accept_label)
    }
}

/// Apply a staged wallpaper file through the compositor. The `SetWallpaper`
/// op predates the projection's version floor, so every negotiated protocol
/// speaks it; what the compositor's dispatch requires is what
/// [`connect_compositor`] already enforces at the handshake: scoped `control`
/// and a live renewable lease. Unlike capture, the connection is per call:
/// wallpaper changes are rare, so a fresh handshake (its lease dies with the
/// connection) beats carrying a renewing client on the worker. One reconnect
/// + retry hides transient failures, matching [`PortalCapture`]'s discipline.
pub(crate) fn set_wallpaper(socket: &Path, staged: &Path) -> io::Result<()> {
    let mut client = connect_compositor(socket, RPC_TIMEOUT)?;
    match client.set_wallpaper(staged) {
        Ok(()) => Ok(()),
        Err(first) => {
            log::info!("portal: wallpaper IPC failed ({first}); reconnecting IPC");
            connect_compositor(socket, RPC_TIMEOUT)?.set_wallpaper(staged)
        }
    }
}
