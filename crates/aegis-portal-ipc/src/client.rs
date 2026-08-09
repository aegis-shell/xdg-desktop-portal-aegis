//! Synchronous client for the Portal-owned Aegis IPC v24 projection.

use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use crate::blob;
use crate::codec::{read_msg, write_msg};
use crate::schema::{LeaseRequest, Request, Response};
use crate::{
    ConfirmPickResult, ConnectionCapabilities, Event, LeaseGrant, PROTOCOL_VERSION, PickKind,
    PickResult, Rect, SettingsSnapshot, StreamPixelFormat, StreamTarget,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStarted {
    pub stream_id: u64,
    pub width: u32,
    pub height: u32,
    pub format: StreamPixelFormat,
}

/// Frame payload descriptor received behind a `StreamFrame` header. The
/// variant mirrors the header's pixel format: SHM formats carry a sealed
/// memfd, `Dmabuf` carries the single-plane GPU buffer descriptor.
#[derive(Debug)]
pub enum StreamPayload {
    /// Sealed memfd of `byte_len` bytes, positioned at offset 0.
    Memfd(std::fs::File),
    /// Single-plane dmabuf of `byte_len` bytes; the plane stride travels in
    /// the frame header.
    Dmabuf(std::fs::File),
}

#[derive(Debug)]
pub struct StreamFrame {
    pub stream_id: u64,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub format: StreamPixelFormat,
    pub damage: Vec<Rect>,
    pub dropped: u64,
    pub payload: StreamPayload,
}

#[derive(Debug)]
pub enum StreamMessage {
    Frame(StreamFrame),
    Ended { stream_id: u64, reason: String },
    LeaseRenewed,
}

pub struct Client {
    stream: UnixStream,
    caps: ConnectionCapabilities,
    lease: Option<LeaseGrant>,
}

impl Client {
    pub fn connect_with_timeout(
        path: &Path,
        requested: ConnectionCapabilities,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::connect_inner(path, requested, None, timeout)
    }

    pub fn connect_scoped_with_timeout(
        path: &Path,
        requested: ConnectionCapabilities,
        scope: impl Into<String>,
        timeout: Duration,
    ) -> io::Result<Self> {
        Self::connect_inner(path, requested, Some(scope.into()), timeout)
    }

    fn connect_inner(
        path: &Path,
        requested: ConnectionCapabilities,
        scope: Option<String>,
        timeout: Duration,
    ) -> io::Result<Self> {
        let mut stream = UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        write_msg(
            &mut stream,
            &Request::Hello {
                version: PROTOCOL_VERSION,
                caps: requested,
                scope,
                lease: requested.privileged().then(LeaseRequest::default),
            },
        )?;
        match read_msg::<_, Response>(&mut stream)? {
            Response::Hello {
                version,
                caps,
                lease,
            } if version == PROTOCOL_VERSION => Ok(Self {
                stream,
                caps,
                lease,
            }),
            Response::Hello { version, .. } => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "Aegis IPC protocol mismatch: server replied {version}, Portal requires {PROTOCOL_VERSION}"
                ),
            )),
            Response::Error { message } => {
                Err(io::Error::new(io::ErrorKind::ConnectionRefused, message))
            }
            other => Err(unexpected("Hello", &other)),
        }
    }

    #[must_use]
    pub fn caps(&self) -> ConnectionCapabilities {
        self.caps
    }

    #[must_use]
    pub fn lease(&self) -> Option<LeaseGrant> {
        self.lease
    }

    pub fn set_io_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)?;
        self.stream.set_write_timeout(timeout)
    }

    pub fn renew_lease(&mut self, ttl_ms: u64) -> io::Result<LeaseGrant> {
        write_msg(&mut self.stream, &Request::RenewLease { ttl_ms })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::LeaseRenewed { lease } => {
                self.lease = Some(lease);
                Ok(lease)
            }
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("LeaseRenewed", &other)),
        }
    }

    pub fn settings(&mut self) -> io::Result<SettingsSnapshot> {
        write_msg(&mut self.stream, &Request::GetSettings)?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Settings { snapshot } => Ok(snapshot),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Settings", &other)),
        }
    }

    pub fn subscribe(&mut self) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::Subscribe)?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Subscribed => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Subscribed", &other)),
        }
    }

    pub fn next_event(&mut self) -> io::Result<Event> {
        let value: serde_json::Value = read_msg(&mut self.stream)?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("SettingsChanged") => serde_json::from_value(value).map_err(json_error),
            Some(_) => Ok(Event::Other),
            None => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Aegis IPC event has no type",
            )),
        }
    }

    pub fn capture_output(&mut self) -> io::Result<(u32, u32, Vec<u8>)> {
        self.capture_output_region(None)
    }

    pub fn capture_output_region(
        &mut self,
        region: Option<Rect>,
    ) -> io::Result<(u32, u32, Vec<u8>)> {
        write_msg(&mut self.stream, &Request::CaptureOutput { region })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::CaptureOutput {
                width,
                height,
                png_bytes,
            } => Ok((width, height, blob::receive(&self.stream, png_bytes)?)),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("CaptureOutput", &other)),
        }
    }

    pub fn pick_target(&mut self, kind: PickKind) -> io::Result<PickResult> {
        write_msg(&mut self.stream, &Request::PickTarget { kind })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::Picked { result } => Ok(result),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("Picked", &other)),
        }
    }

    pub fn pick_confirm(
        &mut self,
        title: String,
        body: String,
        accept_label: Option<String>,
    ) -> io::Result<ConfirmPickResult> {
        write_msg(
            &mut self.stream,
            &Request::PickConfirm {
                title,
                body,
                accept_label,
            },
        )?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::ConfirmPicked { result } => Ok(result),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("ConfirmPicked", &other)),
        }
    }

    pub fn start_output_stream_target(
        &mut self,
        max_fps: Option<u32>,
        target: StreamTarget,
    ) -> io::Result<StreamStarted> {
        write_msg(
            &mut self.stream,
            &Request::StreamOutputStart { max_fps, target },
        )?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::StreamOutputStarted {
                stream_id,
                width,
                height,
                format,
            } => Ok(StreamStarted {
                stream_id,
                width,
                height,
                format,
            }),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("StreamOutputStarted", &other)),
        }
    }

    pub fn stop_output_stream(&mut self, stream_id: u64) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::StreamOutputStop { stream_id })?;
        match read_msg::<_, Response>(&mut self.stream)? {
            Response::StreamOutputStopped { stream_id: stopped } if stopped == stream_id => Ok(()),
            Response::Error { message } => Err(io::Error::other(message)),
            other => Err(unexpected("StreamOutputStopped", &other)),
        }
    }

    pub fn request_lease_renewal(&mut self, ttl_ms: u64) -> io::Result<()> {
        write_msg(&mut self.stream, &Request::RenewLease { ttl_ms })
    }

    pub fn next_stream_message(&mut self) -> io::Result<StreamMessage> {
        loop {
            let value: serde_json::Value = read_msg(&mut self.stream)?;
            match value.get("type").and_then(serde_json::Value::as_str) {
                Some("StreamFrame") => {
                    let event: Event = serde_json::from_value(value).map_err(json_error)?;
                    let Event::StreamFrame {
                        stream_id,
                        sequence,
                        width,
                        height,
                        stride,
                        format,
                        damage,
                        dropped,
                        byte_len,
                    } = event
                    else {
                        unreachable!();
                    };
                    let payload = match format {
                        StreamPixelFormat::Bgra8 | StreamPixelFormat::Rgba8 => {
                            StreamPayload::Memfd(blob::receive_memfd_file(&self.stream, byte_len)?)
                        }
                        StreamPixelFormat::Dmabuf { .. } => StreamPayload::Dmabuf(
                            blob::receive_dmabuf_file(&self.stream, byte_len)?,
                        ),
                    };
                    return Ok(StreamMessage::Frame(StreamFrame {
                        stream_id,
                        sequence,
                        width,
                        height,
                        stride,
                        format,
                        damage,
                        dropped,
                        payload,
                    }));
                }
                Some("StreamEnded") => {
                    let event: Event = serde_json::from_value(value).map_err(json_error)?;
                    let Event::StreamEnded { stream_id, reason } = event else {
                        unreachable!();
                    };
                    return Ok(StreamMessage::Ended { stream_id, reason });
                }
                Some("LeaseRenewed") => {
                    let response: Response = serde_json::from_value(value).map_err(json_error)?;
                    let Response::LeaseRenewed { lease } = response else {
                        unreachable!();
                    };
                    self.lease = Some(lease);
                    return Ok(StreamMessage::LeaseRenewed);
                }
                Some("Error") => {
                    let response: Response = serde_json::from_value(value).map_err(json_error)?;
                    let Response::Error { message } = response else {
                        unreachable!();
                    };
                    return Err(io::Error::other(message));
                }
                Some(_) => continue,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Aegis IPC stream message has no type",
                    ));
                }
            }
        }
    }
}

impl AsRawFd for Client {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.stream.as_raw_fd()
    }
}

fn unexpected(expected: &str, response: &Response) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("expected {expected}, got {response:?}"),
    )
}

fn json_error(error: serde_json::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error.to_string())
}
