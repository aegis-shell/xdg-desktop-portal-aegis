//! `xdg-desktop-portal-aegis` entry point: D-Bus-activated portal backend.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Vault passwords and prompt contents pass through this process's
    // memory; keep them out of core dumps. Best effort: a prctl failure
    // (e.g. under a restrictive seccomp filter) warns and never aborts
    // startup. The logger is not initialized yet, so the warning goes to
    // stderr directly — the early-error channel this file already uses.
    // SAFETY: prctl(PR_SET_DUMPABLE) takes no pointer arguments; the
    // remaining arguments are ignored (zeroed here).
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        let error = std::io::Error::last_os_error();
        eprintln!("xdg-desktop-portal-aegis: could not disable core dumps: {error}");
    }
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
