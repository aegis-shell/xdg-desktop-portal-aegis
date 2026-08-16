//! A minimal PipeWire capture consumer for screencast E2E tests. Unlike a
//! GStreamer pipeline, this consumer controls its format/buffer offers
//! exactly, so tests can pin the two delivery modes: zero-copy dmabuf
//! forwarding and the shared-memory fallback. It connects over an explicit
//! socket fd so no environment variables or global state are involved.

use std::cell::RefCell;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use pipewire as pw;
use pw::spa;
use pw::spa::pod::{self, Pod};
use pw::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle};
use pw::stream::{StreamFlags, StreamState};

/// What one received frame looked like at the buffer level.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
    /// A producer-owned `SPA_DATA_DmaBuf` buffer (zero-copy forwarding).
    DmaBuf(Vec<u8>),
    /// A shared-pool buffer (`MemFd`/`MemPtr`) the producer copied into.
    SharedMem(Vec<u8>),
}

/// One damage rect read from a buffer's `SPA_META_VideoDamage`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DamageRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

struct ConsumerData {
    result: Rc<RefCell<Option<Result<Received, String>>>>,
    loop_weak: pw::main_loop::MainLoopWeak,
}

fn finish(data: &ConsumerData, result: Result<Received, String>) {
    let mut slot = data.result.borrow_mut();
    if slot.is_none() {
        *slot = Some(result);
        if let Some(mainloop) = data.loop_weak.upgrade() {
            mainloop.quit();
        }
    }
}

/// Connect to the PipeWire daemon listening on `socket`, subscribe to
/// `node_id`, and receive exactly one frame. `offer_dmabuf` controls the
/// format offer: with it, the consumer enumerates a modifier (asking for
/// zero-copy delivery); without it, only plain shared memory. `ready` fires
/// once the stream reaches `Streaming` (linking and negotiation complete).
pub fn consume_one_frame(
    socket: &Path,
    node_id: u32,
    width: u32,
    height: u32,
    offer_dmabuf: bool,
    ready: std::sync::mpsc::Sender<()>,
    timeout: Duration,
) -> Result<Received, String> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pw::init);
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let socket = UnixStream::connect(socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    let core = context
        .connect_fd_rc(std::os::fd::OwnedFd::from(socket), None)
        .map_err(|e| e.to_string())?;
    let stream = pw::stream::StreamRc::new(
        core,
        "aegis-portal-test-consumer",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let result: Rc<RefCell<Option<Result<Received, String>>>> = Rc::new(RefCell::new(None));
    let _listener = stream
        .add_local_listener_with_user_data(ConsumerData {
            result: Rc::clone(&result),
            loop_weak: mainloop.downgrade(),
        })
        .state_changed(move |_stream, data, _old, new| {
            if new == StreamState::Streaming {
                let _ = ready.send(());
            }
            if let StreamState::Error(message) = new {
                finish(data, Err(format!("stream error: {message}")));
            }
        })
        .param_changed(move |stream, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            // The format is fixated; state the buffer types accepted.
            let mask: u32 = if offer_dmabuf {
                (1 << 2) | (1 << 3) // MemFd | DmaBuf
            } else {
                (1 << 1) | (1 << 2) // MemPtr | MemFd
            };
            let buffers = buffers_pod(mask);
            let mut params = [Pod::from_bytes(&buffers).expect("buffers pod")];
            if let Err(error) = stream.update_params(&mut params) {
                finish(data, Err(format!("update_params: {error}")));
            }
        })
        .process(|stream, data| {
            while let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    continue;
                }
                let data_ref = &mut datas[0];
                let size = data_ref.chunk().size() as usize;
                if size == 0 {
                    continue;
                }
                let received = if data_ref.type_() == spa::buffer::DataType::DmaBuf {
                    read_dmabuf(data_ref, size)
                } else {
                    let Some(slice) = data_ref.data() else {
                        finish(data, Err("shared buffer has no mapped data".into()));
                        return;
                    };
                    if slice.len() < size {
                        finish(data, Err("shared buffer is smaller than its chunk".into()));
                        return;
                    }
                    Ok(Received::SharedMem(slice[..size].to_vec()))
                };
                finish(data, received);
                return;
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    let timeout_loop_weak = mainloop.downgrade();
    let timeout_result = Rc::clone(&result);
    let timeout_timer = mainloop.loop_().add_timer(move |_| {
        let mut slot = timeout_result.borrow_mut();
        if slot.is_none() {
            *slot = Some(Err("timed out waiting for a frame".into()));
            if let Some(mainloop) = timeout_loop_weak.upgrade() {
                mainloop.quit();
            }
        }
    });
    timeout_timer.update_timer(Some(timeout), None);

    let format_bytes = format_pod(width, height, offer_dmabuf);
    let mut format_refs = [Pod::from_bytes(&format_bytes).expect("format pod")];
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut format_refs,
        )
        .map_err(|e| e.to_string())?;

    mainloop.run();
    let result = result.borrow_mut().take();
    result.expect("the main loop only quits with a result")
}

/// Connect like [`consume_one_frame`] and additionally report the received
/// buffer's `SPA_META_VideoDamage` regions (empty when no meta block was
/// attached).
pub fn consume_one_frame_damage(
    socket: &Path,
    node_id: u32,
    width: u32,
    height: u32,
    ready: std::sync::mpsc::Sender<()>,
    timeout: Duration,
) -> Result<(Received, Vec<DamageRect>), String> {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pw::init);
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let socket = UnixStream::connect(socket).map_err(|e| format!("connect {socket:?}: {e}"))?;
    let core = context
        .connect_fd_rc(std::os::fd::OwnedFd::from(socket), None)
        .map_err(|e| e.to_string())?;
    let stream = pw::stream::StreamRc::new(
        core,
        "aegis-portal-test-consumer",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| e.to_string())?;

    let result: DamageResult = Rc::new(RefCell::new(None));
    let _listener = stream
        .add_local_listener_with_user_data(DamageConsumerData {
            result: Rc::clone(&result),
            loop_weak: mainloop.downgrade(),
        })
        .state_changed(move |_stream, data, _old, new| {
            if new == StreamState::Streaming {
                let _ = ready.send(());
            }
            if let StreamState::Error(message) = new {
                finish_damage(data, Err(format!("stream error: {message}")));
            }
        })
        .param_changed(move |stream, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() || param.is_none() {
                return;
            }
            // The format is fixated; state the buffer types accepted and,
            // like OBS's PipeWire source, request VideoDamage metadata.
            let mask: u32 = (1 << 1) | (1 << 2); // MemPtr | MemFd
            let buffers = buffers_pod(mask);
            let meta = damage_meta_pod();
            let mut params = [
                Pod::from_bytes(&buffers).expect("buffers pod"),
                Pod::from_bytes(&meta).expect("meta pod"),
            ];
            if let Err(error) = stream.update_params(&mut params) {
                finish_damage(data, Err(format!("update_params: {error}")));
            }
        })
        .process(|stream, data| {
            while let Some(mut buffer) = stream.dequeue_buffer() {
                let damage: Vec<DamageRect> = buffer
                    .find_meta::<spa::buffer::meta::MetaVideoDamage>()
                    .map(|meta| {
                        meta.iter()
                            .map(|region| {
                                let raw = region.as_raw();
                                DamageRect {
                                    x: raw.region.position.x,
                                    y: raw.region.position.y,
                                    w: raw.region.size.width,
                                    h: raw.region.size.height,
                                }
                            })
                            .filter(|rect| rect.w != 0 && rect.h != 0)
                            .collect()
                    })
                    .unwrap_or_default();
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    continue;
                }
                let data_ref = &mut datas[0];
                let size = data_ref.chunk().size() as usize;
                if size == 0 {
                    continue;
                }
                let received = if data_ref.type_() == spa::buffer::DataType::DmaBuf {
                    match read_dmabuf(data_ref, size) {
                        Ok(received) => received,
                        Err(error) => {
                            finish_damage(data, Err(error));
                            return;
                        }
                    }
                } else {
                    let Some(slice) = data_ref.data() else {
                        finish_damage(data, Err("shared buffer has no mapped data".into()));
                        return;
                    };
                    if slice.len() < size {
                        finish_damage(data, Err("shared buffer is smaller than its chunk".into()));
                        return;
                    }
                    Received::SharedMem(slice[..size].to_vec())
                };
                finish_damage(data, Ok((received, damage)));
                return;
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    let timeout_loop_weak = mainloop.downgrade();
    let timeout_result = Rc::clone(&result);
    let timeout_timer = mainloop.loop_().add_timer(move |_| {
        let mut slot = timeout_result.borrow_mut();
        if slot.is_none() {
            *slot = Some(Err("timed out waiting for a frame".into()));
            if let Some(mainloop) = timeout_loop_weak.upgrade() {
                mainloop.quit();
            }
        }
    });
    timeout_timer.update_timer(Some(timeout), None);

    let format_bytes = format_pod(width, height, false);
    let mut format_refs = [Pod::from_bytes(&format_bytes).expect("format pod")];
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            StreamFlags::AUTOCONNECT | StreamFlags::MAP_BUFFERS,
            &mut format_refs,
        )
        .map_err(|e| e.to_string())?;

    mainloop.run();
    let result = result.borrow_mut().take();
    result.expect("the main loop only quits with a result")
}

struct DamageConsumerData {
    result: DamageResult,
    loop_weak: pw::main_loop::MainLoopWeak,
}

type DamageResult = Rc<RefCell<Option<Result<(Received, Vec<DamageRect>), String>>>>;

fn finish_damage(data: &DamageConsumerData, result: Result<(Received, Vec<DamageRect>), String>) {
    let mut slot = data.result.borrow_mut();
    if slot.is_none() {
        *slot = Some(result);
        if let Some(mainloop) = data.loop_weak.upgrade() {
            mainloop.quit();
        }
    }
}

/// Read a forwarded dmabuf by mapping its descriptor. The test stand-in is
/// a memfd, which maps like any file; real GPU buffers may not be mappable,
/// but real consumers import them into the GPU instead of reading pixels.
fn read_dmabuf(data: &pw::spa::buffer::Data, size: usize) -> Result<Received, String> {
    // SAFETY: the raw spa_data is live for the callback's duration.
    let raw = data.as_raw();
    let map_len = raw.maxsize as usize;
    if map_len < size {
        return Err("dmabuf is smaller than its chunk".into());
    }
    // SAFETY: the producer keeps the descriptor open until the buffer is
    // returned; the mapping is unmapped before this buffer is dropped (which
    // returns it).
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            data.fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(format!("dmabuf mmap: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: `map`/`map_len` name the live mapping created above.
    let bytes = unsafe { std::slice::from_raw_parts(map.cast::<u8>(), map_len) }[..size].to_vec();
    // SAFETY: as above.
    unsafe { libc::munmap(map, map_len) };
    Ok(Received::DmaBuf(bytes))
}

/// The consumer's format offer: raw BGRx at the stream's geometry, with a
/// modifier enumeration when asking for zero-copy delivery.
fn format_pod(width: u32, height: u32, offer_dmabuf: bool) -> Vec<u8> {
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
            value: pod::Value::Id(spa::utils::Id(
                spa::param::video::VideoFormat::BGRx.as_raw(),
            )),
        },
    ];
    if offer_dmabuf {
        // Exactly one modifier: the producer's offer carries DRM_FORMAT_MOD_LINEAR.
        properties.push(pod::Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Choice(pod::ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: 0,
                    alternatives: Vec::new(),
                },
            ))),
        });
    }
    properties.push(pod::Property {
        key: spa::param::format::FormatProperties::VideoSize.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Rectangle(Rectangle { width, height }),
    });
    properties.push(pod::Property {
        key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
        flags: pod::PropertyFlags::empty(),
        value: pod::Value::Choice(pod::ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: Fraction { num: 60, denom: 1 },
                min: Fraction { num: 1, denom: 1 },
                max: Fraction { num: 360, denom: 1 },
            },
        ))),
    });
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    };
    serialize(&pod::Value::Object(object))
}

/// The consumer's buffer constraints: only the accepted data types, the
/// same minimal shape OBS sends.
fn buffers_pod(data_types: u32) -> Vec<u8> {
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![pod::Property {
            key: 6, // SPA_PARAM_BUFFERS_dataType
            flags: pod::PropertyFlags::empty(),
            value: pod::Value::Int(data_types as i32),
        }],
    };
    serialize(&pod::Value::Object(object))
}

/// The consumer's `SPA_PARAM_Meta` request for VideoDamage: the shape
/// OBS's PipeWire source offers when it wants per-frame damage.
fn damage_meta_pod() -> Vec<u8> {
    let object = pod::Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            pod::Property {
                key: 1, // SPA_PARAM_META_type
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Id(spa::utils::Id(spa::sys::SPA_META_VideoDamage)),
            },
            pod::Property {
                key: 2, // SPA_PARAM_META_size
                flags: pod::PropertyFlags::empty(),
                value: pod::Value::Int(
                    (16 * std::mem::size_of::<spa::sys::spa_meta_region>()) as i32,
                ),
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
