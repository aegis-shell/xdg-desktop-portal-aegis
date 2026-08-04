//! `xdg-desktop-portal-aegis` entry point: D-Bus-activated portal backend.

use std::process::ExitCode;

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    match aegis_portal::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::error!("xdg-desktop-portal-aegis: {error}");
            eprintln!("xdg-desktop-portal-aegis: {error}");
            ExitCode::FAILURE
        }
    }
}
