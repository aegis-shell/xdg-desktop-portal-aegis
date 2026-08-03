//! At-rest vault crypto for the secret store.
//!
//! The vault file is `serde_json` serialized [`VaultData`] encrypted with
//! XChaCha20-Poly1305 under the 32-byte master key; on disk it is a 24-byte
//! nonce prefix followed by the ciphertext. The format is byte-compatible
//! with the wssp vault so an existing `vault.enc` keeps working.
//!
//! Startup uses keyfile mode (`vault.key` holding the master key as hex).
//! The Argon2id password KDF (`derive_key`/`generate_salt`) is kept for the
//! future password-unlock slice that lands together with the native
//! prompter.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use argon2::Argon2;
use argon2::password_hash::SaltString;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rand::RngCore;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use zeroize::{Zeroize, Zeroizing};

use super::SecretError;

const MAX_VAULT_BYTES: u64 = 64 * 1024 * 1024;

/// The decrypted vault contents: every persisted collection and item.
#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct VaultData {
    pub collections: Vec<CollectionData>,
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct CollectionData {
    pub id: String,
    pub label: String,
    pub items: Vec<ItemData>,
}

#[derive(Serialize, Deserialize, Zeroize)]
#[zeroize(drop)]
pub struct ItemData {
    pub id: String,
    pub label: String,
    #[zeroize(skip)]
    pub attributes: std::collections::HashMap<String, String>,
    pub secret: Vec<u8>,
}

/// An open vault: its file location plus the master key in memory.
pub struct Vault {
    path: PathBuf,
    master_key: [u8; 32],
}

impl Drop for Vault {
    fn drop(&mut self) {
        self.master_key.zeroize();
    }
}

impl Vault {
    pub fn new(path: PathBuf, master_key: [u8; 32]) -> Self {
        Self { path, master_key }
    }

    /// The in-memory master key. Callers must derive purpose-separated keys
    /// from it (HKDF) instead of handing it out directly.
    pub fn get_master_key(&self) -> &[u8; 32] {
        &self.master_key
    }

    /// Argon2id password KDF for password-mode vaults. Unused while startup
    /// only supports keyfile mode; kept for the password-unlock slice.
    pub fn derive_key(password: &str, salt_str: &str) -> Result<[u8; 32], SecretError> {
        let salt = SaltString::from_b64(salt_str)
            .map_err(|e| SecretError::Crypto(format!("invalid salt: {e}")))?;
        let argon2 = Argon2::default();

        let mut key = [0u8; 32];
        if let Err(error) =
            argon2.hash_password_into(password.as_bytes(), salt.as_str().as_bytes(), &mut key)
        {
            key.zeroize();
            return Err(SecretError::Crypto(format!("hash failed: {error}")));
        }

        Ok(key)
    }

    /// Fresh Argon2id salt for password mode; see `derive_key`.
    #[allow(dead_code)]
    pub fn generate_salt() -> String {
        SaltString::generate(&mut OsRng).as_str().to_string()
    }

    /// Fresh random master key for keyfile mode.
    pub fn generate_key() -> [u8; 32] {
        let mut key = [0u8; 32];
        OsRng.fill_bytes(&mut key);
        key
    }

    pub fn key_to_hex(key: &[u8; 32]) -> String {
        key.iter().map(|b| format!("{b:02x}")).collect()
    }

    pub fn key_from_hex(hex: &str) -> Result<[u8; 32], SecretError> {
        let hex = hex.trim();
        if hex.len() != 64 {
            return Err(SecretError::Crypto(
                "vault.key must be 64 hex chars (32 bytes)".into(),
            ));
        }
        let mut arr = [0u8; 32];
        for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
            let s = match std::str::from_utf8(chunk) {
                Ok(value) => value,
                Err(_) => {
                    arr.zeroize();
                    return Err(SecretError::Crypto("invalid hex in vault.key".into()));
                }
            };
            arr[i] = match u8::from_str_radix(s, 16) {
                Ok(value) => value,
                Err(_) => {
                    arr.zeroize();
                    return Err(SecretError::Crypto(format!("invalid hex byte: {s}")));
                }
            };
        }
        Ok(arr)
    }

    /// Validate a locked vault's file boundary without needing its key.
    /// Authenticated decryption still happens before the vault is unlocked.
    pub(crate) fn validate_ciphertext(path: &Path) -> Result<(), SecretError> {
        let Some(file_data) = read_vault_file(path, false)? else {
            return Err(SecretError::Vault("vault file is missing".to_owned()));
        };
        if file_data.len() < 24 {
            return Err(SecretError::Vault("vault file corrupted".to_string()));
        }
        Ok(())
    }

    /// Encrypt and persist the vault contents (24-byte nonce prefix +
    /// ciphertext).
    pub fn save(&self, data: &VaultData) -> Result<(), SecretError> {
        let serialized = Zeroizing::new(serde_json::to_vec(data)?);
        let cipher = XChaCha20Poly1305::new(&self.master_key.into());
        let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
        let ciphertext = cipher
            .encrypt(&nonce, serialized.as_ref())
            .map_err(|e| SecretError::Crypto(format!("encryption failure: {e}")))?;

        let mut final_data = nonce.to_vec();
        final_data.extend_from_slice(&ciphertext);

        atomic_replace(&self.path, &final_data).map_err(SecretError::Io)
    }

    /// Load and decrypt the vault. A missing file reads as an empty vault; a
    /// truncated or undecryptable file is an error.
    pub fn load(&self) -> Result<VaultData, SecretError> {
        let Some(file_data) = read_vault_file(&self.path, true)? else {
            return Ok(VaultData {
                collections: vec![],
            });
        };
        if file_data.len() < 24 {
            return Err(SecretError::Vault("vault file corrupted".to_string()));
        }

        let (nonce_bytes, ciphertext) = file_data.split_at(24);
        let nonce = XNonce::from_slice(nonce_bytes);

        let cipher = XChaCha20Poly1305::new(&self.master_key.into());
        let plaintext = Zeroizing::new(
            cipher
                .decrypt(nonce, ciphertext)
                .map_err(|e| SecretError::Crypto(format!("decryption failure: {e}")))?,
        );

        let data: VaultData = serde_json::from_slice(plaintext.as_ref())?;
        Ok(data)
    }
}

fn read_vault_file(path: &Path, missing_is_empty: bool) -> Result<Option<Vec<u8>>, SecretError> {
    let mut file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if missing_is_empty && error.kind() == io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(SecretError::Io(error)),
    };
    let metadata = file.metadata()?;
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    if !metadata.is_file()
        || metadata.uid() != uid
        || metadata.permissions().mode() & 0o7777 != 0o600
        || metadata.len() > MAX_VAULT_BYTES
    {
        return Err(SecretError::Io(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "vault.enc must be a user-owned regular file, mode 0600, at most 64 MiB",
        )));
    }
    let mut file_data = Vec::with_capacity(metadata.len() as usize);
    file.read_to_end(&mut file_data)?;
    Ok(Some(file_data))
}

/// Durably replace one file without ever truncating the previous version.
/// The temporary file lives beside the destination so rename is atomic.
pub(crate) fn atomic_replace(path: &Path, contents: &[u8]) -> io::Result<()> {
    atomic_replace_with(path, contents, || Ok(()))
}

/// Durably create a private file without replacing an existing path. A hard
/// link publishes a fully written same-directory temporary atomically; this
/// closes the first-start race between two activated backend processes.
pub(crate) fn atomic_create(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "vault path must have a parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "vault path must name a file")
    })?;
    let temporary_path = temporary_path(parent, file_name);
    let result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)?;
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        fs::hard_link(&temporary_path, path)?;
        fs::remove_file(&temporary_path)?;
        File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn atomic_replace_with<F>(path: &Path, contents: &[u8], before_rename: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()>,
{
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "vault path must have a parent directory",
        )
    })?;
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "vault path must name a file")
    })?;

    let temporary_path = temporary_path(parent, file_name);

    let result = (|| {
        let mut temporary = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary_path)?;
        temporary.write_all(contents)?;
        temporary.sync_all()?;
        before_rename()?;
        fs::rename(&temporary_path, path)?;

        // Persist the directory entry replacement, not just the file data.
        File::open(parent)?.sync_all()
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}

fn temporary_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let mut random = [0u8; 16];
    OsRng.fill_bytes(&mut random);
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let mut temporary_name = std::ffi::OsString::from(".");
    temporary_name.push(file_name);
    temporary_name.push(format!(".{suffix}.tmp"));
    parent.join(temporary_name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn vault_crypto_roundtrip() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let vault_path = std::env::temp_dir().join(format!("aegis-vault-test-{suffix}.enc"));

        let password = "super-secret-password";
        let salt = Vault::generate_salt();
        let key = Vault::derive_key(password, &salt).expect("key derivation failed");

        let vault = Vault::new(vault_path.clone(), key);
        let data = VaultData {
            collections: vec![CollectionData {
                id: "login".into(),
                label: "Login".into(),
                items: vec![ItemData {
                    id: "i1".into(),
                    label: "Item".into(),
                    attributes: [("app".to_string(), "aegis".to_string())]
                        .into_iter()
                        .collect(),
                    secret: b"hunter2".to_vec(),
                }],
            }],
        };

        vault.save(&data).expect("save failed");
        let loaded = vault.load().expect("load failed");
        assert_eq!(loaded.collections.len(), 1);
        assert_eq!(loaded.collections[0].id, "login");
        assert_eq!(loaded.collections[0].items[0].secret, b"hunter2");

        let _ = std::fs::remove_file(vault_path);
    }

    #[test]
    fn atomic_replace_preserves_previous_file_before_rename() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-atomic-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.enc");
        fs::write(&path, b"previous valid vault").unwrap();

        let error = atomic_replace_with(&path, b"replacement", || {
            Err(io::Error::other("injected failure before rename"))
        })
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(fs::read(&path).unwrap(), b"previous valid vault");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_replace_creates_private_file() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-mode-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.enc");

        atomic_replace(&path, b"ciphertext").unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"ciphertext");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn atomic_create_never_replaces_an_existing_file() {
        let mut suffix = [0u8; 8];
        OsRng.fill_bytes(&mut suffix);
        let suffix: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let directory = std::env::temp_dir().join(format!("aegis-vault-create-{suffix}"));
        fs::create_dir(&directory).unwrap();
        let path = directory.join("vault.key");
        atomic_create(&path, b"first").unwrap();
        assert_eq!(
            atomic_create(&path, b"second").unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"first");
        assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
        fs::remove_dir_all(directory).unwrap();
    }
}
