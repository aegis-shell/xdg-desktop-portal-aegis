//! One PipeWire producer stream per started ScreenCast session.
//!
//! Each cast runs on its own thread: a scoped IPC connection receives the
//! compositor's output-frame stream and a PipeWire `Output`
//! stream republishes every new frame as raw `BGRx` video at a fixed
//! framerate. The PipeWire main loop is also the IPC event loop — the IPC
//! socket, the stop socket, and the lease-renewal timer are ordinary loop
//! sources, so the thread never blocks anywhere but in `poll`.
//!
//! The portal is producer-driven: when a compositor frame arrives the IPC
//! source stores it, marks it pending, and calls `pw_stream_trigger_process`
//! so PipeWire pulls exactly that frame on its next cycle. Stale frames are
//! not copied again, which prevents duplicate frames from confusing the
//! consumer's pacing and causing stutter or latency.
//!
//! Teardown is single-path: closing the write end of the stop socket (or a
//! compositor-side `StreamEnded`, or any read error) quits the loop, after
//! which dropping the IPC client disconnects it — and the compositor's
//! disconnect cleanup stops the stream with no extra round-trip.

use std::cell::RefCell;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use aegis_portal_ipc::{Client, StreamMessage};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use pw::spa::pod::{self, Pod};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle};
use pw::stream::{StreamFlags, StreamState};

use crate::ipc;
use crate::screencast::CastJob;

/// Frame rate requested from the compositor and offered to PipeWire.
const STREAM_FPS: u32 = 30;
/// A screencast publishes frames for PipeWire capture consumers.
const STREAM_DIRECTION: Direction = Direction::Output;
/// Lease TTL requested at handshake and renewal; renewed at half TTL.
const LEASE_TTL_MS: u64 = 900_000;
const IPC_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

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

/// Latest frame shared between the IPC source (writer) and the PipeWire
/// `process` callback (reader). `None` until the first frame arrives.
type LatestFrame = Rc<RefCell<Option<Rc<Vec<u8>>>>>;

/// Stream-listener user data.
struct StreamData {
    latest: LatestFrame,
    width: u32,
    height: u32,
    start_state: Rc<RefCell<StartState>>,
    /// Set when a new IPC frame has arrived but not yet been pushed to
    /// PipeWire. Cleared by the `process` callback after copying the frame.
    pending: Rc<std::cell::Cell<bool>>,
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
        .start_output_stream_target(Some(STREAM_FPS), aegis_portal_ipc::StreamTarget::Output)
        .map_err(|e| format!("start output stream: {e}"))?;
    let (width, height) = (stream_info.width, stream_info.height);
    let expected_frame_len = frame_len(width, height)?;
    if stream_info.format != aegis_portal_ipc::StreamPixelFormat::Bgra8 {
        return Err(format!(
            "unsupported compositor stream format: {:?}",
            stream_info.format
        ));
    }
    log::info!(
        "portal: compositor stream {} for {session_path}: {width}x{height}@{STREAM_FPS}",
        stream_info.stream_id
    );
    let client = Rc::new(RefCell::new(client));

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
        properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let latest: LatestFrame = Rc::new(RefCell::new(None));
    let format_pod = format_pod(width, height);
    let pending = Rc::new(std::cell::Cell::new(false));
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
            width,
            height,
            start_state,
            pending: Rc::clone(&pending),
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
        })
        .param_changed(|stream, data, id, param| {
            if id == spa::param::ParamType::Format.as_raw() && param.is_some() {
                let buffers = buffers_pod(data.width, data.height);
                let mut params = [Pod::from_bytes(&buffers).expect("buffers pod")];
                if let Err(error) = stream.update_params(&mut params) {
                    log::warn!("portal: pipewire update_params failed: {error}");
                }
            }
        })
        .process(|_stream, data| {
            let Some(mut buffer) = _stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data_ref = &mut datas[0];
            let stride = data.width as usize * 4;
            // Only publish frames the compositor has produced since the last
            // process cycle. Copying stale frames makes OBS/GStreamer see
            // duplicate frames and can cause the consumer's pacing logic to
            // stall or misreport the real rate.
            let size = if data.pending.replace(false) {
                let frame = data.latest.borrow();
                match frame.as_ref() {
                    Some(frame) => match data_ref.data() {
                        Some(dest) if dest.len() >= frame.len() => {
                            dest[..frame.len()].copy_from_slice(frame);
                            frame.len()
                        }
                        _ => 0,
                    },
                    None => 0,
                }
            } else {
                0
            };
            let chunk = data_ref.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.size_mut() = size as u32;
            *chunk.stride_mut() = stride as i32;
        })
        .register()
        .map_err(|e| e.to_string())?;

    // IPC frames arrive as a loop source; the lease timer keeps the scoped
    // connection alive across quiet periods.
    let loop_weak = mainloop.downgrade();
    let stream_weak = stream.downgrade();
    let _ipc_source = mainloop.loop_().add_io(
        IpcFd(Rc::clone(&client)),
        spa::support::system::IoFlags::IN | spa::support::system::IoFlags::ERR,
        {
            let jobs = jobs.clone();
            let session_path = session_path.to_string();
            move |io| {
                let message = io.0.borrow_mut().next_stream_message();
                match message {
                    Ok(StreamMessage::Frame(frame)) => match frame.format {
                        aegis_portal_ipc::StreamPixelFormat::Bgra8 => {
                            if frame.width == width
                                && frame.height == height
                                && frame.stride == width * 4
                                && frame.pixels.len() == expected_frame_len
                            {
                                *latest.borrow_mut() = Some(Rc::new(frame.pixels));
                                pending.set(true);
                                if let Some(stream) = stream_weak.upgrade()
                                    && let Err(error) = stream.trigger_process()
                                {
                                    log::debug!("portal: trigger_process failed: {error}");
                                }
                            }
                        }
                        other => {
                            log::warn!("portal: ignoring unexpected stream frame format {other:?}");
                        }
                    },
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

    let stopped_by_owner = Rc::new(std::cell::Cell::new(false));
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

    let mut params = [Pod::from_bytes(&format_pod).expect("format pod")];
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
            StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| e.to_string())?;

    mainloop.run();
    if exit_start_state.borrow().completed && !stopped_by_owner.get() {
        let _ = jobs.send(CastJob::SessionEnded {
            session_path: session_path.to_owned(),
        });
    }
    Ok(())
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

/// Offered video format: raw BGRx at the output's geometry and a fixed
/// framerate matching the compositor stream. The compositor's stream format
/// is BGRA with opaque alpha, which is exactly `BGRx` semantics.
fn format_pod(width: u32, height: u32) -> Vec<u8> {
    let object = pod::object! {
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        pod::property!(
            spa::param::format::FormatProperties::MediaType,
            Id,
            spa::param::format::MediaType::Video
        ),
        pod::property!(
            spa::param::format::FormatProperties::MediaSubtype,
            Id,
            spa::param::format::MediaSubtype::Raw
        ),
        pod::property!(
            spa::param::format::FormatProperties::VideoFormat,
            Id,
            spa::param::video::VideoFormat::BGRx
        ),
        pod::property!(
            spa::param::format::FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        pod::property!(
            spa::param::format::FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: STREAM_FPS, denom: 1 }
        ),
    };
    serialize(&pod::Value::Object(object))
}

/// Buffer constraints offered once the format is negotiated: 2–8 shared or
/// plain buffers of exactly one frame. `SPA_PARAM_BUFFERS_dataType` is a mask
/// of enum positions, not the enum values themselves: MemPtr is bit 1 and
/// MemFd is bit 2, hence `(1 << 1) | (1 << 2) == 6`.
fn buffers_pod(width: u32, height: u32) -> Vec<u8> {
    let stride = width as i32 * 4;
    let size = stride * height as i32;
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
                        default: 4,
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
                        default: (1 << 1) | (1 << 2),
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

    fn pod_words(bytes: &[u8]) -> &[u32] {
        let (head, words, tail) = unsafe { bytes.align_to::<u32>() };
        assert!(head.is_empty() && tail.is_empty());
        words
    }

    #[test]
    fn screencast_stream_is_a_pipewire_producer() {
        assert!(matches!(STREAM_DIRECTION, Direction::Output));
    }

    #[test]
    fn format_pod_is_a_parseable_video_format() {
        let bytes = format_pod(1920, 1080);
        assert!(Pod::from_bytes(&bytes).is_some(), "pod parses");
        let words = pod_words(&bytes);
        // Pod header: body size, then SPA_TYPE_Object; the object body
        // carries type (ObjectParamFormat) and id (EnumFormat).
        assert_eq!(words[0] as usize, bytes.len() - 8);
        assert_eq!(words[2], spa::utils::SpaTypes::ObjectParamFormat.as_raw());
        assert_eq!(words[3], spa::param::ParamType::EnumFormat.as_raw());
        // Round-trip through the deserializer to prove the shape is valid.
        let parsed = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(&bytes)
            .expect("deserialize")
            .1;
        let pod::Value::Object(object) = parsed else {
            panic!("expected an object pod");
        };
        assert_eq!(object.properties.len(), 5);
        // The advertised framerate must be fixed and match the compositor
        // stream so consumers like OBS/GStreamer can pace correctly instead of
        // guessing from a variable 0/1 rate.
        let framerate = object
            .properties
            .iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoFramerate.as_raw())
            .and_then(|p| match p.value {
                pod::Value::Fraction(fraction) => Some(fraction),
                _ => None,
            })
            .expect("framerate property");
        assert_eq!(framerate.num, STREAM_FPS);
        assert_eq!(framerate.denom, 1);
    }

    #[test]
    fn buffers_pod_covers_one_frame() {
        let bytes = buffers_pod(640, 480);
        let parsed = pod::deserialize::PodDeserializer::deserialize_from::<pod::Value>(&bytes)
            .expect("deserialize")
            .1;
        let pod::Value::Object(object) = parsed else {
            panic!("expected an object pod");
        };
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
        assert_eq!(data_types, (6, &[][..]));
    }
}
