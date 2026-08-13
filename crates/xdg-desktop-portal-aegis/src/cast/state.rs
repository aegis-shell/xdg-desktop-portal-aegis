//! Live cast state shared between the stream listener, the IPC source, and
//! teardown: the compositor-side transport (with the protocol-25 slot
//! table), the delivery negotiation, and the latest received frame.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use aegis_portal_ipc::Client;
use pipewire as pw;
use pw::spa;
use pw::sys as pw_sys;

use super::copy::PoolMem;
use super::format::{
    AnnouncedFormat, DMABUF_DATA_TYPE_BIT, DRM_FORMAT_MOD_LINEAR, FixatedFormat, announced_format,
};
use super::frame::FramePayload;
use super::{STREAM_MAX_FPS, StartState};

/// How the fixated PipeWire format makes frames reach the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeliveryMode {
    /// Copy every frame into the shared memory pool.
    Shm,
    /// The consumer fixated the modifier-bearing format: slot buffers may
    /// go out as `SPA_DATA_DmaBuf`.
    Dmabuf,
}

/// Live negotiation state. `param_changed` callbacks update it; the
/// `process` callback reads it for every frame.
#[derive(Debug)]
pub(crate) struct Negotiation {
    pub(crate) mode: DeliveryMode,
    /// The consumer's accepted `SPA_PARAM_BUFFERS_dataType` mask, when the
    /// peer's Buffers param has been observed. A consumer that fixates a
    /// modifier-bearing format is expected to accept DmaBuf buffers, so an
    /// unknown mask does not block forwarding; an observed mask without the
    /// DmaBuf bit does.
    pub(crate) consumer_data_types: Option<u32>,
}

impl Negotiation {
    pub(crate) fn forwarding_eligible(&self) -> bool {
        self.mode == DeliveryMode::Dmabuf
            && self
                .consumer_data_types
                .is_none_or(|mask| mask & DMABUF_DATA_TYPE_BIT != 0)
    }
}

/// One protocol-25 slot's binding to a PipeWire pool buffer.
#[derive(Debug)]
pub(crate) struct SlotBinding {
    /// The pool buffer patched onto this slot's descriptor at `add_buffer`.
    pub(crate) pool: Option<*mut pw_sys::pw_buffer>,
    /// The slot's buffer is with the consumer; the compositor must not
    /// reuse the slot until the release goes out.
    pub(crate) in_flight: bool,
}

/// The compositor-side transport behind the PipeWire stream: which
/// compositor stream frames belong to, what layout they have, and the
/// protocol-25 slot table when streaming dmabuf slots. Shared between the
/// stream listener, the IPC source, and teardown so a transport switch
/// (dmabuf slots ↔ SHM readback) is observed everywhere without
/// renegotiating the PipeWire stream.
pub(crate) struct Transport {
    pub(crate) stream_id: u64,
    pub(crate) announced: AnnouncedFormat,
    pub(crate) slot_files: Vec<aegis_portal_ipc::StreamSlot>,
    pub(crate) slot_bindings: Vec<SlotBinding>,
}

impl Transport {
    pub(crate) fn slot_count(&self) -> usize {
        self.slot_files.len()
    }

    /// True when the copy path may memory-map this transport's frames:
    /// CPU-typed SHM pixels or LINEAR dmabufs. A tiled dmabuf memory-maps
    /// to tile-swizzled bytes, so those frames must come from the
    /// compositor's SHM readback transport instead.
    pub(crate) fn cpu_mappable(&self) -> bool {
        match self.announced {
            AnnouncedFormat::Shm(_) => true,
            AnnouncedFormat::Dmabuf { modifier, .. } => modifier == DRM_FORMAT_MOD_LINEAR,
        }
    }
}

/// Latest frame shared between the IPC source (writer) and the PipeWire
/// `process` callback (reader). `None` until the first frame arrives.
pub(crate) type LatestFrame = Rc<RefCell<Option<FramePayload>>>;

/// Stream-listener user data.
pub(crate) struct StreamData {
    pub(crate) latest: LatestFrame,
    /// Set when a new IPC frame has arrived but not yet been pushed to
    /// PipeWire. Cleared by the `process` callback after publishing.
    pub(crate) pending: Rc<Cell<bool>>,
    pub(crate) width: u32,
    pub(crate) height: u32,
    /// The SPA raw format offered at connect; fixated formats and
    /// restarted transports are validated against it.
    pub(crate) spa_format: spa::param::video::VideoFormat,
    /// The modifier offered at connect, when a dmabuf slot stream was
    /// announced. Consumers can only fixate this modifier, so transport
    /// decisions key off it rather than the current transport state.
    pub(crate) offered_modifier: Option<u64>,
    /// The live compositor transport; swapped by `sync_transport`.
    pub(crate) transport: Rc<RefCell<Transport>>,
    pub(crate) negotiation: RefCell<Negotiation>,
    /// Unbound pool buffers dequeued in earlier cycles; the copy path
    /// fills them.
    pub(crate) pool: RefCell<Vec<*mut pw_sys::pw_buffer>>,
    /// Portal-owned memfd backing for copy-path pool buffers, keyed by
    /// `pw_buffer` pointer. With `ALLOC_BUFFERS` the producer supplies the
    /// pool memory; entries are unmapped at `remove_buffer` and teardown.
    pub(crate) pool_mem: Rc<RefCell<HashMap<usize, PoolMem>>>,
    /// The IPC client, for slot releases and transport restarts.
    pub(crate) client: Rc<RefCell<Client>>,
    /// Quit handle for fatal transport errors.
    pub(crate) mainloop: pw::main_loop::MainLoopWeak,
    pub(crate) start_state: Rc<RefCell<StartState>>,
    /// Portal-side frame drops (unmappable dmabuf, pool starvation),
    /// counted for the stream's lifetime.
    pub(crate) dropped_frames: Cell<u64>,
    /// Rate-limit the unmappable-dmabuf warning to once per stream.
    pub(crate) warned_unmappable: Cell<bool>,
}

impl StreamData {
    /// Tell the compositor a slot is reusable. Best-effort: the stream's
    /// teardown cleans up regardless.
    pub(crate) fn release_slot(&self, slot: u32) {
        let (stream_id, has_slots) = {
            let transport = self.transport.borrow();
            (transport.stream_id, !transport.slot_files.is_empty())
        };
        if !has_slots {
            return;
        }
        if let Err(error) = self
            .client
            .borrow_mut()
            .release_stream_buffer(stream_id, slot)
        {
            log::debug!("portal: slot release for stream {stream_id} failed: {error}");
        }
    }
}

impl StreamData {
    /// Record the fixated format: verify it against what was offered,
    /// switch the compositor transport when the current one cannot serve
    /// it, and derive the delivery mode. Consumers can renegotiate
    /// mid-stream (OBS removes an unimportable modifier and retries), so
    /// this runs on every `Format` param change.
    pub(crate) fn apply_fixated_format(&self, fixated: &FixatedFormat) {
        if fixated.spa_format != self.spa_format.as_raw() {
            log::warn!(
                "portal: consumer fixated SPA format {} but only {} was offered",
                fixated.spa_format,
                self.spa_format.as_raw()
            );
        }
        if fixated.width != self.width || fixated.height != self.height {
            log::warn!(
                "portal: consumer fixated {}x{} but the compositor streams {}x{}",
                fixated.width,
                fixated.height,
                self.width,
                self.height
            );
        }
        if let Err(error) = self.sync_transport(fixated.modifier) {
            log::error!("portal: compositor transport switch failed: {error}");
            if let Some(mainloop) = self.mainloop.upgrade() {
                mainloop.quit();
            }
            return;
        }
        let transport = self.transport.borrow();
        let mode = match (transport.announced, fixated.modifier) {
            (AnnouncedFormat::Dmabuf { modifier, .. }, Some(fixated_modifier))
                if fixated_modifier == modifier && !transport.slot_files.is_empty() =>
            {
                DeliveryMode::Dmabuf
            }
            (AnnouncedFormat::Dmabuf { modifier, .. }, Some(fixated_modifier))
                if !transport.slot_files.is_empty() =>
            {
                log::warn!(
                    "portal: consumer fixated modifier {fixated_modifier:#x} but the compositor streams {modifier:#x}; falling back to SHM delivery"
                );
                DeliveryMode::Shm
            }
            _ => DeliveryMode::Shm,
        };
        drop(transport);
        let mut negotiation = self.negotiation.borrow_mut();
        if negotiation.mode != mode {
            match mode {
                DeliveryMode::Dmabuf => log::info!(
                    "portal: pipewire consumer negotiated zero-copy dmabuf capture ({}x{})",
                    self.width,
                    self.height
                ),
                DeliveryMode::Shm => log::info!(
                    "portal: pipewire consumer negotiated shared-memory capture ({}x{})",
                    self.width,
                    self.height
                ),
            }
            negotiation.mode = mode;
        }
    }

    /// Restart the compositor stream on the transport the fixated PipeWire
    /// format needs: dmabuf slots when the consumer fixated the offered
    /// modifier, the compositor's SHM readback when it did not. A no-op
    /// when the current transport already serves the fixation — crucially,
    /// a LINEAR dmabuf transport stays, because memory-mapping it is
    /// exact. A tiled dmabuf transport never serves SHM consumers: the
    /// copy path would read tile-swizzled bytes, so the readback
    /// transport (which de-tiles on the GPU) takes over. The PipeWire
    /// stream itself is untouched: the offered format is identical on
    /// both transports, so the consumer never observes the switch.
    fn sync_transport(&self, fixated_modifier: Option<u64>) -> Result<(), String> {
        let want_dmabuf = matches!(
            (self.offered_modifier, fixated_modifier),
            (Some(offered), Some(fixated)) if fixated == offered
        );
        let (stream_id, needs_switch) = {
            let transport = self.transport.borrow();
            let is_dmabuf = matches!(transport.announced, AnnouncedFormat::Dmabuf { .. });
            let needs = if want_dmabuf {
                !is_dmabuf
            } else {
                is_dmabuf && !transport.cpu_mappable()
            };
            (transport.stream_id, needs)
        };
        if !needs_switch {
            return Ok(());
        }
        let mut client = self.client.borrow_mut();
        client
            .stop_output_stream(stream_id)
            .map_err(|e| format!("stop compositor stream {stream_id}: {e}"))?;
        let started = client
            .start_output_stream(
                Some(STREAM_MAX_FPS),
                aegis_portal_ipc::StreamTarget::Output,
                want_dmabuf,
            )
            .map_err(|e| format!("restart compositor stream (dmabuf={want_dmabuf}): {e}"))?;
        drop(client);
        if started.width != self.width || started.height != self.height {
            return Err(format!(
                "restarted stream geometry {}x{} differs from the negotiated {}x{}",
                started.width, started.height, self.width, self.height
            ));
        }
        let announced = announced_format(started.format)?;
        if announced.spa_format() != self.spa_format {
            return Err(format!(
                "restarted stream format {announced:?} differs from the negotiated {:?}",
                self.spa_format
            ));
        }
        let slot_files = started.slots.unwrap_or_default();
        log::info!(
            "portal: restarted compositor stream {} as {} ({} slots)",
            started.stream_id,
            match announced {
                AnnouncedFormat::Dmabuf { .. } => "dmabuf slots",
                AnnouncedFormat::Shm(_) => "shared-memory readback",
            },
            slot_files.len()
        );
        let slot_count = slot_files.len();
        let mut transport = self.transport.borrow_mut();
        transport.stream_id = started.stream_id;
        transport.announced = announced;
        transport.slot_files = slot_files;
        transport.slot_bindings = (0..slot_count)
            .map(|_| SlotBinding {
                pool: None,
                in_flight: false,
            })
            .collect();
        drop(transport);
        // Frames of the superseded stream must never be published.
        *self.latest.borrow_mut() = None;
        self.pending.set(false);
        Ok(())
    }
}
