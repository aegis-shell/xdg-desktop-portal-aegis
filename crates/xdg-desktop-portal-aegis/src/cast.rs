//! One PipeWire producer stream per started ScreenCast session.
//!
//! Each cast runs on its own thread: a scoped IPC connection receives the
//! compositor's output-frame stream and a PipeWire `Output` stream
//! republishes every new frame as raw video at the compositor's cadence.
//! The PipeWire main loop is also the IPC event loop — the IPC socket, the
//! stop socket, and the lease-renewal timer are ordinary loop sources, so
//! the thread never blocks anywhere but in `poll`.
//!
//! Two frame transports exist (see ADR-0005):
//!
//! - **Slot streaming (protocol 25)**: the compositor transfers a fixed set
//!   of dmabuf slot descriptors once at start, and each frame references a
//!   slot by index. The stream connects with `ALLOC_BUFFERS`, so every pool
//!   buffer's data slot is filled in the `add_buffer` event — bound to a
//!   compositor slot (`SPA_DATA_DmaBuf`) when the consumer negotiated the
//!   modifier-bearing format, or given a Portal-owned memfd otherwise.
//!   PipeWire fixes buffer descriptors at registration, which is why this
//!   is the only zero-copy shape that works; per-frame descriptors cannot
//!   be forwarded. A slot comes back when the consumer returns its buffer,
//!   and only then is its release reported to the compositor.
//! - **Per-frame descriptors**: every frame carries a sealed memfd or a
//!   single-plane dmabuf behind its header. The frame is memory-mapped and
//!   copied into a pool buffer exactly once. Memory-mapping is only defined
//!   for CPU-typed pixels, so this path serves SHM frames and LINEAR
//!   dmabufs; a tiled dmabuf would copy tile-swizzled bytes.
//!
//! The compositor's dmabuf slots normally carry a device-native tiled
//! modifier, which only GPU consumers can import. When the fixated
//! PipeWire format cannot be served by the current transport — a
//! modifier-ignorant consumer facing a tiled slot stream, or a GPU
//! consumer facing an SHM stream — the compositor stream is restarted on
//! the matching transport (the `dmabuf` flag at `StreamOutputStart`)
//! underneath the live PipeWire connection. The offered format is the same
//! on both transports, so the consumer never observes the switch.
//!
//! The stream is a PipeWire DRIVER fed by `pw_stream_trigger_process` when
//! a compositor frame arrives, so graph cycles run only when there is a new
//! frame. The latest frame is republished on the Streaming transition: an
//! early trigger fails with EIO before the link is up, and a pause flushes
//! queued buffers back to the producer.
//!
//! Teardown is single-path: closing the write end of the stop socket (or a
//! compositor-side `StreamEnded`, or any read error) quits the loop, after
//! which dropping the IPC client disconnects it — and the compositor's
//! disconnect cleanup stops the stream with no extra round-trip.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use aegis_portal_ipc::{Client, StreamFrame, StreamMessage, StreamPayload};
use pipewire as pw;
use pw::spa;
use pw::spa::pod::{self, Pod};
use pw::spa::sys as spa_sys;
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle};
use pw::stream::{StreamFlags, StreamState};
use pw::sys as pw_sys;

use crate::ipc;
use crate::screencast::CastJob;

/// Frame-rate ceiling requested from the compositor. The compositor paces
/// frames itself (damage-driven, bounded by the output's vertical sync);
/// this is only a cap, and the server clamps it to its own supported range.
const STREAM_MAX_FPS: u32 = 60;
/// Framerate choice offered to PipeWire consumers. Frames arrive at the
/// compositor's actual cadence; the range lets each consumer pick the rate
/// its own pipeline wants instead of forcing one fixed clock on everyone.
const FRAMERATE_DEFAULT: Fraction = Fraction { num: 60, denom: 1 };
const FRAMERATE_MIN: Fraction = Fraction { num: 1, denom: 1 };
const FRAMERATE_MAX: Fraction = Fraction { num: 360, denom: 1 };
/// A screencast publishes frames for PipeWire capture consumers.
const STREAM_DIRECTION: Direction = Direction::Output;
/// Lease TTL requested at handshake and renewal; renewed at half TTL.
const LEASE_TTL_MS: u64 = 900_000;
const IPC_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
/// `SPA_PARAM_BUFFERS_dataType` is a mask of `1 << SPA_DATA_*`: MemPtr is
/// bit 1, MemFd is bit 2, and DmaBuf is bit 3.
const DMABUF_DATA_TYPE_BIT: u32 = 1 << 3;
const ALL_DATA_TYPES: u32 = (1 << 1) | (1 << 2) | DMABUF_DATA_TYPE_BIT;
/// DRM_FORMAT_MOD_LINEAR: the only dmabuf layout the copy path may
/// memory-map. Tiled layouts read back tile-swizzled on the CPU, so they
/// must come from the compositor's SHM readback instead.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Negotiated parameters of a running cast, handed back to the worker once
/// the stream reaches `Paused` (the first state where the node id exists).
pub(crate) struct CastStarted {
    pub(crate) node_id: u32,
    pub(crate) serial: u64,
    pub(crate) width: u32,
    pub(crate) height: u32,
}

/// Live cast handle owned by the session registry: drop `stop` to end the
/// cast, then `join` the thread before reporting the session closed.
pub(crate) struct CastHandle {
    pub(crate) stop: UnixStream,
    pub(crate) started: mpsc::Receiver<Result<CastStarted, String>>,
    pub(crate) thread: std::thread::JoinHandle<()>,
}

/// Spawn the cast thread. Returns immediately; the PipeWire negotiation
/// result arrives on `handle.started` exactly once.
pub(crate) fn spawn(
    socket: PathBuf,
    session_path: String,
    jobs: mpsc::SyncSender<CastJob>,
) -> io::Result<CastHandle> {
    let (stop_read, stop_write) = UnixStream::pair()?;
    let (started_tx, started_rx) = mpsc::channel();
    let thread = std::thread::Builder::new()
        .name("aegis-portal-cast".to_string())
        .spawn(move || cast_thread(socket, session_path, jobs, stop_read, started_tx))?;
    Ok(CastHandle {
        stop: stop_write,
        started: started_rx,
        thread,
    })
}

/// `AsRawFd` wrapper so the shared IPC client can be a PipeWire loop source.
/// The timer and the IO callback both reach the client through the `Rc`;
/// PipeWire's `Fn` callbacks require interior mutability.
struct IpcFd(Rc<RefCell<Client>>);

impl AsRawFd for IpcFd {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.0.borrow().as_raw_fd()
    }
}

/// The pixel format the compositor announced at `StreamOutputStart`, with
/// the PipeWire-side mapping resolved once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnouncedFormat {
    /// Sealed-memfd SHM frames; the value is the SPA raw format offered to
    /// PipeWire (`BGRx` for Bgra8, `RGBx` for Rgba8; compositor alpha is
    /// always opaque).
    Shm(spa::param::video::VideoFormat),
    /// Single-plane dmabuf frames with a fixed DRM format/modifier pair;
    /// the value is the equivalent SPA raw format.
    Dmabuf {
        drm_format: u32,
        modifier: u64,
        spa_format: spa::param::video::VideoFormat,
    },
}

impl AnnouncedFormat {
    fn spa_format(&self) -> spa::param::video::VideoFormat {
        match *self {
            AnnouncedFormat::Shm(format)
            | AnnouncedFormat::Dmabuf {
                spa_format: format, ..
            } => format,
        }
    }
}

/// Resolve the compositor's announced wire format into an offerable SPA
/// format. Unknown dmabuf fourccs fail the cast: offering a guessed pixel
/// layout would produce wrong colors in the consumer.
fn announced_format(
    format: aegis_portal_ipc::StreamPixelFormat,
) -> Result<AnnouncedFormat, String> {
    use aegis_portal_ipc::StreamPixelFormat as Wire;
    match format {
        Wire::Bgra8 => Ok(AnnouncedFormat::Shm(spa::param::video::VideoFormat::BGRx)),
        Wire::Rgba8 => Ok(AnnouncedFormat::Shm(spa::param::video::VideoFormat::RGBx)),
        Wire::Dmabuf {
            drm_format,
            modifier,
        } => {
            let spa_format = spa_format_for_drm(drm_format).ok_or_else(|| {
                format!("unsupported compositor dmabuf format {drm_format:#010x}")
            })?;
            Ok(AnnouncedFormat::Dmabuf {
                drm_format,
                modifier,
                spa_format,
            })
        }
    }
}

/// Little-endian DRM fourcc, matching `drm_fourcc.h`'s `fourcc_code`.
const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// Map a single-plane 8-bit DRM fourcc to the SPA raw video format with the
/// same memory order (DRM names read most- to least-significant byte, SPA
/// names read in memory order).
fn spa_format_for_drm(drm_format: u32) -> Option<spa::param::video::VideoFormat> {
    use spa::param::video::VideoFormat;
    Some(match drm_format {
        f if f == fourcc(b'X', b'R', b'2', b'4') => VideoFormat::BGRx,
        f if f == fourcc(b'A', b'R', b'2', b'4') => VideoFormat::BGRA,
        f if f == fourcc(b'X', b'B', b'2', b'4') => VideoFormat::RGBx,
        f if f == fourcc(b'A', b'B', b'2', b'4') => VideoFormat::RGBA,
        f if f == fourcc(b'R', b'X', b'2', b'4') => VideoFormat::xBGR,
        f if f == fourcc(b'R', b'A', b'2', b'4') => VideoFormat::ABGR,
        f if f == fourcc(b'B', b'X', b'2', b'4') => VideoFormat::xRGB,
        f if f == fourcc(b'B', b'A', b'2', b'4') => VideoFormat::ARGB,
        _ => return None,
    })
}

/// A received compositor frame.
enum FramePayload {
    /// A frame carrying its own descriptor (sealed memfd or dmabuf blob),
    /// plus the plane stride from the frame header.
    Descriptor { file: File, stride: u32 },
    /// A protocol-25 frame referencing a slot transferred at start.
    Slot(u32),
}

/// How the fixated PipeWire format makes frames reach the consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DeliveryMode {
    /// Copy every frame into the shared memory pool.
    Shm,
    /// The consumer fixated the modifier-bearing format: slot buffers may
    /// go out as `SPA_DATA_DmaBuf`.
    Dmabuf,
}

/// Live negotiation state. `param_changed` callbacks update it; the
/// `process` callback reads it for every frame.
#[derive(Debug)]
struct Negotiation {
    mode: DeliveryMode,
    /// The consumer's accepted `SPA_PARAM_BUFFERS_dataType` mask, when the
    /// peer's Buffers param has been observed. A consumer that fixates a
    /// modifier-bearing format is expected to accept DmaBuf buffers, so an
    /// unknown mask does not block forwarding; an observed mask without the
    /// DmaBuf bit does.
    consumer_data_types: Option<u32>,
}

impl Negotiation {
    fn forwarding_eligible(&self) -> bool {
        self.mode == DeliveryMode::Dmabuf
            && self
                .consumer_data_types
                .is_none_or(|mask| mask & DMABUF_DATA_TYPE_BIT != 0)
    }
}

/// One protocol-25 slot's binding to a PipeWire pool buffer.
#[derive(Debug)]
struct SlotBinding {
    /// The pool buffer patched onto this slot's descriptor at `add_buffer`.
    pool: Option<*mut pw_sys::pw_buffer>,
    /// The slot's buffer is with the consumer; the compositor must not
    /// reuse the slot until the release goes out.
    in_flight: bool,
}

/// The compositor-side transport behind the PipeWire stream: which
/// compositor stream frames belong to, what layout they have, and the
/// protocol-25 slot table when streaming dmabuf slots. Shared between the
/// stream listener, the IPC source, and teardown so a transport switch
/// (dmabuf slots ↔ SHM readback) is observed everywhere without
/// renegotiating the PipeWire stream.
struct Transport {
    stream_id: u64,
    announced: AnnouncedFormat,
    slot_files: Vec<aegis_portal_ipc::StreamSlot>,
    slot_bindings: Vec<SlotBinding>,
}

impl Transport {
    fn slot_count(&self) -> usize {
        self.slot_files.len()
    }

    /// True when the copy path may memory-map this transport's frames:
    /// CPU-typed SHM pixels or LINEAR dmabufs. A tiled dmabuf memory-maps
    /// to tile-swizzled bytes, so those frames must come from the
    /// compositor's SHM readback transport instead.
    fn cpu_mappable(&self) -> bool {
        match self.announced {
            AnnouncedFormat::Shm(_) => true,
            AnnouncedFormat::Dmabuf { modifier, .. } => modifier == DRM_FORMAT_MOD_LINEAR,
        }
    }
}

/// Latest frame shared between the IPC source (writer) and the PipeWire
/// `process` callback (reader). `None` until the first frame arrives.
type LatestFrame = Rc<RefCell<Option<FramePayload>>>;

/// Portal-owned backing for one copy-path pool buffer: a memfd the
/// consumer maps, plus our own mapping of it.
struct PoolMem {
    file: File,
    map: *mut u8,
    len: usize,
}

impl PoolMem {
    fn new(len: usize) -> io::Result<PoolMem> {
        // SAFETY: the name is static and NUL-terminated; the fd is checked
        // before ownership is constructed.
        let fd = unsafe { libc::memfd_create(c"aegis-portal-pool".as_ptr(), libc::MFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a new owned descriptor from memfd_create.
        let file = unsafe { File::from_raw_fd(fd) };
        file.set_len(len as u64)?;
        // SAFETY: the file is `len` bytes and outlives the mapping (owned by
        // the returned value).
        let map = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                file.as_raw_fd(),
                0,
            )
        };
        if map == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        Ok(PoolMem {
            file,
            map: map.cast::<u8>(),
            len,
        })
    }
}

impl Drop for PoolMem {
    fn drop(&mut self) {
        // SAFETY: `map`/`len` name the live mapping created in `new`.
        unsafe { libc::munmap(self.map.cast(), self.len) };
    }
}

/// Stream-listener user data.
struct StreamData {
    latest: LatestFrame,
    /// Set when a new IPC frame has arrived but not yet been pushed to
    /// PipeWire. Cleared by the `process` callback after publishing.
    pending: Rc<Cell<bool>>,
    width: u32,
    height: u32,
    /// The SPA raw format offered at connect; fixated formats and
    /// restarted transports are validated against it.
    spa_format: spa::param::video::VideoFormat,
    /// The modifier offered at connect, when a dmabuf slot stream was
    /// announced. Consumers can only fixate this modifier, so transport
    /// decisions key off it rather than the current transport state.
    offered_modifier: Option<u64>,
    /// The live compositor transport; swapped by `sync_transport`.
    transport: Rc<RefCell<Transport>>,
    negotiation: RefCell<Negotiation>,
    /// Unbound pool buffers dequeued in earlier cycles; the copy path
    /// fills them.
    pool: RefCell<Vec<*mut pw_sys::pw_buffer>>,
    /// Portal-owned memfd backing for copy-path pool buffers, keyed by
    /// `pw_buffer` pointer. With `ALLOC_BUFFERS` the producer supplies the
    /// pool memory; entries are unmapped at `remove_buffer` and teardown.
    pool_mem: Rc<RefCell<HashMap<usize, PoolMem>>>,
    /// The IPC client, for slot releases and transport restarts.
    client: Rc<RefCell<Client>>,
    /// Quit handle for fatal transport errors.
    mainloop: pw::main_loop::MainLoopWeak,
    start_state: Rc<RefCell<StartState>>,
    /// Portal-side frame drops (unmappable dmabuf, pool starvation),
    /// counted for the stream's lifetime.
    dropped_frames: Cell<u64>,
    /// Rate-limit the unmappable-dmabuf warning to once per stream.
    warned_unmappable: Cell<bool>,
}

impl StreamData {
    /// Tell the compositor a slot is reusable. Best-effort: the stream's
    /// teardown cleans up regardless.
    fn release_slot(&self, slot: u32) {
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

/// PipeWire reports the node id through the stream state callback and the
/// stable object serial through registry properties. Either event may arrive
/// first, so Start completes only after both have been observed.
struct StartState {
    started: Option<mpsc::Sender<Result<CastStarted, String>>>,
    paused: Option<(u32, u32, u32)>,
    serials: std::collections::HashMap<u32, u64>,
    completed: bool,
}

impl StartState {
    fn try_complete(&mut self) {
        let Some((node_id, width, height)) = self.paused else {
            return;
        };
        let Some(serial) = self.serials.get(&node_id).copied() else {
            return;
        };
        if let Some(started) = self.started.take() {
            self.completed = true;
            let _ = started.send(Ok(CastStarted {
                node_id,
                serial,
                width,
                height,
            }));
        }
    }
}

fn cast_thread(
    socket: PathBuf,
    session_path: String,
    jobs: mpsc::SyncSender<CastJob>,
    stop_read: UnixStream,
    started: mpsc::Sender<Result<CastStarted, String>>,
) {
    if let Err(error) = run_cast(&socket, &session_path, &jobs, stop_read, &started) {
        log::warn!("portal: cast for {session_path} failed: {error}");
        let _ = started.send(Err(error));
    }
    // Dropping here closes the IPC connection; the compositor's disconnect
    // cleanup stops the output stream.
}

/// Check one received frame against the stream's announced format and
/// geometry, returning the storable payload.
fn validate_frame(
    frame: StreamFrame,
    width: u32,
    height: u32,
    announced: AnnouncedFormat,
    slot_count: usize,
) -> Result<FramePayload, String> {
    if frame.width != width || frame.height != height {
        return Err(format!(
            "frame geometry {}x{} differs from the announced {width}x{height}",
            frame.width, frame.height
        ));
    }
    let row_bytes = width as u64 * 4;
    if u64::from(frame.stride) < row_bytes || frame.stride > i32::MAX as u32 {
        return Err(format!("invalid frame stride {}", frame.stride));
    }
    if let Some(slot) = frame.slot {
        if slot_count == 0 {
            return Err("slot frame on a stream without a slot table".to_string());
        }
        if slot as usize >= slot_count {
            return Err(format!(
                "slot {slot} is outside the {slot_count}-slot table"
            ));
        }
        if !matches!(frame.payload, StreamPayload::Slot) {
            return Err("slot frame carried a descriptor".to_string());
        }
        return Ok(FramePayload::Slot(slot));
    }
    match (announced, frame.format, frame.payload) {
        (
            AnnouncedFormat::Shm(_),
            aegis_portal_ipc::StreamPixelFormat::Bgra8 | aegis_portal_ipc::StreamPixelFormat::Rgba8,
            StreamPayload::Memfd(file),
        ) => {
            // The compositor's SHM readback is tightly packed.
            if u64::from(frame.stride) != row_bytes {
                return Err(format!(
                    "SHM frame stride {} is not tightly packed",
                    frame.stride
                ));
            }
            Ok(FramePayload::Descriptor {
                file,
                stride: frame.stride,
            })
        }
        (
            AnnouncedFormat::Dmabuf {
                drm_format,
                modifier,
                ..
            },
            aegis_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: frame_drm,
                modifier: frame_modifier,
            },
            StreamPayload::Dmabuf(file),
        ) if frame_drm == drm_format && frame_modifier == modifier => {
            // The copy path memory-maps the descriptor, which is only
            // defined for CPU-typed pixels: a tiled dmabuf would copy
            // tile-swizzled bytes. Tiled streams belong to the
            // compositor's SHM readback (see `sync_transport`).
            if modifier != DRM_FORMAT_MOD_LINEAR {
                return Err(format!(
                    "dmabuf frame with non-LINEAR modifier {modifier:#x} cannot be memory-mapped"
                ));
            }
            Ok(FramePayload::Descriptor {
                file,
                stride: frame.stride,
            })
        }
        (announced, wire, _) => Err(format!(
            "frame format {wire:?} does not match the announced {announced:?}"
        )),
    }
}

/// Reclaim every buffer PipeWire returns to the producer. A buffer bound to
/// a compositor slot triggers the slot's release; unbound buffers go to the
/// copy path's stash.
fn reclaim_returned_buffers(stream: &pw::stream::Stream, data: &StreamData) {
    loop {
        // SAFETY: called from the stream's own thread inside `process`.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        let Some(raw) = NonNull::new(raw) else {
            break;
        };
        let mut transport = data.transport.borrow_mut();
        let bound = transport
            .slot_bindings
            .iter_mut()
            .enumerate()
            .find(|(_, binding)| binding.pool == Some(raw.as_ptr()));
        if let Some((slot, binding)) = bound {
            if binding.in_flight {
                binding.in_flight = false;
                let stream_id = transport.stream_id;
                let has_slots = !transport.slot_files.is_empty();
                let client = Rc::clone(&data.client);
                drop(transport);
                if has_slots
                    && let Err(error) = client
                        .borrow_mut()
                        .release_stream_buffer(stream_id, slot as u32)
                {
                    log::debug!("portal: slot release failed: {error}");
                }
            }
        } else {
            drop(transport);
            data.pool.borrow_mut().push(raw.as_ptr());
        }
    }
}

/// The PipeWire `process` callback: publish the pending frame, if any.
fn process_frame(stream: &pw::stream::Stream, data: &mut StreamData) {
    reclaim_returned_buffers(stream, data);

    if !data.pending.replace(false) {
        return;
    }
    // The latest frame stays stored after publishing: a consumer that
    // (re)activates later gets it republished on the Streaming transition,
    // since queued buffers are flushed back on pause.
    let frame = data.latest.borrow();
    let Some(frame) = &*frame else {
        return;
    };
    match frame {
        FramePayload::Descriptor { file, stride } => copy_into_pool(stream, data, file, *stride),
        FramePayload::Slot(slot) => publish_slot(stream, data, *slot),
    }
}

/// Publish a protocol-25 slot frame: queue the pool buffer bound to the
/// slot when the consumer takes dmabufs, or copy the slot's pixels into a
/// free pool buffer (and release the slot immediately) otherwise.
fn publish_slot(stream: &pw::stream::Stream, data: &StreamData, slot: u32) {
    let mut transport = data.transport.borrow_mut();
    let Some(binding) = transport.slot_bindings.get_mut(slot as usize) else {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!("portal: frame for unknown slot {slot}; dropping");
        return;
    };
    if binding.in_flight {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!("portal: compositor reused slot {slot} before its release; dropping frame");
        return;
    }
    let forward = data.negotiation.borrow().forwarding_eligible();
    match (forward, binding.pool) {
        (true, Some(pool_raw)) => {
            binding.in_flight = true;
            drop(transport);
            // SAFETY: `pool_raw` is a live pool buffer of this stream bound
            // to this slot, dequeued earlier and not referenced elsewhere.
            unsafe { stream.queue_raw_buffer(pool_raw) };
        }
        _ => {
            if !transport.cpu_mappable() {
                // Never linear-copy tiled pixels: the transport switch to
                // the compositor's SHM readback is what serves this
                // consumer. Drop the frame and hand the slot back so the
                // compositor's ring keeps turning until then.
                data.dropped_frames.set(data.dropped_frames.get() + 1);
                drop(transport);
                data.release_slot(slot);
                return;
            }
            let file = &transport.slot_files[slot as usize].file;
            let stride = transport.slot_files[slot as usize].stride;
            copy_into_pool(stream, data, file, stride);
            drop(transport);
            data.release_slot(slot);
        }
    }
}

/// Map a frame descriptor and copy the pixels into a shared-pool buffer.
/// Sealed memfds and mappable dmabufs take the same path.
fn copy_into_pool(stream: &pw::stream::Stream, data: &StreamData, file: &File, stride: u32) {
    let height = data.height as usize;
    let row_bytes = data.width as usize * 4;
    let stride = stride as usize;

    let Ok(src_len) = file.metadata().map(|meta| meta.len() as usize) else {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::debug!("portal: could not stat the frame descriptor");
        return;
    };
    let needed = stride * (height - 1) + row_bytes;
    if src_len < needed || needed > MAX_FRAME_BYTES {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::warn!(
            "portal: frame payload of {src_len} bytes cannot hold {height} rows of stride {stride}"
        );
        return;
    }
    // SAFETY: the descriptor outlives the mapping; the caller owns it.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            src_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        if data.warned_unmappable.replace(true) {
            log::debug!(
                "portal: frame descriptor is not mappable: {}",
                io::Error::last_os_error()
            );
        } else {
            log::warn!(
                "portal: frame descriptor is not mappable ({}); frames are dropped because \
                 this capture cannot be delivered as shared memory",
                io::Error::last_os_error()
            );
        }
        return;
    }

    let pool_raw = data
        .pool
        .borrow_mut()
        .pop()
        .or_else(|| NonNull::new(unsafe { stream.dequeue_raw_buffer() }).map(NonNull::as_ptr));
    let published = match pool_raw {
        Some(pool_raw) => unsafe {
            // SAFETY: `pool_raw` is a live pool buffer of this stream,
            // dequeued on this thread; its first spa_data is a mapped memory
            // block of `maxsize` bytes.
            let spa_buffer = (*pool_raw).buffer;
            let spa_data = (*spa_buffer).datas;
            let dest_ptr = (*spa_data).data.cast::<u8>();
            let dest_cap = (*spa_data).maxsize as usize;
            if dest_ptr.is_null() || dest_cap < height * row_bytes {
                data.pool.borrow_mut().push(pool_raw);
                false
            } else {
                let dest = std::slice::from_raw_parts_mut(dest_ptr, dest_cap);
                let src = std::slice::from_raw_parts(map.cast::<u8>(), src_len);
                copy_rows(src, stride, dest, row_bytes, height);
                let chunk = (*spa_data).chunk;
                (*chunk).offset = 0;
                (*chunk).size = (height * row_bytes) as u32;
                (*chunk).stride = row_bytes as i32;
                (*chunk).flags = spa_sys::SPA_CHUNK_FLAG_NONE as i32;
                stream.queue_raw_buffer(pool_raw);
                true
            }
        },
        None => false,
    };
    if !published {
        data.dropped_frames.set(data.dropped_frames.get() + 1);
        log::debug!("portal: no free PipeWire buffer; dropping frame");
    }
    // SAFETY: `map`/`src_len` name the live mapping created above.
    unsafe { libc::munmap(map, src_len) };
}

/// Copy `height` rows of `row_bytes` from a source with the given row
/// stride into a tightly packed destination.
fn copy_rows(src: &[u8], src_stride: usize, dest: &mut [u8], row_bytes: usize, height: usize) {
    if src_stride == row_bytes {
        dest[..height * row_bytes].copy_from_slice(&src[..height * row_bytes]);
        return;
    }
    for row in 0..height {
        let src_start = row * src_stride;
        let dest_start = row * row_bytes;
        dest[dest_start..dest_start + row_bytes]
            .copy_from_slice(&src[src_start..src_start + row_bytes]);
    }
}

/// The whole cast, as a fallible sequence. `started` is consumed by the
/// stream listener on success, so on failure the caller still owns it.
fn run_cast(
    socket: &std::path::Path,
    session_path: &str,
    jobs: &mpsc::SyncSender<CastJob>,
    stop_read: UnixStream,
    started: &mpsc::Sender<Result<CastStarted, String>>,
) -> Result<(), String> {
    // Inward half first: without frames there is nothing to publish.
    let mut client = ipc::connect_compositor(socket, IPC_TIMEOUT)
        .map_err(|e| format!("compositor IPC connect: {e}"))?;
    let stream_info = client
        .start_output_stream(
            Some(STREAM_MAX_FPS),
            aegis_portal_ipc::StreamTarget::Output,
            true,
        )
        .map_err(|e| format!("start output stream: {e}"))?;
    let (width, height) = (stream_info.width, stream_info.height);
    frame_len(width, height)?;
    let announced = announced_format(stream_info.format)?;
    let slot_files = stream_info.slots.unwrap_or_default();
    let slot_count = slot_files.len();
    let offered_modifier = match announced {
        AnnouncedFormat::Dmabuf { modifier, .. } if slot_count > 0 => Some(modifier),
        _ => None,
    };
    let transport = Rc::new(RefCell::new(Transport {
        stream_id: stream_info.stream_id,
        announced,
        slot_files,
        slot_bindings: (0..slot_count)
            .map(|_| SlotBinding {
                pool: None,
                in_flight: false,
            })
            .collect(),
    }));
    let teardown_transport = Rc::clone(&transport);
    let teardown_pool_mem: Rc<RefCell<HashMap<usize, PoolMem>>> =
        Rc::new(RefCell::new(HashMap::new()));
    log::info!(
        "portal: compositor stream {} for {session_path}: {width}x{height}, format {announced:?}, {slot_count} slots",
        stream_info.stream_id,
    );
    let client = Rc::new(RefCell::new(client));
    let teardown_client = Rc::clone(&client);

    // Outward half: PipeWire producer.
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let registry = core.get_registry_rc().map_err(|e| e.to_string())?;
    // Use a reference-counted stream so the IPC source can wake the PipeWire
    // side with trigger_process when a new compositor frame arrives.
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "xdg-desktop-portal-aegis-screencast",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let latest: LatestFrame = Rc::new(RefCell::new(None));
    let format_bytes = format_pods(width, height, announced, slot_count > 0);
    let pending = Rc::new(Cell::new(false));
    let start_state = Rc::new(RefCell::new(StartState {
        started: Some(started.clone()),
        paused: None,
        serials: std::collections::HashMap::new(),
        completed: false,
    }));
    let exit_start_state = Rc::clone(&start_state);
    let registry_start_state = Rc::clone(&start_state);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |object| {
            let Some(serial) = object
                .props
                .and_then(|properties| properties.get("object.serial"))
                .and_then(|serial| serial.parse::<u64>().ok())
            else {
                return;
            };
            let mut state = registry_start_state.borrow_mut();
            state.serials.insert(object.id, serial);
            state.try_complete();
        })
        .register();

    let state_loop_weak = mainloop.downgrade();
    let _listener = stream
        .add_local_listener_with_user_data(StreamData {
            latest: Rc::clone(&latest),
            pending: Rc::clone(&pending),
            width,
            height,
            spa_format: announced.spa_format(),
            offered_modifier,
            transport: Rc::clone(&transport),
            negotiation: RefCell::new(Negotiation {
                mode: DeliveryMode::Shm,
                consumer_data_types: None,
            }),
            pool: RefCell::new(Vec::new()),
            pool_mem: Rc::clone(&teardown_pool_mem),
            client: Rc::clone(&client),
            mainloop: mainloop.downgrade(),
            start_state,
            dropped_frames: Cell::new(0),
            warned_unmappable: Cell::new(false),
        })
        .state_changed(move |stream, data, _old, new| {
            log::debug!("portal: pipewire stream {new:?}");
            if let StreamState::Error(message) = new {
                if let Some(started) = data.start_state.borrow_mut().started.take() {
                    let _ = started.send(Err(format!("PipeWire stream error: {message}")));
                }
                if let Some(mainloop) = state_loop_weak.upgrade() {
                    mainloop.quit();
                }
                return;
            }
            // Paused is the first state with a valid node id. The registry
            // normally announced its object.serial already; also consult the
            // stream properties in case this PipeWire version exposes it
            // there first.
            if new == StreamState::Paused {
                let node_id = stream.node_id();
                let mut state = data.start_state.borrow_mut();
                if let Some(serial) = stream
                    .properties()
                    .get("object.serial")
                    .and_then(|serial| serial.parse::<u64>().ok())
                {
                    state.serials.insert(node_id, serial);
                }
                state.paused = Some((node_id, data.width, data.height));
                state.try_complete();
            }
            // A DRIVER stream only runs cycles when triggered, and a
            // trigger before the link is up fails with EIO. Once the stream
            // (re)reaches Streaming, republish the latest frame: early
            // frames were never delivered, and a pause flushed queued
            // buffers back to us.
            if new == StreamState::Streaming && data.latest.borrow().is_some() {
                data.pending.set(true);
                if let Err(error) = stream.trigger_process() {
                    log::debug!("portal: trigger_process failed: {error}");
                }
            }
        })
        .param_changed(|stream, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id == spa::param::ParamType::Format.as_raw() {
                match parse_format_param(param) {
                    Some(fixated) => data.apply_fixated_format(&fixated),
                    None => log::warn!("portal: could not parse the fixated PipeWire format"),
                }
                // Advertise the layout delivery actually uses: the slot's
                // stride and size for zero-copy dmabuf, tightly packed for
                // the shared-memory copy path.
                let buffers = {
                    let transport = data.transport.borrow();
                    let mode = data.negotiation.borrow().mode;
                    let slot = if mode == DeliveryMode::Dmabuf {
                        transport.slot_files.first()
                    } else {
                        None
                    };
                    match slot {
                        Some(slot) => buffers_pod(
                            transport.slot_files.len(),
                            slot.stride as i32,
                            slot.byte_len as i32,
                        ),
                        None => buffers_pod(
                            transport.slot_files.len(),
                            (data.width * 4) as i32,
                            (data.width * data.height * 4) as i32,
                        ),
                    }
                };
                let mut params = [Pod::from_bytes(&buffers).expect("buffers pod")];
                if let Err(error) = stream.update_params(&mut params) {
                    log::warn!("portal: pipewire update_params failed: {error}");
                }
            } else if id == spa::param::ParamType::Buffers.as_raw()
                && let Some(mask) = parse_buffers_data_types(param)
            {
                log::debug!("portal: consumer accepts buffer data types {mask:#05b}");
                data.negotiation.borrow_mut().consumer_data_types = Some(mask);
            }
        })
        .add_buffer(|_stream, data, buffer| {
            // With ALLOC_BUFFERS the producer fills each pool buffer's data
            // slot here, before the buffer registers with the consumer. A
            // dmabuf-negotiated slot stream binds the next compositor slot;
            // everything else gets a Portal-owned memfd for the copy path.
            let bind = if data.negotiation.borrow().forwarding_eligible() {
                let mut transport = data.transport.borrow_mut();
                transport
                    .slot_bindings
                    .iter_mut()
                    .enumerate()
                    .find(|(_, binding)| binding.pool.is_none())
                    .map(|(index, binding)| {
                        binding.pool = Some(buffer);
                        index
                    })
            } else {
                None
            };
            let Some(datas) = (unsafe {
                // SAFETY: `buffer` is a live pool buffer handed to this
                // stream by the add_buffer event; its spa_buffer is valid.
                let datas = (*(*buffer).buffer).datas;
                if datas.is_null() || (*datas).chunk.is_null() {
                    None
                } else {
                    Some(datas)
                }
            }) else {
                log::warn!("portal: pool buffer without data or chunk slots");
                return;
            };
            if let Some(index) = bind {
                let (fd, byte_len, stride) = {
                    let transport = data.transport.borrow();
                    let slot = &transport.slot_files[index];
                    (slot.file.as_raw_fd() as i64, slot.byte_len, slot.stride)
                };
                // SAFETY: `datas`/`chunk` are live pool slots patched here
                // before any use; they stay patched until remove_buffer.
                unsafe {
                    let chunk = (*datas).chunk;
                    (*datas).type_ = spa_sys::SPA_DATA_DmaBuf;
                    (*datas).flags = spa::buffer::DataFlags::READABLE.bits();
                    (*datas).fd = fd;
                    (*datas).mapoffset = 0;
                    (*datas).maxsize = byte_len as u32;
                    (*datas).data = std::ptr::null_mut();
                    (*chunk).offset = 0;
                    (*chunk).size = byte_len as u32;
                    (*chunk).stride = stride as i32;
                    (*chunk).flags = spa_sys::SPA_CHUNK_FLAG_NONE as i32;
                }
                log::debug!("portal: bound compositor slot {index} to a pool buffer");
                return;
            }
            let frame_bytes = data.width as usize * data.height as usize * 4;
            match PoolMem::new(frame_bytes) {
                Ok(mem) => {
                    // SAFETY: as above; the memfd stays owned by
                    // `pool_mem` until remove_buffer/teardown, and `map`
                    // points at its pages.
                    unsafe {
                        let chunk = (*datas).chunk;
                        (*datas).type_ = spa_sys::SPA_DATA_MemFd;
                        (*datas).flags =
                            spa_sys::SPA_DATA_FLAG_READABLE | spa_sys::SPA_DATA_FLAG_MAPPABLE;
                        (*datas).fd = mem.file.as_raw_fd() as i64;
                        (*datas).mapoffset = 0;
                        (*datas).maxsize = mem.len as u32;
                        (*datas).data = mem.map.cast();
                        (*chunk).offset = 0;
                        (*chunk).size = 0;
                        (*chunk).stride = (data.width * 4) as i32;
                        (*chunk).flags = spa_sys::SPA_CHUNK_FLAG_NONE as i32;
                    }
                    data.pool_mem.borrow_mut().insert(buffer as usize, mem);
                }
                Err(error) => {
                    log::warn!("portal: could not allocate a pool buffer: {error}");
                }
            }
        })
        .remove_buffer(|_stream, data, buffer| {
            // The pool is being torn down (renegotiation or stop): drop the
            // binding and release any in-flight slot the consumer abandoned.
            data.pool_mem.borrow_mut().remove(&(buffer as usize));
            data.pool.borrow_mut().retain(|pooled| *pooled != buffer);
            let mut transport = data.transport.borrow_mut();
            let Some((slot, binding)) = transport
                .slot_bindings
                .iter_mut()
                .enumerate()
                .find(|(_, binding)| binding.pool == Some(buffer))
            else {
                return;
            };
            binding.pool = None;
            let was_in_flight = std::mem::replace(&mut binding.in_flight, false);
            let stream_id = transport.stream_id;
            let has_slots = !transport.slot_files.is_empty();
            let client = Rc::clone(&data.client);
            drop(transport);
            if was_in_flight
                && has_slots
                && let Err(error) = client
                    .borrow_mut()
                    .release_stream_buffer(stream_id, slot as u32)
            {
                log::debug!("portal: slot release failed: {error}");
            }
        })
        .process(process_frame)
        .register()
        .map_err(|e| e.to_string())?;

    // IPC frames arrive as a loop source; the lease timer keeps the scoped
    // connection alive across quiet periods.
    let loop_weak = mainloop.downgrade();
    let stream_weak = stream.downgrade();
    let compositor_dropped = Rc::new(Cell::new(0_u64));
    let _ipc_source = mainloop.loop_().add_io(
        IpcFd(Rc::clone(&client)),
        spa::support::system::IoFlags::IN | spa::support::system::IoFlags::ERR,
        {
            let jobs = jobs.clone();
            let session_path = session_path.to_string();
            let compositor_dropped = Rc::clone(&compositor_dropped);
            let transport = Rc::clone(&transport);
            move |io| {
                let message = io.0.borrow_mut().next_stream_message();
                match message {
                    Ok(StreamMessage::Frame(frame)) => {
                        if frame.dropped != compositor_dropped.get() {
                            log::debug!(
                                "portal: compositor reports {} backpressure-dropped stream frames",
                                frame.dropped
                            );
                            compositor_dropped.set(frame.dropped);
                        }
                        let state = transport.borrow();
                        if frame.stream_id != state.stream_id {
                            // A transport switch left frames of the
                            // superseded stream on the wire.
                            log::debug!(
                                "portal: ignoring frame for superseded stream {}",
                                frame.stream_id
                            );
                            return;
                        }
                        match validate_frame(
                            frame,
                            width,
                            height,
                            state.announced,
                            state.slot_count(),
                        ) {
                            Ok(payload) => {
                                drop(state);
                                *latest.borrow_mut() = Some(payload);
                                pending.set(true);
                                if let Some(stream) = stream_weak.upgrade()
                                    && let Err(error) = stream.trigger_process()
                                {
                                    log::debug!("portal: trigger_process failed: {error}");
                                }
                            }
                            Err(reason) => {
                                log::debug!("portal: ignoring stream frame: {reason}");
                            }
                        }
                    }
                    Ok(StreamMessage::LeaseRenewed) => {}
                    Ok(StreamMessage::Ended { reason, .. }) => {
                        log::info!("portal: compositor ended stream for {session_path}: {reason}");
                        end_cast(&jobs, &session_path, &loop_weak);
                    }
                    Err(error) => {
                        log::warn!("portal: stream read for {session_path} failed: {error}");
                        end_cast(&jobs, &session_path, &loop_weak);
                    }
                }
            }
        },
    );

    let stopped_by_owner = Rc::new(Cell::new(false));
    let stop_flag = Rc::clone(&stopped_by_owner);
    let _stop_source = mainloop.loop_().add_io(
        stop_read,
        spa::support::system::IoFlags::IN | spa::support::system::IoFlags::HUP,
        {
            let loop_weak = mainloop.downgrade();
            move |socket| {
                stop_flag.set(true);
                let mut byte = [0u8; 1];
                use std::io::Read;
                // EOF (write end dropped) is the only expected readability.
                let _ = socket.read(&mut byte);
                if let Some(mainloop) = loop_weak.upgrade() {
                    mainloop.quit();
                }
            }
        },
    );

    let lease_timer = mainloop.loop_().add_timer(move |_| {
        if let Err(error) = client.borrow_mut().request_lease_renewal(LEASE_TTL_MS) {
            log::warn!("portal: cast lease renewal failed: {error}");
        }
    });
    let half_ttl = Duration::from_millis(LEASE_TTL_MS / 2);
    lease_timer.update_timer(Some(half_ttl), Some(half_ttl));

    let mut format_refs: Vec<&Pod> = format_bytes
        .iter()
        .map(|bytes| Pod::from_bytes(bytes).expect("format pod"))
        .collect();
    stream
        .connect(
            // This stream publishes compositor frames. `Input` describes a
            // capture consumer (for example a camera reader) and leaves OBS
            // with no producer port to link to; a screencast source must
            // expose an output port.
            STREAM_DIRECTION,
            None,
            // Portal consumers explicitly target the node returned by
            // Start/OpenPipeWireRemote. AUTOCONNECT asks session-manager
            // policy to route this source to an unrelated default target and
            // can tear the stream down with "no target node available".
            // ALLOC_BUFFERS lets the producer attach each pool buffer's
            // memory in the add_buffer event (the only way a consumer ever
            // sees a dmabuf descriptor: buffer contents are fixed at
            // registration), and DRIVER pairs it with trigger_process so
            // cycles run only when a compositor frame arrives.
            StreamFlags::MAP_BUFFERS | StreamFlags::ALLOC_BUFFERS | StreamFlags::DRIVER,
            &mut format_refs,
        )
        .map_err(|e| e.to_string())?;

    mainloop.run();
    // Teardown: unmap pool backing, then release slots the consumer still
    // holds so the compositor can reuse them without waiting for the
    // disconnect cleanup.
    teardown_pool_mem.borrow_mut().clear();
    let transport_state = teardown_transport.borrow();
    for (slot, binding) in transport_state.slot_bindings.iter().enumerate() {
        if binding.in_flight
            && let Err(error) = teardown_client
                .borrow_mut()
                .release_stream_buffer(transport_state.stream_id, slot as u32)
        {
            log::debug!("portal: teardown slot release failed: {error}");
        }
    }
    drop(transport_state);
    if exit_start_state.borrow().completed && !stopped_by_owner.get() {
        let _ = jobs.send(CastJob::SessionEnded {
            session_path: session_path.to_owned(),
        });
    }
    Ok(())
}

impl StreamData {
    /// Record the fixated format: verify it against what was offered,
    /// switch the compositor transport when the current one cannot serve
    /// it, and derive the delivery mode. Consumers can renegotiate
    /// mid-stream (OBS removes an unimportable modifier and retries), so
    /// this runs on every `Format` param change.
    fn apply_fixated_format(&self, fixated: &FixatedFormat) {
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

/// The fixated `SPA_PARAM_Format`, reduced to what delivery needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FixatedFormat {
    spa_format: u32,
    width: u32,
    height: u32,
    modifier: Option<u64>,
}

/// The default of a SPA choice; fixation may leave any choice kind behind.
fn choice_default<T: Copy + pod::CanonicalFixedSizedPod>(choice: &Choice<T>) -> T {
    match &choice.1 {
        ChoiceEnum::None(value) => *value,
        ChoiceEnum::Range { default, .. } => *default,
        ChoiceEnum::Step { default, .. } => *default,
        ChoiceEnum::Enum { default, .. } => *default,
        ChoiceEnum::Flags { default, .. } => *default,
    }
}

fn pod_value_id(value: &pod::Value) -> Option<u32> {
    match value {
        pod::Value::Id(id) => Some(id.0),
        pod::Value::Choice(pod::ChoiceValue::Id(choice)) => Some(choice_default(choice).0),
        _ => None,
    }
}

fn pod_value_rectangle(value: &pod::Value) -> Option<Rectangle> {
    match value {
        pod::Value::Rectangle(rectangle) => Some(*rectangle),
        pod::Value::Choice(pod::ChoiceValue::Rectangle(choice)) => Some(choice_default(choice)),
        _ => None,
    }
}

/// Parse a fixated `SPA_PARAM_Format` raw-video pod, tolerating both plain
/// and choice-wrapped property values.
fn parse_format_param(param: &Pod) -> Option<FixatedFormat> {
    let value = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(param.as_bytes())
        .ok()?
        .1;
    let pod::Value::Object(object) = value else {
        return None;
    };
    let mut format = None;
    let mut size = None;
    let mut modifier = None;
    for property in &object.properties {
        if property.key == spa::param::format::FormatProperties::VideoFormat.as_raw() {
            format = pod_value_id(&property.value);
        } else if property.key == spa::param::format::FormatProperties::VideoSize.as_raw() {
            size = pod_value_rectangle(&property.value);
        } else if property.key == spa::param::format::FormatProperties::VideoModifier.as_raw() {
            modifier = pod_value_long(&property.value).map(|raw| raw as u64);
        }
    }
    Some(FixatedFormat {
        spa_format: format?,
        width: size?.width,
        height: size?.height,
        modifier,
    })
}

fn pod_value_long(value: &pod::Value) -> Option<i64> {
    match value {
        pod::Value::Long(long) => Some(*long),
        pod::Value::Choice(pod::ChoiceValue::Long(choice)) => Some(choice_default(choice)),
        _ => None,
    }
}

/// Extract the accepted `SPA_PARAM_BUFFERS_dataType` mask from a fixated
/// Buffers param.
fn parse_buffers_data_types(param: &Pod) -> Option<u32> {
    let value = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(param.as_bytes())
        .ok()?
        .1;
    let pod::Value::Object(object) = value else {
        return None;
    };
    for property in &object.properties {
        if property.key != 6 {
            // SPA_PARAM_BUFFERS_dataType
            continue;
        }
        return match &property.value {
            pod::Value::Int(mask) => Some(*mask as u32),
            pod::Value::Choice(pod::ChoiceValue::Int(choice)) => {
                Some(choice_default(choice) as u32)
            }
            _ => None,
        };
    }
    None
}

fn frame_len(width: u32, height: u32) -> Result<usize, String> {
    let bytes = usize::try_from(width)
        .ok()
        .and_then(|width| width.checked_mul(height as usize))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| format!("invalid compositor stream geometry {width}x{height}"))?;
    if width == 0 || height == 0 || bytes > MAX_FRAME_BYTES {
        return Err(format!(
            "compositor stream geometry {width}x{height} exceeds the 256 MiB frame limit"
        ));
    }
    Ok(bytes)
}

/// Report a compositor-side stream end to the screencast worker and stop
/// the cast thread's loop.
fn end_cast(
    jobs: &mpsc::SyncSender<CastJob>,
    session_path: &str,
    loop_weak: &pw::main_loop::MainLoopWeak,
) {
    let _ = jobs.send(CastJob::SessionEnded {
        session_path: session_path.to_string(),
    });
    if let Some(mainloop) = loop_weak.upgrade() {
        mainloop.quit();
    }
}

/// The offered video format: raw video in the SPA format matching the
/// compositor's pixel layout at the output's geometry. The framerate is a
/// range because the compositor is damage-driven: frames arrive when the
/// screen changes, bounded by its vertical sync, and each consumer picks
/// the rate its pipeline wants.
fn format_pod(
    width: u32,
    height: u32,
    spa_format: spa::param::video::VideoFormat,
    modifier: Option<u64>,
) -> Vec<u8> {
    let mut properties = vec![
        pod::Property {
            key: spa::param::format::FormatProperties::MediaType.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(
                spa::param::format::MediaType::Video.as_raw(),
            )),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::MediaSubtype.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(
                spa::param::format::MediaSubtype::Raw.as_raw(),
            )),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::VideoFormat.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Id(spa::utils::Id(spa_format.as_raw())),
        },
    ];
    if let Some(modifier) = modifier {
        // The Long choice Enum carries exactly one modifier: the one the
        // compositor's slots have. `property!`'s Choice arms do not compile
        // for Long, so the property is built by hand. MANDATORY keeps
        // modifier-ignorant consumers off this entry: they fixate the plain
        // entry instead of fixating a dmabuf format they cannot serve.
        properties.push(pod::Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: pod::PropertyFlags::MANDATORY,
            value: pod::Value::Choice(pod::ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifier as i64,
                    alternatives: Vec::new(),
                },
            ))),
        });
    }
    properties.extend([
        pod::Property {
            key: spa::param::format::FormatProperties::VideoSize.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Rectangle(Rectangle { width, height }),
        },
        pod::Property {
            key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Fraction(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Range {
                    default: FRAMERATE_DEFAULT,
                    min: FRAMERATE_MIN,
                    max: FRAMERATE_MAX,
                },
            ))),
        },
    ]);
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    };
    serialize(&pod::Value::Object(object))
}

/// The format set offered at connect time. A slot stream offers its
/// modifier entry first (preferred by GPU consumers) and an equivalent
/// plain entry as the universal fallback; everything else offers only the
/// plain entry, because per-frame descriptors cannot populate DmaBuf pool
/// buffers (see the module docs).
fn format_pods(
    width: u32,
    height: u32,
    announced: AnnouncedFormat,
    has_slots: bool,
) -> Vec<Vec<u8>> {
    let mut pods = Vec::new();
    if let AnnouncedFormat::Dmabuf {
        spa_format,
        modifier,
        ..
    } = announced
        && has_slots
    {
        pods.push(format_pod(width, height, spa_format, Some(modifier)));
        pods.push(format_pod(width, height, spa_format, None));
        return pods;
    }
    pods.push(format_pod(width, height, announced.spa_format(), None));
    pods
}

/// Buffer constraints offered once the format is negotiated: buffers of
/// exactly one frame at the layout delivery actually uses (the slot's
/// stride and size for zero-copy dmabuf, tightly packed for the copy
/// path), defaulting to the slot count on slot streams.
fn buffers_pod(slots: usize, stride: i32, size: i32) -> Vec<u8> {
    let default = u32::try_from(slots).unwrap_or(0).clamp(2, 8);
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            pod::Property {
                key: 1, // SPA_PARAM_BUFFERS_buffers
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: default as i32,
                        min: 2,
                        max: 8,
                    },
                ))),
            },
            pod::Property {
                key: 2, // SPA_PARAM_BUFFERS_blocks
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(1),
            },
            pod::Property {
                key: 3, // SPA_PARAM_BUFFERS_size
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(size),
            },
            pod::Property {
                key: 4, // SPA_PARAM_BUFFERS_stride
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(stride),
            },
            pod::Property {
                key: 6, // SPA_PARAM_BUFFERS_dataType
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Flags {
                        default: ALL_DATA_TYPES as i32,
                        flags: Vec::new(),
                    },
                ))),
            },
        ],
    };
    serialize(&pod::Value::Object(object))
}

fn serialize(value: &pod::Value) -> Vec<u8> {
    pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .expect("pod serialization")
        .0
        .into_inner()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::os::fd::FromRawFd;

    fn pod_words(bytes: &[u8]) -> &[u32] {
        let (head, words, tail) = unsafe { bytes.align_to::<u32>() };
        assert!(head.is_empty() && tail.is_empty());
        words
    }

    fn parse_pod(bytes: &[u8]) -> pod::Object {
        let parsed = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(bytes)
            .expect("deserialize")
            .1;
        let pod::Value::Object(object) = parsed else {
            panic!("expected an object pod");
        };
        object
    }

    fn unsealed_memfd(bytes: &[u8]) -> File {
        // SAFETY: the name is static and NUL-terminated; the returned fd is
        // checked before ownership is constructed.
        let fd = unsafe { libc::memfd_create(c"aegis-cast-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(fd >= 0, "memfd_create: {}", io::Error::last_os_error());
        // SAFETY: `fd` is a new owned descriptor from memfd_create.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes).unwrap();
        file
    }

    fn frame_for_test(format: aegis_portal_ipc::StreamPixelFormat) -> StreamFrame {
        StreamFrame {
            stream_id: 1,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format,
            damage: Vec::new(),
            dropped: 0,
            slot: None,
            payload: StreamPayload::Memfd(unsealed_memfd(&[0; 16])),
        }
    }

    #[test]
    fn screencast_stream_is_a_pipewire_producer() {
        assert!(matches!(STREAM_DIRECTION, Direction::Output));
    }

    #[test]
    fn drm_fourccs_map_to_spa_formats_in_memory_order() {
        use spa::param::video::VideoFormat;
        let cases = [
            (fourcc(b'X', b'R', b'2', b'4'), VideoFormat::BGRx),
            (fourcc(b'A', b'R', b'2', b'4'), VideoFormat::BGRA),
            (fourcc(b'X', b'B', b'2', b'4'), VideoFormat::RGBx),
            (fourcc(b'A', b'B', b'2', b'4'), VideoFormat::RGBA),
            (fourcc(b'R', b'X', b'2', b'4'), VideoFormat::xBGR),
            (fourcc(b'R', b'A', b'2', b'4'), VideoFormat::ABGR),
            (fourcc(b'B', b'X', b'2', b'4'), VideoFormat::xRGB),
            (fourcc(b'B', b'A', b'2', b'4'), VideoFormat::ARGB),
        ];
        for (drm, spa_format) in cases {
            assert_eq!(spa_format_for_drm(drm), Some(spa_format), "{drm:#010x}");
        }
        assert_eq!(spa_format_for_drm(fourcc(b'N', b'V', b'1', b'2')), None);
    }

    #[test]
    fn format_pod_is_a_parseable_video_format_with_a_framerate_range() {
        let bytes = format_pod(1920, 1080, spa::param::video::VideoFormat::BGRx, None);
        assert!(Pod::from_bytes(&bytes).is_some(), "pod parses");
        let words = pod_words(&bytes);
        // Pod header: body size, then SPA_TYPE_Object; the object body
        // carries type (ObjectParamFormat) and id (EnumFormat).
        assert_eq!(words[0] as usize, bytes.len() - 8);
        assert_eq!(words[2], spa::utils::SpaTypes::ObjectParamFormat.as_raw());
        assert_eq!(words[3], spa::param::ParamType::EnumFormat.as_raw());
        let object = parse_pod(&bytes);
        assert_eq!(object.properties.len(), 5);
        // The framerate is a range so each consumer paces against its own
        // clock; frames arrive at the compositor's actual cadence.
        let framerate = object
            .properties
            .iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoFramerate.as_raw())
            .and_then(|p| match &p.value {
                pod::Value::Choice(pod::ChoiceValue::Fraction(choice)) => Some(choice),
                _ => None,
            })
            .expect("framerate property");
        assert_eq!(choice_default(framerate), FRAMERATE_DEFAULT);
        let Choice(_, ChoiceEnum::Range { min, max, .. }) = framerate else {
            panic!("framerate must be a range");
        };
        assert_eq!(*min, FRAMERATE_MIN);
        assert_eq!(*max, FRAMERATE_MAX);
    }

    #[test]
    fn buffers_pod_covers_one_frame_and_every_data_type() {
        let bytes = buffers_pod(0, 640 * 4, 640 * 480 * 4);
        let object = parse_pod(&bytes);
        assert_eq!(
            object.type_,
            spa::utils::SpaTypes::ObjectParamBuffers.as_raw()
        );
        let size = object
            .properties
            .iter()
            .find(|p| p.key == 3)
            .and_then(|p| match p.value {
                pod::Value::Int(size) => Some(size),
                _ => None,
            })
            .expect("size property");
        assert_eq!(size, 640 * 480 * 4);
        let data_types = object
            .properties
            .iter()
            .find(|property| property.key == 6)
            .and_then(|property| match &property.value {
                pod::Value::Choice(pod::ChoiceValue::Int(Choice(
                    _,
                    ChoiceEnum::Flags { default, flags },
                ))) => Some((*default, flags.as_slice())),
                _ => None,
            })
            .expect("dataType flags property");
        assert_eq!(data_types, (ALL_DATA_TYPES as i32, &[][..]));
    }

    #[test]
    fn format_param_parsing_tolerates_plain_and_choice_values() {
        let fixated = parse_format_param(
            Pod::from_bytes(&format_pod(
                640,
                480,
                spa::param::video::VideoFormat::BGRx,
                None,
            ))
            .expect("pod"),
        )
        .expect("parseable format pod");
        assert_eq!(
            fixated,
            FixatedFormat {
                spa_format: spa::param::video::VideoFormat::BGRx.as_raw(),
                width: 640,
                height: 480,
                modifier: None,
            }
        );
    }

    #[test]
    fn shm_frames_validate_against_the_announced_geometry() {
        let announced = announced_format(aegis_portal_ipc::StreamPixelFormat::Bgra8).unwrap();
        let frame = frame_for_test(aegis_portal_ipc::StreamPixelFormat::Bgra8);
        assert!(validate_frame(frame, 2, 2, announced, 0).is_ok());

        let wrong_geometry = StreamFrame {
            width: 4,
            payload: StreamPayload::Memfd(unsealed_memfd(&[0; 32])),
            ..frame_for_test(aegis_portal_ipc::StreamPixelFormat::Bgra8)
        };
        assert!(validate_frame(wrong_geometry, 2, 2, announced, 0).is_err());
    }

    #[test]
    fn dmabuf_frames_allow_padded_strides_but_pin_the_modifier() {
        let announced = announced_format(aegis_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: DRM_FORMAT_MOD_LINEAR,
        })
        .unwrap();
        let frame = StreamFrame {
            stride: 16, // padded: 2 pixels wide would only need 8
            payload: StreamPayload::Dmabuf(unsealed_memfd(&[0; 32])),
            ..frame_for_test(aegis_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: fourcc(b'X', b'R', b'2', b'4'),
                modifier: DRM_FORMAT_MOD_LINEAR,
            })
        };
        assert!(matches!(
            validate_frame(frame, 2, 2, announced, 0),
            Ok(FramePayload::Descriptor { stride: 16, .. })
        ));

        let wrong_modifier = frame_for_test(aegis_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: 4,
        });
        assert!(validate_frame(wrong_modifier, 2, 2, announced, 0).is_err());
    }

    /// The copy path memory-maps the frame descriptor, so a tiled dmabuf
    /// can never enter it: mapping it would copy tile-swizzled bytes into
    /// the consumer's buffers. Tiled streams are served by the
    /// compositor's SHM readback transport instead.
    #[test]
    fn non_linear_dmabuf_frames_never_enter_the_copy_path() {
        let announced = announced_format(aegis_portal_ipc::StreamPixelFormat::Dmabuf {
            drm_format: fourcc(b'X', b'R', b'2', b'4'),
            modifier: 0x0100_0000_0000_0001, // DRM_FORMAT_MOD_I915_X_TILED
        })
        .unwrap();
        let frame = StreamFrame {
            stride: 16,
            payload: StreamPayload::Dmabuf(unsealed_memfd(&[0; 32])),
            ..frame_for_test(aegis_portal_ipc::StreamPixelFormat::Dmabuf {
                drm_format: fourcc(b'X', b'R', b'2', b'4'),
                modifier: 0x0100_0000_0000_0001,
            })
        };
        let Err(reason) = validate_frame(frame, 2, 2, announced, 0) else {
            panic!("a tiled dmabuf frame must not validate for the copy path");
        };
        assert!(reason.contains("non-LINEAR"), "{reason}");
    }

    #[test]
    fn copy_rows_handles_tight_and_padded_strides() {
        let src: Vec<u8> = (0..64).collect();
        let mut dest = vec![0_u8; 16];
        copy_rows(&src, 8, &mut dest, 8, 2);
        assert_eq!(dest, src[..16]);

        let mut dest = vec![0_u8; 8];
        copy_rows(&src, 16, &mut dest, 4, 2);
        assert_eq!(dest, [0, 1, 2, 3, 16, 17, 18, 19]);
    }
}
