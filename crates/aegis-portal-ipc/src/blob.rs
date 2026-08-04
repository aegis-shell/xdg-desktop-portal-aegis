//! Sealed memfd transport for screenshot and screencast payloads.

use std::fs::File;
#[cfg(any(test, feature = "test-server"))]
use std::io::Write;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::{size_of, zeroed};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

const BLOB_MARKER: u8 = 0xfd;
pub(crate) const MAX_BLOB_BYTES: u64 = 288 * 1024 * 1024;
const REQUIRED_SEALS: libc::c_int =
    libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE;

#[cfg(any(test, feature = "test-server"))]
pub(crate) struct SealedBlob {
    file: File,
    len: u64,
}

#[cfg(any(test, feature = "test-server"))]
impl SealedBlob {
    pub(crate) fn new(bytes: &[u8]) -> io::Result<Self> {
        let len = u64::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "blob length overflow"))?;
        validate_len(len)?;
        // SAFETY: the name is static and NUL-terminated; the returned fd is
        // checked before ownership is constructed.
        let fd = unsafe {
            libc::memfd_create(
                c"aegis-portal-ipc".as_ptr(),
                libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `memfd_create` returned a new owned descriptor.
        let mut file = unsafe { File::from_raw_fd(fd) };
        file.write_all(bytes)?;
        file.seek(SeekFrom::Start(0))?;
        // SAFETY: `file` owns a memfd created with sealing enabled.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, REQUIRED_SEALS) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { file, len })
    }

    pub(crate) fn len(&self) -> u64 {
        self.len
    }

    pub(crate) fn send(&self, stream: &UnixStream) -> io::Result<()> {
        send_fd(stream, self.file.as_raw_fd())
    }
}

pub(crate) fn receive(stream: &UnixStream, expected_len: u64) -> io::Result<Vec<u8>> {
    validate_len(expected_len)?;
    let fd = receive_fd(stream)?;
    // SAFETY: `receive_fd` returns a newly received owned descriptor.
    let mut file = unsafe { File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() != expected_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "capture memfd length/type mismatch (expected {expected_len}, got {})",
                metadata.len()
            ),
        ));
    }
    // SAFETY: F_GET_SEALS has no pointer argument and `file` owns a valid fd.
    let seals = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) };
    if seals < 0 || seals & REQUIRED_SEALS != REQUIRED_SEALS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "capture descriptor is not fully sealed",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let length = usize::try_from(expected_len)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "capture is too large"))?;
    let mut bytes = vec![0_u8; length];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn validate_len(length: u64) -> io::Result<()> {
    if length == 0 || length > MAX_BLOB_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("blob length {length} is outside 1..={MAX_BLOB_BYTES}"),
        ));
    }
    Ok(())
}

#[cfg(any(test, feature = "test-server"))]
fn send_fd(stream: &UnixStream, fd: RawFd) -> io::Result<()> {
    let mut marker = BLOB_MARKER;
    let mut iov = libc::iovec {
        iov_base: (&mut marker as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE computes storage for exactly one descriptor.
    let control_len = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0_u8; control_len];
    // SAFETY: all pointers reference live storage for the duration of sendmsg.
    let sent = unsafe {
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        let header = libc::CMSG_FIRSTHDR(&message);
        if header.is_null() {
            return Err(io::Error::other("failed to construct SCM_RIGHTS header"));
        }
        (*header).cmsg_level = libc::SOL_SOCKET;
        (*header).cmsg_type = libc::SCM_RIGHTS;
        (*header).cmsg_len = libc::CMSG_LEN(size_of::<RawFd>() as libc::c_uint) as usize;
        std::ptr::write_unaligned(libc::CMSG_DATA(header).cast::<RawFd>(), fd);
        libc::sendmsg(stream.as_raw_fd(), &message, libc::MSG_NOSIGNAL)
    };
    if sent < 0 {
        return Err(io::Error::last_os_error());
    }
    if sent != 1 {
        return Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "capture descriptor marker was not written atomically",
        ));
    }
    Ok(())
}

fn receive_fd(stream: &UnixStream) -> io::Result<RawFd> {
    let mut marker = 0_u8;
    let mut iov = libc::iovec {
        iov_base: (&mut marker as *mut u8).cast(),
        iov_len: 1,
    };
    // SAFETY: CMSG_SPACE computes storage for exactly one descriptor.
    let control_len = unsafe { libc::CMSG_SPACE(size_of::<RawFd>() as libc::c_uint) } as usize;
    let mut control = vec![0_u8; control_len];
    // SAFETY: all msghdr pointers target live writable storage.
    let (received, flags, fd) = unsafe {
        let mut message: libc::msghdr = zeroed();
        message.msg_iov = &mut iov;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = control.len();
        let received = libc::recvmsg(stream.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC);
        if received < 0 {
            return Err(io::Error::last_os_error());
        }
        let header = libc::CMSG_FIRSTHDR(&message);
        let fd = if header.is_null()
            || (*header).cmsg_level != libc::SOL_SOCKET
            || (*header).cmsg_type != libc::SCM_RIGHTS
            || (*header).cmsg_len < libc::CMSG_LEN(size_of::<RawFd>() as libc::c_uint) as usize
        {
            -1
        } else {
            std::ptr::read_unaligned(libc::CMSG_DATA(header).cast::<RawFd>())
        };
        (received, message.msg_flags, fd)
    };
    if received != 1 || marker != BLOB_MARKER || flags & libc::MSG_CTRUNC != 0 || fd < 0 {
        if fd >= 0 {
            // SAFETY: the received descriptor is rejected before ownership is wrapped.
            unsafe { libc::close(fd) };
        }
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "missing or truncated capture descriptor",
        ));
    }
    Ok(fd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_blob_round_trips() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let blob = SealedBlob::new(b"immutable pixels").unwrap();
        blob.send(&sender).unwrap();
        assert_eq!(receive(&receiver, blob.len()).unwrap(), b"immutable pixels");
    }

    #[test]
    fn blob_length_is_bounded_before_allocation() {
        assert!(validate_len(0).is_err());
        assert!(validate_len(MAX_BLOB_BYTES).is_ok());
        assert!(validate_len(MAX_BLOB_BYTES + 1).is_err());
    }
}
