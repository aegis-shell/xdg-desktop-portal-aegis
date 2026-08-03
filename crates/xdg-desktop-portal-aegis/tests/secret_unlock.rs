//! End-to-end password-unlock tests for the native Secret portal. A real
//! backend talks to a fake Aegis compositor over scoped IPC and writes the
//! derived portal secret to a client-provided pipe.

use std::sync::Arc;
use std::time::Duration;

use aegis_ipc::{
    ActorCapability, ConnectionCapabilities, Handler, JournalSnapshot, Scope, SecretPromptResult,
    Server,
};
use argon2::Argon2;
use argon2::password_hash::SaltString;
use chacha20poly1305::XChaCha20Poly1305;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::Sha256;
use zbus::blocking::Proxy;
use zbus::zvariant::{Fd, ObjectPath, Value};

mod common;
use common::{
    KillOnDrop, pipe_pair, private_bus, read_all_with_timeout, spawn_daemon, temp_dir,
    wait_for_name,
};

const PORTAL: &str = "org.freedesktop.impl.portal.desktop.aegis";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const PASSWORD: &str = "hunter2";
/// Fixed test salt ("somesalt" in base64).
const SALT_B64: &str = "c29tZXNhbHQ";

fn write_password_vault(secrets_dir: &std::path::Path) {
    std::fs::create_dir_all(secrets_dir).expect("create secrets dir");
    std::fs::write(secrets_dir.join("vault.salt"), SALT_B64).expect("write salt");

    let salt = SaltString::from_b64(SALT_B64).expect("parse salt");
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(PASSWORD.as_bytes(), salt.as_str().as_bytes(), &mut key)
        .expect("derive fixture key");

    let data = serde_json::json!({
        "collections": [{
            "label": "Login",
            "id": "login",
            "items": [{
                "id": "i01",
                "label": "token",
                "attributes": { "k": "v" },
                "secret": [115, 51, 99, 114, 51, 116]
            }]
        }]
    });
    let plaintext = serde_json::to_vec(&data).expect("serialize fixture");
    let cipher = XChaCha20Poly1305::new((&key).into());
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ciphertext = cipher.encrypt(&nonce, plaintext.as_ref()).expect("encrypt");
    let mut file = nonce.to_vec();
    file.extend_from_slice(&ciphertext);
    let vault_path = secrets_dir.join("vault.enc");
    std::fs::write(&vault_path, file).expect("write vault");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&vault_path, std::fs::Permissions::from_mode(0o600))
        .expect("make vault private");
}

struct FakeCompositor {
    answer: SecretPromptResult,
    grants: std::sync::Mutex<aegis_authority::ResourceGrantRegistry>,
}

impl Handler for FakeCompositor {
    fn policy_caps(&self) -> ConnectionCapabilities {
        ConnectionCapabilities {
            query: true,
            control: true,
            input: false,
            session: false,
            interaction_domain: false,
        }
    }

    fn issue_resource_grant(
        &self,
        session: aegis_authority::ActorSessionId,
        principal: Option<&str>,
        resource: aegis_authority::ActorResource,
        ttl: Duration,
        uses: u32,
        _confirm_exact_resource: bool,
    ) -> Result<aegis_authority::ResourceGrant, String> {
        let principal = principal
            .map(aegis_authority::ActorPrincipal::new)
            .transpose()
            .map_err(str::to_owned)?;
        self.grants.lock().unwrap().issue(
            session,
            principal,
            resource.required_capability(),
            resource,
            ttl,
            uses,
        )
    }

    fn consume_resource_grant(
        &self,
        session: aegis_authority::ActorSessionId,
        principal: Option<&str>,
        id: &aegis_authority::ResourceGrantId,
        resource: &aegis_authority::ActorResource,
    ) -> Result<aegis_authority::ResourceGrant, String> {
        let principal = principal
            .map(aegis_authority::ActorPrincipal::new)
            .transpose()
            .map_err(str::to_owned)?;
        self.grants
            .lock()
            .unwrap()
            .consume(session, principal.as_ref(), id, resource)
    }

    fn windows(&self) -> Vec<aegis_core::window::Window> {
        Vec::new()
    }

    fn workspaces(&self) -> aegis_core::workspace::WorkspaceSnapshot {
        aegis_core::workspace::WorkspaceSnapshot { outputs: vec![] }
    }

    fn notifications(&self) -> Vec<aegis_core::notify::Notification> {
        Vec::new()
    }

    fn outputs(&self) -> Vec<aegis_core::output::OutputInfo> {
        Vec::new()
    }

    fn journal_since(&self, _since: u64) -> JournalSnapshot {
        JournalSnapshot {
            entries: vec![],
            oldest_seq: 1,
            latest_seq: 0,
        }
    }

    fn command(&self, _conn_id: u64, _subject: Option<&str>, _cmd: aegis_ipc::Command) {}

    fn resolve_scope(&self, name: &str) -> Option<Scope> {
        (name == aegis_ipc::LOCAL_PORTAL_SCOPE).then(|| Scope {
            ops: Some(vec![ActorCapability::PromptSecret]),
            ..Scope::default()
        })
    }

    fn prompt_secret(
        &self,
        _conn_id: u64,
        _title: String,
        _reason: Option<String>,
    ) -> Result<SecretPromptResult, String> {
        Ok(self.answer.clone())
    }
}

fn expected_portal_secret_password_mode() -> [u8; 32] {
    let salt = SaltString::from_b64(SALT_B64).expect("parse salt");
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(PASSWORD.as_bytes(), salt.as_str().as_bytes(), &mut key)
        .expect("derive fixture key");
    let hk = Hkdf::<Sha256>::new(None, &key);
    let mut out = [0u8; 32];
    hk.expand(b"aegis.portal.Secret/v1\0dev.aegis.locked", &mut out)
        .expect("expand");
    out
}

/// Run RetrieveSecret against a locked vault. `None` means dbus-daemon is
/// unavailable and the caller should skip.
fn retrieve_secret_while_locked(answer: SecretPromptResult) -> Option<(u32, Vec<u8>)> {
    let bus = private_bus()?;
    let conn = bus.connect();
    let data_dir = temp_dir("data");
    write_password_vault(&data_dir.join("aegis/secrets"));
    let runtime_dir = temp_dir("runtime");

    let fake = Arc::new(FakeCompositor {
        answer,
        grants: std::sync::Mutex::new(aegis_authority::ResourceGrantRegistry::default()),
    });
    let _server = Server::start(&runtime_dir.join("aegis.sock"), Arc::clone(&fake))
        .expect("bind fake compositor IPC");
    let _daemon = KillOnDrop(spawn_daemon(&bus, &data_dir, &runtime_dir));
    wait_for_name(&conn, PORTAL);

    let portal = Proxy::new(
        &conn,
        PORTAL,
        DESKTOP_PATH,
        "org.freedesktop.impl.portal.Secret",
    )
    .expect("portal proxy");
    let (read_end, write_end) = pipe_pair();
    let fd: Fd<'_> = Fd::from(write_end);
    let handle = ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1/locked")
        .expect("request handle path");
    let (response, _): (
        u32,
        std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) = portal
        .call(
            "RetrieveSecret",
            &(
                handle,
                "dev.aegis.locked",
                fd,
                std::collections::HashMap::<String, Value<'_>>::new(),
            ),
        )
        .expect("RetrieveSecret call");
    let bytes = read_all_with_timeout(read_end, Duration::from_secs(5));

    let _ = std::fs::remove_dir_all(&data_dir);
    let _ = std::fs::remove_dir_all(&runtime_dir);
    Some((response, bytes))
}

#[test]
fn retrieve_secret_while_locked_prompts_then_delivers() {
    let Some((response, bytes)) = retrieve_secret_while_locked(SecretPromptResult::Secret {
        value: PASSWORD.to_string(),
    }) else {
        eprintln!("retrieve secret locked: no dbus-daemon, skipping");
        return;
    };
    assert_eq!(response, 0, "RetrieveSecret must succeed after unlock");
    assert_eq!(bytes.as_slice(), &expected_portal_secret_password_mode());
}

#[test]
fn retrieve_secret_dismissed_reports_cancelled() {
    let Some((response, bytes)) = retrieve_secret_while_locked(SecretPromptResult::Cancelled)
    else {
        eprintln!("retrieve secret dismissed: no dbus-daemon, skipping");
        return;
    };
    assert_eq!(response, 1, "a dismissed prompt must report cancelled");
    assert!(bytes.is_empty(), "dismissal must not write secret bytes");
}
