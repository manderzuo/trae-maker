use std::{collections::HashMap, sync::Mutex};

use aiwork_core::{AdminCredentialRecord, CoreError, CoreStore, NewAdminCredential};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use pbkdf2::pbkdf2_hmac;
use rand::{rngs::OsRng, RngCore};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub const PASSWORD_ITERATIONS: u32 = 600_000;
pub const SESSION_TTL_MS: i64 = 12 * 60 * 60 * 1_000;
const SALT_BYTES: usize = 16;
const DERIVED_KEY_BYTES: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PasswordHash {
    pub hash: String,
    pub salt: String,
    pub iterations: u32,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthSetupError {
    #[error("core credential storage failed: {0}")]
    Core(#[from] CoreError),
    #[error("admin password must not be empty")]
    EmptyPassword,
}

pub fn hash_password(password: &str) -> Result<PasswordHash, AuthSetupError> {
    if password.is_empty() {
        return Err(AuthSetupError::EmptyPassword);
    }
    let mut salt = [0_u8; SALT_BYTES];
    OsRng.fill_bytes(&mut salt);
    let mut derived = [0_u8; DERIVED_KEY_BYTES];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, PASSWORD_ITERATIONS, &mut derived);
    Ok(PasswordHash {
        hash: URL_SAFE_NO_PAD.encode(derived),
        salt: URL_SAFE_NO_PAD.encode(salt),
        iterations: PASSWORD_ITERATIONS,
    })
}

pub fn verify_password(password: &str, record: &AdminCredentialRecord) -> bool {
    let Ok(salt) = URL_SAFE_NO_PAD.decode(record.salt.as_bytes()) else { return false };
    let Ok(expected) = URL_SAFE_NO_PAD.decode(record.password_hash.as_bytes()) else { return false };
    if salt.is_empty() || expected.len() != DERIVED_KEY_BYTES || record.iterations < 100_000 { return false }
    let mut derived = [0_u8; DERIVED_KEY_BYTES];
    pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, record.iterations, &mut derived);
    derived.as_slice().ct_eq(expected.as_slice()).into()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminSession {
    pub user_id: String,
    pub token_hash: String,
    pub csrf_token: String,
    pub expires_at_ms: i64,
}

pub struct AdminSessionStore {
    ttl_ms: i64,
    sessions: Mutex<HashMap<String, AdminSession>>,
}

impl AdminSessionStore {
    pub fn new(ttl_ms: i64) -> Self {
        Self { ttl_ms, sessions: Mutex::new(HashMap::new()) }
    }

    pub fn issue(&self, user_id: String, now_ms: i64) -> (String, AdminSession) {
        let token = random_token();
        let token_hash = digest_token(&token);
        let session = AdminSession {
            user_id,
            token_hash: token_hash.clone(),
            csrf_token: random_token(),
            expires_at_ms: now_ms.saturating_add(self.ttl_ms),
        };
        self.sessions.lock().expect("admin session mutex poisoned").insert(token_hash, session.clone());
        (token, session)
    }

    pub fn lookup(&self, token: &str, now_ms: i64) -> Option<AdminSession> {
        let token_hash = digest_token(token);
        let mut sessions = self.sessions.lock().expect("admin session mutex poisoned");
        let session = sessions.get(&token_hash).cloned();
        if session.as_ref().is_some_and(|value| value.expires_at_ms <= now_ms) {
            sessions.remove(&token_hash);
            return None;
        }
        session
    }

    pub fn revoke(&self, token: &str) {
        self.sessions.lock().expect("admin session mutex poisoned").remove(&digest_token(token));
    }

    pub fn revoke_user(&self, user_id: &str) {
        self.sessions.lock().expect("admin session mutex poisoned").retain(|_, session| session.user_id != user_id);
    }
}

#[derive(Debug, Clone)]
struct AttemptWindow { started_at_ms: i64, failures: u32 }

pub struct LoginThrottle {
    attempts: Mutex<HashMap<String, AttemptWindow>>,
}

impl LoginThrottle {
    pub fn new() -> Self { Self { attempts: Mutex::new(HashMap::new()) } }

    pub fn allow(&self, key: &str, now_ms: i64) -> bool {
        let mut attempts = self.attempts.lock().expect("login throttle mutex poisoned");
        let entry = attempts.entry(key.to_owned()).or_insert(AttemptWindow { started_at_ms: now_ms, failures: 0 });
        if now_ms.saturating_sub(entry.started_at_ms) >= 15 * 60 * 1_000 {
            *entry = AttemptWindow { started_at_ms: now_ms, failures: 0 };
        }
        entry.failures < 5
    }

    pub fn record_failure(&self, key: &str, now_ms: i64) {
        let mut attempts = self.attempts.lock().expect("login throttle mutex poisoned");
        let entry = attempts.entry(key.to_owned()).or_insert(AttemptWindow { started_at_ms: now_ms, failures: 0 });
        if now_ms.saturating_sub(entry.started_at_ms) >= 15 * 60 * 1_000 {
            *entry = AttemptWindow { started_at_ms: now_ms, failures: 0 };
        }
        entry.failures = entry.failures.saturating_add(1);
    }

    pub fn clear(&self, key: &str) {
        self.attempts.lock().expect("login throttle mutex poisoned").remove(key);
    }
}

pub fn ensure_initial_admin_credential(store: &CoreStore, initial_password: Option<&str>) -> Result<(), AuthSetupError> {
    if store.find_admin_credential("admin")?.is_some() {
        return Ok(());
    }
    let Some(password) = initial_password else { return Ok(()) };
    let hash = hash_password(password)?;
    match store.upsert_admin_credential(NewAdminCredential {
        user_id: "admin".into(),
        username: "admin".into(),
        password_hash: hash.hash,
        salt: hash.salt,
        iterations: hash.iterations,
        must_change_password: true,
    }) {
        Ok(()) | Err(CoreError::AdminRequired) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn random_token() -> String {
    let mut material = [0_u8; 32];
    OsRng.fill_bytes(&mut material);
    URL_SAFE_NO_PAD.encode(material)
}

fn digest_token(token: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(token.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify_password_value(password: &str, value: &PasswordHash) -> bool {
        let record = AdminCredentialRecord { user_id: "admin".into(), username: "admin".into(), password_hash: value.hash.clone(), salt: value.salt.clone(), iterations: value.iterations, must_change_password: true };
        verify_password(password, &record)
    }

    #[test]
    fn password_hash_verifies_and_wrong_password_fails() {
        let first = hash_password("test-password").unwrap();
        let second = hash_password("test-password").unwrap();
        assert!(verify_password_value("test-password", &first));
        assert!(!verify_password_value("wrong-password", &first));
        assert_ne!(first.salt, second.salt);
        assert_ne!(first.hash, second.hash);
    }

    #[test]
    fn sessions_expire_and_revoke() {
        let sessions = AdminSessionStore::new(100);
        let (token, session) = sessions.issue("admin".into(), 1_000);
        assert_eq!(sessions.lookup(&token, 1_050).unwrap().user_id, "admin");
        assert!(sessions.lookup(&token, session.expires_at_ms + 1).is_none());
        let (token2, _) = sessions.issue("admin".into(), 2_000);
        sessions.revoke(&token2);
        assert!(sessions.lookup(&token2, 2_001).is_none());
    }
}
