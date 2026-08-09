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
    ) -> Result<StreamInfo, String> {
        Ok(StreamInfo {
            stream_id: 7,
            width: 2,
            height: 2,
            format: StreamPixelFormat::Dmabuf {
                drm_format: DRM_FORMAT_XRGB8888,
                modifier: MOD_LINEAR,
            },
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
