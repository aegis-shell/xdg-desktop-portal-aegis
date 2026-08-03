//! The inward half of the bridge: a scoped aegis-IPC capture client.
//!
//! The portal connects under the built-in owner-only `aegis-portal` scope
//! (`aegis_ipc::LOCAL_PORTAL_SCOPE`), which the compositor resolves to exactly
//! the capture, stream, target-picking, and idle-inhibit operations used by
//! the advertised portal interfaces, with the `control` capability and a
//! time-bounded lease. The wrapper keeps each connection alive across idle
//! periods by renewing the lease at half its TTL, and reconnects once on any
//! failure so a compositor restart or an expired lease self-heals on the
//! next screenshot instead of killing the D-Bus service.

use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aegis_ipc::{Client, ConnectionCapabilities, LOCAL_PORTAL_SCOPE};

/// Lease TTL requested at handshake and renewal; matches the reference
/// client's default (`LeaseRequest::default`).
const LEASE_TTL_MS: u64 = 900_000;
/// Ordinary compositor RPCs must not retain a portal worker forever if a
/// local peer accepts the socket and then stops responding.
const RPC_TIMEOUT: Duration = Duration::from_secs(15);
/// Interactive compositor chrome is itself bounded at five minutes. Leave
/// a small transport margin so its typed cancellation/error can arrive.
const INTERACTION_TIMEOUT: Duration = Duration::from_secs(305);

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
        let caps = ConnectionCapabilities {
            query: true,
            control: true,
            input: false,
            session: false,
            interaction_domain: false,
        };
        let client = Client::connect_scoped_with_timeout(
            &self.socket,
            caps,
            LOCAL_PORTAL_SCOPE,
            RPC_TIMEOUT,
        )?;
        client.set_io_timeout(Some(RPC_TIMEOUT))?;
        Ok(client)
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
    pub(crate) fn capture_region_png(&mut self, region: aegis_core::Rect) -> io::Result<Vec<u8>> {
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
    pub(crate) fn pick(&mut self, kind: aegis_ipc::PickKind) -> io::Result<aegis_ipc::PickResult> {
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
    ) -> io::Result<aegis_ipc::ConfirmPickResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.pick_confirm(title, body, accept_label)
    }

    /// Ask the user for the vault password through the compositor's masked
    /// secret prompt (the vault unlock's compositor side). Same blocking,
    /// no-retry discipline as [`PortalCapture::pick`].
    pub(crate) fn prompt_secret(
        &mut self,
        title: String,
        reason: Option<String>,
    ) -> io::Result<aegis_ipc::SecretPromptResult> {
        let client = self.client()?;
        client.set_io_timeout(Some(INTERACTION_TIMEOUT))?;
        client.prompt_secret(title, reason)
    }
}
