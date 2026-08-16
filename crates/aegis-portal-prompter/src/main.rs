//! One-shot optics (iris/lens) host for portal prompt requests.
//!
//! The backend writes one versioned JSON request to stdin; this process shows
//! the matching native dialog (file chooser, confirmation, secret password,
//! application chooser, or screen-source chooser) and writes one versioned
//! JSON response to stdout.
//! The wire contract lives in `aegis_portal_prompter`; this binary only
//! renders it.
//!
//! With `--notification-daemon` the process instead runs the long-lived
//! notification daemon (stream protocol in `aegis_portal_prompter::notify`,
//! UI in `ui::notify`).

use std::io::{Read, Write};
use std::process::ExitCode;

use aegis_portal_prompter::{
    PromptRequest, PromptResult, PrompterRequest, PrompterResponse, SecretResponse,
};

mod ui;
mod wire;

use wire::Wire;

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

// This crate does not depend on libc (unlike the daemon crate); declare
// the one syscall entry point needed to opt out of core dumps instead of
// growing the manifest for a single call. Linux-only, like the iris/lens
// stack this binary links.
#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn prctl(
        option: std::ffi::c_int,
        arg2: std::ffi::c_ulong,
        arg3: std::ffi::c_ulong,
        arg4: std::ffi::c_ulong,
        arg5: std::ffi::c_ulong,
    ) -> std::ffi::c_int;
}

/// Linux `PR_SET_DUMPABLE`: clear the process's dumpable flag.
#[cfg(target_os = "linux")]
const PR_SET_DUMPABLE: std::ffi::c_int = 4;

fn main() -> ExitCode {
    // Claim the protocol wire before anything else can touch stdout: the
    // optics C stack prints process-lifetime diagnostics, and a buffered
    // one flushed at exit corrupts a shared wire (see wire.rs).
    let mut wire = Wire::acquire();
    // Prompt requests can carry vault passwords through this process's
    // memory; keep them out of core dumps. Best effort: a prctl failure
    // (e.g. under a restrictive seccomp filter) warns and never aborts
    // startup. The logger is not initialized yet, so the warning goes to
    // stderr directly.
    // SAFETY: PR_SET_DUMPABLE takes no pointer arguments; the remaining
    // arguments are ignored (zeroed here).
    #[cfg(target_os = "linux")]
    if unsafe { prctl(PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        let error = std::io::Error::last_os_error();
        eprintln!("aegis-portal-prompter: could not disable core dumps: {error}");
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    if std::env::args().nth(1).as_deref() == Some("--notification-daemon") {
        return ui::notify::run_daemon(wire);
    }
    let response = match read_request().and_then(run_dialog) {
        Ok(response) => response,
        Err(message) => {
            log::error!("prompter: {message}");
            PrompterResponse::failed(message)
        }
    };
    match write_response(&mut wire, &response) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("prompter: could not write response: {error}");
            ExitCode::FAILURE
        }
    }
}

fn read_request() -> Result<PromptRequest, String> {
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(MAX_MESSAGE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read request: {error}"))?;
    if bytes.len() as u64 > MAX_MESSAGE_BYTES {
        return Err("request exceeds the 8 MiB process-contract limit".into());
    }
    let request: PrompterRequest =
        serde_json::from_slice(&bytes).map_err(|error| format!("invalid request JSON: {error}"))?;
    request.into_prompt()
}

fn write_response(wire: &mut Wire, response: &PrompterResponse) -> Result<(), String> {
    // Pin a secret response's password bytes against swapping for their
    // short serialization lifetime. Best effort: the guard is None when the
    // value is empty, the platform has no mlock, or the rlimit refuses, and
    // serialization proceeds either way. serde reads the String in place,
    // so its heap address is stable while the guard is held.
    let _secret_lock = match &response.result {
        PromptResult::Secret(SecretResponse::Secret { value }) => {
            ui::secret_buffer::PageLock::new(value.as_bytes())
        }
        _ => None,
    };
    serde_json::to_writer(&mut *wire, response)
        .map_err(|error| format!("could not encode response: {error}"))?;
    wire.write_all(b"\n")
        .and_then(|()| wire.flush())
        .map_err(|error| error.to_string())
}

fn run_dialog(request: PromptRequest) -> Result<PrompterResponse, String> {
    let result = match request {
        PromptRequest::FileChooser(request) => ui::file_chooser::run(request)?,
        PromptRequest::Confirm(request) => ui::confirm::run(request)?,
        PromptRequest::Secret(request) => ui::secret::run(request)?,
        PromptRequest::ChooseApp(request) => ui::choose_app::run(request)?,
        PromptRequest::ChooseSource(request) => ui::choose_source::run(request)?,
        PromptRequest::LauncherEdit(request) => ui::launcher_edit::run(request)?,
    };
    Ok(PrompterResponse {
        version: aegis_portal_prompter::PROCESS_CONTRACT_VERSION,
        result,
    })
}
