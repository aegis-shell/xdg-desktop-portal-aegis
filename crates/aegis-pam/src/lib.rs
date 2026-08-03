//! `pam_aegis`: cache the login authtok for the aegis secret vault.
//!
//! Installed as `auth optional pam_aegis.so` in the login stack (and in the
//! lock screen's stack), this module writes the just-verified password to
//! `/run/user/<uid>/aegis-pam-token` with
//! mode 0600. `xdg-desktop-portal-aegis` consumes and deletes the token to
//! unlock a password-mode vault without prompting — the wssp-pam pattern.
//! The token is written atomically (temp-then-rename) so a reader never
//! observes a partial file, and the in-memory copy is zeroized afterwards.
//!
//! Failure posture: a module error never blocks authentication
//! (`optional`); at worst the vault stays locked and the user is prompted
//! through compositor chrome instead.

use pamsm::{Pam, PamError, PamFlag, PamLibExt, PamServiceModule, pam_module};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use zeroize::Zeroize;

/// The token file name under the user's runtime directory.
pub const TOKEN_NAME: &str = "aegis-pam-token";

struct AegisPam;

impl PamServiceModule for AegisPam {
    fn open_session(_pamh: Pam, _flags: PamFlag, _args: Vec<String>) -> PamError {
        PamError::SUCCESS
    }

    fn authenticate(pamh: Pam, _flags: PamFlag, _args: Vec<String>) -> PamError {
        let user = match pamh.get_cached_user() {
            Ok(Some(user)) => user.to_bytes().to_vec(),
            _ => return PamError::USER_UNKNOWN,
        };
        let mut authtok = match pamh.get_cached_authtok() {
            Ok(Some(token)) => token.to_bytes().to_vec(),
            _ => return PamError::SUCCESS,
        };
        if authtok.is_empty() {
            return PamError::SUCCESS;
        }

        let c_user = match CString::new(user) {
            Ok(user) => user,
            Err(_) => {
                authtok.zeroize();
                return PamError::USER_UNKNOWN;
            }
        };
        let Some((uid, gid)) = account_ids(&c_user) else {
            authtok.zeroize();
            return PamError::USER_UNKNOWN;
        };

        let result = write_token(uid, gid, &authtok);
        authtok.zeroize();
        result
    }

    fn setcred(_pam: Pam, _flags: PamFlag, _args: Vec<String>) -> PamError {
        PamError::SUCCESS
    }

    fn close_session(_pam: Pam, _flags: PamFlag, _args: Vec<String>) -> PamError {
        PamError::SUCCESS
    }
}

/// Thread-safe passwd lookup. Display managers may authenticate more than
/// one session concurrently, so the process-global storage from `getpwnam`
/// is not safe inside a PAM module.
fn account_ids(user: &CString) -> Option<(libc::uid_t, libc::gid_t)> {
    // SAFETY: sysconf reads one process configuration value.
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let mut size = if suggested > 0 {
        usize::try_from(suggested).unwrap_or(16 * 1024)
    } else {
        16 * 1024
    }
    .clamp(1024, 1024 * 1024);

    loop {
        // SAFETY: passwd is an output-only C struct initialized before use.
        let mut passwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = std::ptr::null_mut();
        let mut buffer = vec![0_u8; size];
        // SAFETY: pointers and buffer are valid for the duration of the call.
        let status = unsafe {
            libc::getpwnam_r(
                user.as_ptr(),
                &mut passwd,
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if status == libc::ERANGE && size < 1024 * 1024 {
            size = (size * 2).min(1024 * 1024);
            continue;
        }
        return (status == 0 && !result.is_null()).then_some((passwd.pw_uid, passwd.pw_gid));
    }
}

/// PAM runs in a privileged, environment-sensitive process. Never trust an
/// inherited `XDG_RUNTIME_DIR` here: logind's `/run/user/<uid>` location has
/// a root-owned parent and is the only directory accepted by the module.
fn runtime_dir(uid: libc::uid_t) -> std::path::PathBuf {
    std::path::PathBuf::from(format!("/run/user/{uid}"))
}

fn last_os_error() -> io::Error {
    io::Error::last_os_error()
}

/// Open and validate the logind runtime directory without following a
/// symlink. All subsequent filesystem operations are relative to this file
/// descriptor, closing the check/use race on the directory path.
fn open_runtime_dir(path: &std::path::Path, uid: libc::uid_t) -> io::Result<File> {
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in runtime path"))?;
    // SAFETY: `path` is NUL terminated; open returns a new owned descriptor.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(last_os_error());
    }
    // SAFETY: `fd` was just returned by open and ownership moves to `File`.
    let directory = unsafe { File::from_raw_fd(fd) };

    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `stat` points to writable storage and the fd remains open.
    if unsafe { libc::fstat(directory.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(last_os_error());
    }
    // SAFETY: a successful fstat initialized the structure.
    let stat = unsafe { stat.assume_init() };
    let mode = stat.st_mode;
    if mode & libc::S_IFMT != libc::S_IFDIR
        || stat.st_uid != uid
        || mode & (libc::S_IRWXG | libc::S_IRWXO) != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime directory must be owned by the target user and mode 0700",
        ));
    }
    Ok(directory)
}

fn random_temporary_name() -> io::Result<CString> {
    let mut random = [0u8; 16];
    let mut filled = 0;
    while filled < random.len() {
        // SAFETY: the remaining slice is valid writable memory.
        let count = unsafe {
            libc::getrandom(
                random[filled..].as_mut_ptr().cast(),
                random.len() - filled,
                0,
            )
        };
        if count < 0 {
            let error = last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if count == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "getrandom returned no data",
            ));
        }
        filled += count as usize;
    }
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    CString::new(format!(".{TOKEN_NAME}.{suffix}.tmp"))
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid temporary name"))
}

fn unlink_at(directory: RawFd, name: &CString) {
    // SAFETY: the directory fd and NUL-terminated relative name are valid.
    let _ = unsafe { libc::unlinkat(directory, name.as_ptr(), 0) };
}

/// Write the token beneath an already selected runtime directory. This is
/// split out so the security invariants can be exercised without a live
/// logind session in unit tests.
fn write_token_in(
    runtime_dir: &std::path::Path,
    uid: libc::uid_t,
    gid: libc::gid_t,
    secret: &[u8],
) -> io::Result<()> {
    let directory = open_runtime_dir(runtime_dir, uid)?;
    let directory_fd = directory.as_raw_fd();
    let token_name = CString::new(TOKEN_NAME).expect("static token name has no NUL");

    let (temporary_name, mut temporary) = loop {
        let name = random_temporary_name()?;
        // SAFETY: all pointers are valid and the relative name is NUL
        // terminated. O_EXCL prevents opening any attacker-created entry;
        // O_NOFOLLOW is defense in depth.
        let fd = unsafe {
            libc::openat(
                directory_fd,
                name.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd >= 0 {
            // SAFETY: `fd` is newly owned by this scope.
            break (name, unsafe { File::from_raw_fd(fd) });
        }
        let error = last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(error);
        }
    };

    let result = (|| {
        temporary.write_all(secret)?;
        // Set ownership through the descriptor. A path substitution can
        // therefore never redirect privileged chown to another file.
        // SAFETY: the fd remains open for the duration of the call.
        if unsafe { libc::fchown(temporary.as_raw_fd(), uid, gid) } != 0 {
            return Err(last_os_error());
        }
        // SAFETY: the fd remains open for the duration of the call.
        if unsafe { libc::fchmod(temporary.as_raw_fd(), 0o600) } != 0 {
            return Err(last_os_error());
        }
        temporary.sync_all()?;

        // renameat replaces a pre-existing token entry itself; it never
        // follows a symlink stored at the destination.
        // SAFETY: both names are NUL terminated and relative to the open
        // directory descriptor.
        if unsafe {
            libc::renameat(
                directory_fd,
                temporary_name.as_ptr(),
                directory_fd,
                token_name.as_ptr(),
            )
        } != 0
        {
            return Err(last_os_error());
        }
        directory.sync_all()
    })();

    if result.is_err() {
        unlink_at(directory_fd, &temporary_name);
    }
    result
}

/// Write the token atomically, mode 0600, owned by the user.
fn write_token(uid: libc::uid_t, gid: libc::gid_t, secret: &[u8]) -> PamError {
    match write_token_in(&runtime_dir(uid), uid, gid, secret) {
        Ok(()) => PamError::SUCCESS,
        Err(_) => PamError::SESSION_ERR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn test_directory(tag: &str) -> std::path::PathBuf {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("aegis-pam-{tag}-{}-{sequence}", std::process::id()))
    }

    fn current_identity() -> (libc::uid_t, libc::gid_t) {
        // SAFETY: these calls only read the process credentials.
        unsafe { (libc::getuid(), libc::getgid()) }
    }

    #[test]
    fn destination_symlink_is_replaced_without_following_it() {
        let directory = test_directory("symlink");
        let victim = test_directory("victim");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(&victim, b"do not touch").unwrap();
        symlink(&victim, directory.join(TOKEN_NAME)).unwrap();
        let (uid, gid) = current_identity();

        write_token_in(&directory, uid, gid, b"login password").unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"do not touch");
        let token = directory.join(TOKEN_NAME);
        let metadata = std::fs::symlink_metadata(&token).unwrap();
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.gid(), gid);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(token).unwrap(), b"login password");

        std::fs::remove_dir_all(directory).unwrap();
        std::fs::remove_file(victim).unwrap();
    }

    #[test]
    fn rejects_runtime_directory_accessible_to_other_users() {
        let directory = test_directory("unsafe-mode");
        std::fs::create_dir(&directory).unwrap();
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o770)).unwrap();
        let (uid, gid) = current_identity();

        let error = write_token_in(&directory, uid, gid, b"secret").unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(!directory.join(TOKEN_NAME).exists());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

pam_module!(AegisPam);
