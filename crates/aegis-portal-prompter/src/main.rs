//! One-shot optics (iris/lens) host for portal prompt requests.
//!
//! The backend writes one versioned JSON request to stdin; this process shows
//! the matching native dialog (file selection, confirmation, or secret
//! password) and writes one versioned JSON response to stdout. The wire
//! contract lives in `aegis_portal_prompter`; this binary only renders it.

use std::io::{Read, Write};
use std::process::ExitCode;

use aegis_portal_prompter::{PromptRequest, PrompterRequest, PrompterResponse};

mod ui;

const MAX_MESSAGE_BYTES: u64 = 8 * 1024 * 1024;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let response = match read_request().and_then(run_dialog) {
        Ok(response) => response,
        Err(message) => {
            log::error!("prompter: {message}");
            PrompterResponse::failed(message)
        }
    };
    match write_response(&response) {
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

fn write_response(response: &PrompterResponse) -> Result<(), String> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, response)
        .map_err(|error| format!("could not encode response: {error}"))?;
    stdout
        .write_all(b"\n")
        .and_then(|()| stdout.flush())
        .map_err(|error| error.to_string())
}

fn run_dialog(request: PromptRequest) -> Result<PrompterResponse, String> {
    let result = match request {
        PromptRequest::Selection(request) => ui::chooser::run(request)?,
        PromptRequest::Confirm(request) => ui::confirm::run(request)?,
        PromptRequest::Secret(request) => ui::secret::run(request)?,
    };
    Ok(PrompterResponse {
        version: aegis_portal_prompter::PROCESS_CONTRACT_VERSION,
        result,
    })
}
