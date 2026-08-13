//! Supervision for one Portal-owned, one-shot optics (iris/lens) prompter process.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use aegis_portal_prompter::{PromptResult, PrompterRequest, PrompterResponse};
use zeroize::Zeroizing;

const PROMPTER_ENV: &str = "AEGIS_PORTAL_PROMPTER";
const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum InvokeError {
    #[error("portal request was cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

pub(crate) fn invoke(
    request: PrompterRequest,
    cancellation: Option<&dyn Fn() -> bool>,
) -> Result<PromptResult, InvokeError> {
    let executable = executable().map_err(InvokeError::Failed)?;
    let mut child = Command::new(&executable)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| {
            InvokeError::Failed(format!("could not start {}: {error}", executable.display()))
        })?;

    let Some(mut stdin) = child.stdin.take() else {
        terminate(&mut child);
        return Err(InvokeError::Failed(
            "prompter stdin was not piped".to_owned(),
        ));
    };
    let Some(stdout) = child.stdout.take() else {
        terminate(&mut child);
        return Err(InvokeError::Failed(
            "prompter stdout was not piped".to_owned(),
        ));
    };
    let send_result = serde_json::to_writer(&mut stdin, &request)
        .map_err(|error| error.to_string())
        .and_then(|()| stdin.write_all(b"\n").map_err(|error| error.to_string()));
    if let Err(error) = send_result {
        terminate(&mut child);
        return Err(InvokeError::Failed(format!(
            "could not send prompter request: {error}"
        )));
    }
    drop(stdin);

    let reader = std::thread::spawn(move || {
        // Responses that carry secrets (passwords, choices) are small
        // JSON documents, well under 1 KiB: reserving that upfront keeps
        // them in a single allocation, so no growth reallocation can
        // leave an unzeroized copy in freed heap. Larger no-secret
        // responses (file lists) may still reallocate.
        let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
        stdout
            .take(MAX_MESSAGE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes)
    });

    let status = loop {
        if cancellation.is_some_and(|cancelled| cancelled()) {
            terminate(&mut child);
            let _ = reader.join();
            return Err(InvokeError::Cancelled);
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                terminate(&mut child);
                let _ = reader.join();
                return Err(InvokeError::Failed(format!(
                    "could not wait for prompter: {error}"
                )));
            }
        }
    };
    let bytes = reader
        .join()
        .map_err(|_| InvokeError::Failed("prompter response reader panicked".to_owned()))?
        .map_err(|error| {
            InvokeError::Failed(format!("could not read prompter response: {error}"))
        })?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err(InvokeError::Failed(
            "prompter response exceeds the 8 MiB process-contract limit".into(),
        ));
    }
    if bytes.is_empty() {
        return Err(InvokeError::Failed(format!(
            "prompter exited with {status} and no response"
        )));
    }
    // The read buffer has reached its final size and is only borrowed from
    // here on; pin it against swapping while the response (which can carry
    // a vault password) is parsed. Best effort: the guard is None when the
    // platform or rlimit refuses, and parsing proceeds either way.
    let _response_lock = MlockGuard::new(&bytes);
    let decoded = serde_json::from_slice(&bytes);
    let response: PrompterResponse = decoded.map_err(|error| {
        InvokeError::Failed(format!(
            "prompter exited with {status} and returned invalid JSON: {error}"
        ))
    })?;
    match response.into_result().map_err(InvokeError::Failed)? {
        PromptResult::Failed { message } => Err(InvokeError::Failed(message)),
        result => Ok(result),
    }
}

fn terminate(child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// Best-effort `mlock` guard for the prompter's response bytes, which can
/// carry a vault password: pins the fully-read buffer against swapping
/// while it is alive, `munlock`ing on drop. `None` from [`MlockGuard::new`]
/// means the region is empty or the platform/rlimit refused; the caller
/// proceeds without a lock in both cases.
struct MlockGuard {
    addr: *const libc::c_void,
    len: usize,
}

impl MlockGuard {
    fn new(bytes: &[u8]) -> Option<MlockGuard> {
        if bytes.is_empty() {
            return None;
        }
        // SAFETY: `bytes` is a valid readable region owned by this process;
        // mlock only pins its pages against swapping. Failure (for example
        // RLIMIT_MEMLOCK) is non-fatal.
        let result = unsafe { libc::mlock(bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        if result != 0 {
            log::warn!(
                "portal: could not mlock the prompter response: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(MlockGuard {
            addr: bytes.as_ptr().cast::<libc::c_void>(),
            len: bytes.len(),
        })
    }
}

impl Drop for MlockGuard {
    fn drop(&mut self) {
        // SAFETY: the region was successfully mlock'd in `new`; the owning
        // `Zeroizing<Vec<u8>>` is only read (never reallocated) while the
        // guard is alive, so the recorded address is still exact, and the
        // Vec's zeroize-on-drop runs after the guard is released.
        unsafe { libc::munlock(self.addr, self.len) };
    }
}

/// The prompter executable's path: `$AEGIS_PORTAL_PROMPTER`, then beside
/// the backend, then the standard libexec directories. Shared by the
/// one-shot invocation and the notification daemon spawn.
pub(crate) fn executable() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os(PROMPTER_ENV).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(directory) = current.parent()
    {
        let sibling = directory.join("aegis-portal-prompter");
        if sibling.is_file() {
            return Ok(sibling);
        }
    }
    for installed in [
        PathBuf::from("/usr/libexec/aegis-portal-prompter"),
        PathBuf::from("/usr/lib/aegis-portal-prompter"),
    ] {
        if installed.is_file() {
            return Ok(installed);
        }
    }
    Err(format!(
        "aegis-portal-prompter was not found beside the backend or in the standard libexec directories; set {PROMPTER_ENV}"
    ))
}
