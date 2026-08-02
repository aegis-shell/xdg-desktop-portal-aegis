//! `xdg-desktop-portal-aegis` entry point: D-Bus-activated portal backend.

use std::process::ExitCode;

fn main() -> ExitCode {
    aegis_logging::init("info");
    match aegis_portal::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("xdg-desktop-portal-aegis: {error}");
            eprintln!("xdg-desktop-portal-aegis: {error}");
            ExitCode::FAILURE
        }
    }
}
