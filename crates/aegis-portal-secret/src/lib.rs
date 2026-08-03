//! Native `org.freedesktop.impl.portal.Secret` storage for Aegis.
//!
//! The public portal secret is derived from a private, encrypted vault key.
//! Password-mode vaults can unlock from a one-shot PAM token or the
//! compositor's masked prompt. This crate deliberately does not implement
//! `org.freedesktop.secrets`; claiming that separate API without its full
//! locking, alias, prompt, and collection semantics would be unsafe.

mod portal;
mod vault;

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zeroize::{Zeroize, Zeroizing};

use vault::{Vault, VaultData};

const PAM_TOKEN_NAME: &str = "aegis-pam-token";
const MAX_PAM_TOKEN_BYTES: u64 = 64 * 1024;
const MAX_KEYFILE_BYTES: u64 = 1024;
const MAX_PENDING_UNLOCKS: usize = 64;

/// Result of asking the desktop shell for the vault password.
pub enum PromptResponse {
    /// The user confirmed a password. This crate zeroizes it immediately
    /// after it crosses the boundary.
    Secret(String),
    /// The user dismissed the prompt without submitting a password.
    Cancelled,
}

/// Narrow host capability required to unlock a password-protected vault.
pub trait SecretPrompter: Send + Sync + 'static {
    fn prompt_secret(&self, title: &str, reason: Option<&str>) -> Result<PromptResponse, String>;
}

/// Native Secret portal service and its shared unlock coordinator.
pub struct SecretService {
    state: Arc<Mutex<SecretState>>,
    prompter: Arc<dyn SecretPrompter>,
}

impl SecretService {
    /// Open or create the user's vault and bind its unlock path to `prompter`.
    pub fn initialize(prompter: Arc<dyn SecretPrompter>) -> Result<Self, SecretError> {
        let state = init()?;
        Ok(Self { state, prompter })
    }

    /// Register the native Secret backend interface.
    pub fn register_portal(
        &self,
        conn: &zbus::blocking::Connection,
        tracker: Arc<Mutex<aegis_portal_runtime::RequestTracker>>,
        path: &str,
    ) -> zbus::Result<()> {
        conn.object_server().at(
            path,
            portal::SecretIface {
                conn: conn.clone(),
                tracker,
                state: Arc::clone(&self.state),
                prompter: Arc::clone(&self.prompter),
            },
        )?;
        Ok(())
    }

    /// Watch for a PAM token that arrives after backend startup (for example
    /// on screen unlock). The watcher exits permanently once the vault is
    /// unlocked and never exposes the token to D-Bus.
    pub fn start_pam_watcher(&self) {
        if self.state.lock().unwrap().is_unlocked() {
            return;
        }
        let state = Arc::clone(&self.state);
        let spawned = std::thread::Builder::new()
            .name("aegis-pam-token-watcher".to_owned())
            .spawn(move || {
                loop {
                    if state.lock().unwrap().is_unlocked() {
                        return;
                    }
                    if let Some(password) = consume_pam_token() {
                        match state.lock().unwrap().unlock_with_password(&password) {
                            Ok(()) => {
                                log::info!("portal: secret vault unlocked by a new PAM token");
                                return;
                            }
                            Err(error) => log::warn!("portal: PAM-token unlock failed: {error}"),
                        }
                    }
                    std::thread::sleep(Duration::from_millis(500));
                }
            });
        if let Err(error) = spawned {
            log::error!("portal: could not start PAM-token watcher: {error}");
        }
    }
}

/// Errors that keep secret support from coming up at all.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// No XDG data directory is available.
    #[error("no XDG data directory available")]
    NoDataDir,
    /// Filesystem failure around the vault directory or files.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Key derivation, encryption, or decryption failed.
    #[error("crypto error: {0}")]
    Crypto(String),
    /// The vault file is truncated or otherwise malformed.
    #[error("vault error: {0}")]
    Vault(String),
    /// (De)serialization of the vault contents failed.
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
}

struct SecretState {
    pub(crate) vault: Option<Vault>,
    pub(crate) pending_unlocks: Vec<PendingUnlock>,
    pub(crate) unlock_worker_active: bool,
    pub(crate) vault_path: PathBuf,
    pub(crate) salt_path: PathBuf,
}

impl SecretState {
    pub(crate) fn is_unlocked(&self) -> bool {
        self.vault.is_some()
    }

    fn unlock_with_password(&mut self, password: &str) -> Result<(), SecretError> {
        if !self.vault_path.exists() {
            return Err(SecretError::Vault(
                "password-mode vault is missing vault.enc".to_owned(),
            ));
        }
        let salt = read_regular_file(&self.salt_path, 1024, false)?;
        let salt = std::str::from_utf8(&salt)
            .map_err(|_| SecretError::Crypto("vault.salt is not UTF-8".to_owned()))?;
        let mut key = Vault::derive_key(password, salt.trim())?;
        let vault = Vault::new(self.vault_path.clone(), key);
        key.zeroize();
        // Authenticated decryption validates the password and the complete
        // legacy vault before the master key becomes live.
        let _validated = vault.load()?;
        self.vault = Some(vault);
        Ok(())
    }
}

/// The result reported to a native RetrieveSecret request waiting for an
/// unlock.
pub(crate) enum PortalUnlockOutcome {
    Delivered,
    Dismissed,
    Failed,
}

pub(crate) struct PendingUnlock {
    pub(crate) fd: OwnedFd,
    pub(crate) outcome: async_channel::Sender<PortalUnlockOutcome>,
    pub(crate) tracker: Arc<Mutex<aegis_portal_runtime::RequestTracker>>,
    pub(crate) request_path: String,
    pub(crate) app_id: String,
}

pub(crate) fn write_secret_fd(fd: OwnedFd, secret: &[u8]) -> std::io::Result<()> {
    std::fs::File::from(fd).write_all(secret)
}

/// Queue one caller behind the shared unlock prompt. One worker services the
/// whole batch, while every request retains its own cancellation check.
pub(crate) fn enqueue_unlock_request(
    state: &Arc<Mutex<SecretState>>,
    prompter: &Arc<dyn SecretPrompter>,
    request: PendingUnlock,
) {
    let (spawn, rejected) = {
        let mut state = state.lock().unwrap();
        if state.pending_unlocks.len() >= MAX_PENDING_UNLOCKS {
            (false, Some(request))
        } else {
            state.pending_unlocks.push(request);
            if state.unlock_worker_active {
                (false, None)
            } else {
                state.unlock_worker_active = true;
                (true, None)
            }
        }
    };
    if let Some(request) = rejected {
        log::warn!("portal: refusing RetrieveSecret request: unlock queue limit reached");
        let _ = request.outcome.send_blocking(PortalUnlockOutcome::Failed);
        return;
    }
    if !spawn {
        return;
    }

    let worker_state = Arc::clone(state);
    let worker_prompter = Arc::clone(prompter);
    let spawned = std::thread::Builder::new()
        .name("aegis-portal-unlock".to_owned())
        .spawn(move || unlock_worker(worker_state, worker_prompter));
    if let Err(error) = spawned {
        log::error!("portal: could not spawn unlock worker: {error}");
        let requests = {
            let mut state = state.lock().unwrap();
            state.unlock_worker_active = false;
            std::mem::take(&mut state.pending_unlocks)
        };
        complete_unlock_requests(state, requests, false);
    }
}

fn unlock_worker(state: Arc<Mutex<SecretState>>, prompter: Arc<dyn SecretPrompter>) {
    loop {
        let requests = {
            let mut state = state.lock().unwrap();
            let requests = std::mem::take(&mut state.pending_unlocks);
            if requests.is_empty() {
                state.unlock_worker_active = false;
            }
            requests
        };
        if requests.is_empty() {
            return;
        }

        let unlocked = if state.lock().unwrap().is_unlocked() {
            true
        } else {
            match prompt_and_unlock(&state, prompter.as_ref()) {
                Ok(()) => {
                    log::info!("portal: secret vault unlocked via password prompt");
                    true
                }
                Err(reason) => {
                    log::warn!("portal: vault unlock did not complete: {reason}");
                    false
                }
            }
        };
        complete_unlock_requests(&state, requests, unlocked);
    }
}

fn prompt_and_unlock(
    state: &Arc<Mutex<SecretState>>,
    prompter: &dyn SecretPrompter,
) -> Result<(), String> {
    let password = match prompter.prompt_secret(
        "Unlock Keyring",
        Some("The secret vault is locked. Enter its password to unlock it."),
    ) {
        Ok(PromptResponse::Secret(value)) => Zeroizing::new(value),
        Ok(PromptResponse::Cancelled) => return Err("prompt dismissed".into()),
        Err(error) => return Err(format!("secret prompt failed: {error}")),
    };
    state
        .lock()
        .unwrap()
        .unlock_with_password(&password)
        .map_err(|error| format!("wrong password or unreadable vault: {error}"))
}

fn complete_unlock_requests(
    state: &Arc<Mutex<SecretState>>,
    requests: Vec<PendingUnlock>,
    unlocked: bool,
) {
    for request in requests {
        let cancelled = request
            .tracker
            .lock()
            .unwrap()
            .was_closed(&request.request_path);
        let result = if cancelled || !unlocked {
            PortalUnlockOutcome::Dismissed
        } else {
            deliver_portal_secret(state, request.fd, &request.app_id)
        };
        if request.outcome.send_blocking(result).is_err() {
            log::warn!("portal: RetrieveSecret caller went away before unlock completed");
        }
    }
}

fn deliver_portal_secret(
    state: &Arc<Mutex<SecretState>>,
    fd: OwnedFd,
    app_id: &str,
) -> PortalUnlockOutcome {
    let mut secret = {
        let state = state.lock().unwrap();
        match state.vault.as_ref() {
            Some(vault) => portal::derive_portal_secret(vault.get_master_key(), app_id),
            None => return PortalUnlockOutcome::Failed,
        }
    };
    let written = write_secret_fd(fd, &secret);
    secret.zeroize();
    match written {
        Ok(()) => PortalUnlockOutcome::Delivered,
        Err(error) => {
            log::warn!("portal: could not write RetrieveSecret fd after unlock: {error}");
            PortalUnlockOutcome::Failed
        }
    }
}

/// PAM and the backend use the same root-owned session runtime location.
/// Never trust an inherited XDG_RUNTIME_DIR for a login password token.
fn pam_token_path() -> PathBuf {
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    PathBuf::from(format!("/run/user/{uid}/{PAM_TOKEN_NAME}"))
}

fn consume_pam_token() -> Option<Zeroizing<String>> {
    consume_pam_token_at(&pam_token_path())
}

/// Open a one-shot token without following links, validate the opened file,
/// unlink the name before reading, and cap its size.
fn consume_pam_token_at(path: &Path) -> Option<Zeroizing<String>> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.len() > MAX_PAM_TOKEN_BYTES
    {
        log::warn!("portal: refusing unsafe PAM token at {}", path.display());
        let _ = std::fs::remove_file(path);
        return None;
    }
    // Refuse reuse even if parsing or password validation later fails.
    std::fs::remove_file(path).ok()?;

    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.read_to_end(&mut bytes).ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    let password = text.trim_end_matches(['\n', '\r']);
    if password.is_empty() {
        None
    } else {
        Some(Zeroizing::new(password.to_owned()))
    }
}

fn init() -> Result<Arc<Mutex<SecretState>>, SecretError> {
    let dir = dirs::data_dir()
        .ok_or(SecretError::NoDataDir)?
        .join("aegis")
        .join("secrets");
    init_in(&dir)
}

fn init_in(dir: &Path) -> Result<Arc<Mutex<SecretState>>, SecretError> {
    prepare_private_dir(dir)?;

    let key_path = dir.join("vault.key");
    let mut state = SecretState {
        vault: None,
        pending_unlocks: Vec::new(),
        unlock_worker_active: false,
        vault_path: dir.join("vault.enc"),
        salt_path: dir.join("vault.salt"),
    };

    if key_path.exists() {
        let hex = read_regular_file(&key_path, MAX_KEYFILE_BYTES, true)?;
        let hex = std::str::from_utf8(&hex)
            .map_err(|_| SecretError::Crypto("vault.key is not UTF-8".to_owned()))?;
        let mut key = Vault::key_from_hex(hex)?;
        let vault = Vault::new(state.vault_path.clone(), key);
        key.zeroize();
        let missing = !state.vault_path.exists();
        let _validated = vault.load()?;
        if missing {
            vault.save(&VaultData {
                collections: vec![],
            })?;
        }
        state.vault = Some(vault);
        log::info!("portal: secret vault unlocked via keyfile");
    } else if state.salt_path.exists() {
        if !state.vault_path.exists() {
            return Err(SecretError::Vault(
                "password-mode vault has vault.salt but no vault.enc".to_owned(),
            ));
        }
        // A locked vault still advertises Secret, so validate every backing
        // path now rather than deferring symlink/mode/size failures until a
        // caller is already waiting on an unlock prompt.
        let _salt = read_regular_file(&state.salt_path, MAX_KEYFILE_BYTES, false)?;
        Vault::validate_ciphertext(&state.vault_path)?;
        match consume_pam_token() {
            Some(password) => match state.unlock_with_password(&password) {
                Ok(()) => log::info!("portal: secret vault unlocked via PAM token"),
                Err(error) => log::warn!("portal: PAM-token unlock failed: {error}"),
            },
            None => log::info!("portal: password-protected secret vault starts locked"),
        }
    } else {
        if state.vault_path.exists() {
            return Err(SecretError::Vault(
                "refusing to overwrite vault.enc without vault.key or vault.salt".to_owned(),
            ));
        }
        let mut key = Vault::generate_key();
        let encoded = Zeroizing::new(Vault::key_to_hex(&key));
        match vault::atomic_create(&key_path, encoded.as_bytes()) {
            Ok(()) => {
                let vault = Vault::new(state.vault_path.clone(), key);
                key.zeroize();
                vault.save(&VaultData {
                    collections: vec![],
                })?;
                state.vault = Some(vault);
                log::info!("portal: secret vault initialized at {}", dir.display());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                // Another concurrently activated backend won initialization.
                // Use only its fully published key.
                key.zeroize();
                let hex = read_regular_file(&key_path, MAX_KEYFILE_BYTES, true)?;
                let hex = std::str::from_utf8(&hex)
                    .map_err(|_| SecretError::Crypto("vault.key is not UTF-8".to_owned()))?;
                let mut key = Vault::key_from_hex(hex)?;
                let vault = Vault::new(state.vault_path.clone(), key);
                key.zeroize();
                if !state.vault_path.exists() {
                    vault.save(&VaultData {
                        collections: vec![],
                    })?;
                } else {
                    let _validated = vault.load()?;
                }
                state.vault = Some(vault);
            }
            Err(error) => return Err(SecretError::Io(error)),
        }
    }

    Ok(Arc::new(Mutex::new(state)))
}

/// Open the final vault directory without following a symlink. Tighten its
/// mode through the descriptor only after owner/type validation, so a
/// rejected path cannot chmod a link target as a side effect.
fn prepare_private_dir(dir: &Path) -> Result<(), SecretError> {
    std::fs::create_dir_all(dir)?;
    let directory = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(dir)?;
    let metadata = directory.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_dir() || metadata.uid() != uid {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret directory must be a user-owned real directory",
        )));
    }
    directory.set_permissions(std::fs::Permissions::from_mode(0o700))?;
    let metadata = directory.metadata()?;
    if metadata.permissions().mode() & 0o7777 != 0o700 {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "secret directory mode must be 0700",
        )));
    }
    Ok(())
}

/// Read a bounded regular file through an O_NOFOLLOW descriptor. `private`
/// additionally requires exact mode 0600.
fn read_regular_file(
    path: &Path,
    limit: u64,
    private: bool,
) -> Result<Zeroizing<Vec<u8>>, SecretError> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || (private && metadata.permissions().mode() & 0o7777 != 0o600)
        || (!private && metadata.permissions().mode() & 0o022 != 0)
        || metadata.len() > limit
    {
        return Err(SecretError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "unsafe file ownership, mode, type, or size: {}",
                path.display()
            ),
        )));
    }
    let mut bytes = Zeroizing::new(Vec::with_capacity(metadata.len() as usize));
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::RngCore;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut suffix = [0u8; 8];
        rand::rngs::OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|byte| format!("{byte:02x}")).collect();
        std::env::temp_dir().join(format!("aegis-secret-test-{tag}-{suffix}"))
    }

    #[test]
    fn first_run_creates_private_keyfile_and_reopens() {
        let dir = temp_dir("first-run");
        let state = init_in(&dir).expect("first-run init");
        assert!(state.lock().unwrap().is_unlocked());
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(dir.join("vault.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(dir.join("vault.enc").exists());
        assert!(
            init_in(&dir)
                .expect("second init")
                .lock()
                .unwrap()
                .is_unlocked()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn salt_without_ciphertext_is_rejected() {
        let dir = temp_dir("salt-only");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("vault.salt"), "c29tZXNhbHQ").unwrap();
        assert!(init_in(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn orphan_ciphertext_is_never_overwritten() {
        let dir = temp_dir("orphan");
        std::fs::create_dir_all(&dir).unwrap();
        let vault_path = dir.join("vault.enc");
        std::fs::write(&vault_path, b"irreplaceable ciphertext").unwrap();
        let before = std::fs::read(&vault_path).unwrap();

        assert!(init_in(&dir).is_err());
        assert_eq!(std::fs::read(&vault_path).unwrap(), before);
        assert!(!dir.join("vault.key").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn symlink_directory_is_rejected_without_chmodding_target() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("directory-symlink");
        let target = root.join("target");
        let link = root.join("link");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        symlink(&target, &link).unwrap();

        assert!(init_in(&link).is_err());
        assert_eq!(
            std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn locked_vault_rejects_a_symlink_salt_at_startup() {
        use std::os::unix::fs::symlink;

        let root = temp_dir("salt-symlink");
        let dir = root.join("secrets");
        std::fs::create_dir_all(&dir).unwrap();
        let target = root.join("salt-target");
        std::fs::write(&target, b"c29tZXNhbHQ").unwrap();
        symlink(&target, dir.join("vault.salt")).unwrap();
        std::fs::write(dir.join("vault.enc"), [0_u8; 24]).unwrap();
        std::fs::set_permissions(
            dir.join("vault.enc"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();

        assert!(init_in(&dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn pam_token_must_be_regular_private_and_is_one_shot() {
        let dir = temp_dir("pam-token");
        std::fs::create_dir_all(&dir).unwrap();
        let token = dir.join(PAM_TOKEN_NAME);
        std::fs::write(&token, b"password\n").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let consumed = consume_pam_token_at(&token);
        assert_eq!(
            consumed.as_ref().map(|value| value.as_str()),
            Some("password")
        );
        assert!(!token.exists());

        std::fs::write(&token, b"leaked").unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(consume_pam_token_at(&token).is_none());
        assert!(!token.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
