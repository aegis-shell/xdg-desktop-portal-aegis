//! Stream-frame transport tests against the independent test server. The
//! server speaks the literal v24 wire shape; a plain memfd stands in for a
//! GPU dmabuf so the descriptor paths run on machines without a render node.
#![cfg(feature = "test-server")]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::time::Duration;

use aegis_portal_ipc::testing::{Handler, Server, StreamFrameFdPayload, StreamInfo};
use aegis_portal_ipc::{
    Client, ConnectionCapabilities, StreamMessage, StreamPixelFormat, StreamTarget,
};

const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const MOD_LINEAR: u64 = 0;

struct DmabufStream;

impl Handler for DmabufStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        _dmabuf: Option<bool>,
    ) -> Result<StreamInfo, String> {
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            slots: None,
        })
    }
}

fn socket_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "aegis-ipc-{name}-{}-{}.sock",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ))
}

/// An unsealed memfd mirrors what the compositor sends for a dmabuf frame:
/// a fixed-size descriptor without memfd seals.
fn unsealed_memfd(bytes: &[u8]) -> std::fs::File {
    // SAFETY: the name is static and NUL-terminated; the fd is checked
    // before ownership is constructed.
    let fd = unsafe { libc::memfd_create(c"aegis-ipc-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(fd >= 0, "memfd_create failed");
    // SAFETY: `fd` is a new owned descriptor from memfd_create.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    file.write_all(bytes).unwrap();
    file
}

#[test]
fn dmabuf_stream_frames_cross_the_wire_as_descriptors() {
    let server = Server::start(&socket_path("dmabuf-stream"), Arc::new(DmabufStream))
        .expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");

    let started = client
        .start_output_stream_target(None, StreamTarget::Output)
        .expect("stream start");
    assert_eq!(started.stream_id, 7);
    assert_eq!(
        started.format,
        StreamPixelFormat::Dmabuf {
            drm_format: DRM_FORMAT_XRGB8888,
            modifier: MOD_LINEAR
        }
    );

    let pixels = [0x5a_u8; 16];
    let frame_fd = unsealed_memfd(&pixels);
    assert!(server.push_stream_frame_fd(
        StreamFrameFdPayload {
            stream_id: 7,
            sequence: 1,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            damage: vec![],
            dropped: 0,
            byte_len: pixels.len() as u64,
        },
        frame_fd.as_raw_fd(),
    ));

    let message = client.next_stream_message().expect("frame message");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected a frame, got {message:?}");
    };
    assert_eq!(frame.sequence, 1);
    assert_eq!(frame.stride, 8);
    let aegis_portal_ipc::StreamPayload::Dmabuf(mut file) = frame.payload else {
        panic!("dmabuf frames must carry a dmabuf payload");
    };
    let mut received = Vec::new();
    file.read_to_end(&mut received).unwrap();
    assert_eq!(received, pixels);
}

struct SlotStream {
    releases: std::sync::Mutex<Vec<(u64, u32)>>,
    slot_files: std::sync::Mutex<Vec<std::fs::File>>,
}

impl Handler for SlotStream {
    fn stream_output_start(
        &self,
        _connection: u64,
        _max_fps: Option<u32>,
        _target: StreamTarget,
        dmabuf: Option<bool>,
    ) -> Result<StreamInfo, String> {
        if dmabuf != Some(true) {
            return Err("expected the dmabuf opt-in".into());
        }
        let mut files = Vec::new();
        let mut infos = Vec::new();
        for _ in 0..3 {
            let file = unsealed_memfd(&[0_u8; 16]);
            infos.push(aegis_portal_ipc::testing::StreamSlotInfo {
                fd: file.as_raw_fd(),
                stride: 8,
                byte_len: 16,
            });
            files.push(file);
        }
        *self.slot_files.lock().unwrap() = files;
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            slots: Some(infos),
        })
    }

    fn stream_buffer_release(&self, stream_id: u64, slot: u32) {
        self.releases.lock().unwrap().push((stream_id, slot));
    }
}

#[test]
fn slot_streams_transfer_the_table_frames_and_releases() {
    let handler = Arc::new(SlotStream {
        releases: std::sync::Mutex::new(Vec::new()),
        slot_files: std::sync::Mutex::new(Vec::new()),
    });
    let server =
        Server::start(&socket_path("slot-stream"), handler.clone()).expect("bind test server");
    let mut client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake");
    assert_eq!(
        client.protocol_version(),
        aegis_portal_ipc::PROTOCOL_VERSION
    );

    let started = client
        .start_output_stream(None, StreamTarget::Output, true)
        .expect("stream start");
    let slots = started.slots.expect("a slot table");
    assert_eq!(slots.len(), 3);
    assert_eq!((slots[0].stride, slots[0].byte_len), (8, 16));

    assert!(server.push_stream_frame_slot(
        aegis_portal_ipc::testing::StreamFrameFdPayload {
            stream_id: 7,
            sequence: 9,
            width: 2,
            height: 2,
            stride: 8,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
            damage: vec![],
            dropped: 0,
            byte_len: 16,
        },
        2,
    ));
    let message = client.next_stream_message().expect("frame message");
    let StreamMessage::Frame(frame) = message else {
        panic!("expected a frame, got {message:?}");
    };
    assert_eq!(frame.slot, Some(2));
    assert!(matches!(
        frame.payload,
        aegis_portal_ipc::StreamPayload::Slot
    ));

    client.release_stream_buffer(7, 2).expect("release write");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !handler.releases.lock().unwrap().contains(&(7, 2)) {
        assert!(
            std::time::Instant::now() < deadline,
            "release never arrived"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn handshake_negotiates_down_to_a_legacy_server() {
    let server = Server::start_legacy(
        &socket_path("legacy"),
        Arc::new(SlotStream {
            releases: std::sync::Mutex::new(Vec::new()),
            slot_files: std::sync::Mutex::new(Vec::new()),
        }),
        aegis_portal_ipc::MIN_PROTOCOL_VERSION,
    )
    .expect("bind legacy test server");
    let client = Client::connect_with_timeout(
        server.path(),
        ConnectionCapabilities::QUERY,
        Duration::from_secs(5),
    )
    .expect("handshake with downgrade");
    assert_eq!(
        client.protocol_version(),
        aegis_portal_ipc::MIN_PROTOCOL_VERSION
    );
}
