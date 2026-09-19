//! Protected persistence for queued Core video inputs.
//!
//! Core deliberately stores only an input digest.  This module keeps the
//! transient adapter body outside Core and encrypts it with the current
//! Windows user's DPAPI before writing it to the application data directory.
//! The file name is an opaque job id; no prompt or request body is used as a
//! path component.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone)]
pub struct VideoPayloadStore {
    root: PathBuf,
    lock: Arc<Mutex<()>>,
}

const PAYLOAD_VERSION: u8 = 1;
const MAX_PLAINTEXT_BYTES: usize = 8 * 1024 * 1024;

impl VideoPayloadStore {
    pub fn new(data_dir: impl AsRef<Path>) -> Self {
        Self {
            root: data_dir.as_ref().join("data").join("video_payloads"),
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn put(
        &self,
        job_id: &str,
        user_id: &str,
        input_hash: &[u8],
        body: &Value,
    ) -> Result<(), String> {
        validate_job_id(job_id)?;
        validate_user_id(user_id)?;
        validate_input_hash(input_hash)?;
        let envelope = PayloadEnvelope {
            version: PAYLOAD_VERSION,
            job_id: job_id.to_string(),
            user_id: user_id.to_string(),
            input_hash: input_hash.to_vec(),
            body: body.clone(),
        };
        let plaintext = serde_json::to_vec(&envelope)
            .map_err(|_| "video payload serialization failed".to_string())?;
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err("video payload exceeds the 8MB protected limit".into());
        }
        let protected = crate::vault::protect_blob(&plaintext)?;

        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        std::fs::create_dir_all(&self.root)
            .map_err(|_| "video payload directory is unavailable".to_string())?;
        let path = self.path_for(job_id);
        if path.exists() {
            let existing = self.read_envelope_locked(&path)?;
            if existing == envelope {
                return Ok(());
            }
            return Err("video payload conflicts with the existing job".into());
        }

        // Write-then-rename prevents a crash from leaving a partially written
        // ciphertext at the authoritative job path.  A random suffix keeps
        // concurrent process instances from sharing a temporary path.
        let temp_path = self.root.join(format!(".{job_id}.{}.tmp", rand::random::<u64>()));
        std::fs::write(&temp_path, protected)
            .map_err(|_| "video payload could not be written".to_string())?;
        match std::fs::rename(&temp_path, &path) {
            Ok(()) => Ok(()),
            Err(_) if path.exists() => {
                let _ = std::fs::remove_file(&temp_path);
                let existing = self.read_envelope_locked(&path)?;
                if existing == envelope {
                    Ok(())
                } else {
                    Err("video payload conflicts with the existing job".into())
                }
            }
            Err(_) => {
                let _ = std::fs::remove_file(&temp_path);
                Err("video payload could not be committed".into())
            }
        }
    }

    pub fn get(
        &self,
        job_id: &str,
        user_id: &str,
        input_hash: &[u8],
    ) -> Result<Option<Value>, String> {
        validate_job_id(job_id)?;
        validate_user_id(user_id)?;
        validate_input_hash(input_hash)?;
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        let path = self.path_for(job_id);
        if !path.exists() {
            return Ok(None);
        }
        let envelope = self.read_envelope_locked(&path)?;
        if envelope.job_id != job_id
            || envelope.user_id != user_id
            || envelope.input_hash.as_slice() != input_hash
        {
            return Err("video payload ownership or digest mismatch".into());
        }
        Ok(Some(envelope.body))
    }

    pub fn remove(&self, job_id: &str) -> Result<(), String> {
        validate_job_id(job_id)?;
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        match std::fs::remove_file(self.path_for(job_id)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err("video payload could not be removed".into()),
        }
    }

    fn path_for(&self, job_id: &str) -> PathBuf {
        self.root.join(format!("{job_id}.bin"))
    }

    fn read_envelope_locked(&self, path: &Path) -> Result<PayloadEnvelope, String> {
        let protected = std::fs::read(path)
            .map_err(|_| "video payload could not be read".to_string())?;
        let plaintext = crate::vault::unprotect_blob(&protected)?;
        if plaintext.len() > MAX_PLAINTEXT_BYTES {
            return Err("video payload exceeds the protected limit".into());
        }
        let envelope: PayloadEnvelope = serde_json::from_slice(&plaintext)
            .map_err(|_| "video payload is invalid".to_string())?;
        if envelope.version != PAYLOAD_VERSION
            || envelope.job_id.is_empty()
            || envelope.input_hash.len() != 32
        {
            return Err("video payload metadata is invalid".into());
        }
        Ok(envelope)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PayloadEnvelope {
    version: u8,
    job_id: String,
    user_id: String,
    input_hash: Vec<u8>,
    body: Value,
}

fn validate_job_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("video payload job id is invalid".into());
    }
    Ok(())
}

fn validate_user_id(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err("video payload owner is invalid".into());
    }
    Ok(())
}

fn validate_input_hash(value: &[u8]) -> Result<(), String> {
    if value.len() != 32 {
        return Err("video payload digest is invalid".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_payload_round_trips_and_enforces_owner_and_digest() {
        let dir = std::env::temp_dir().join(format!(
            "aiwork-video-payload-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let store = VideoPayloadStore::new(&dir);
        let body = serde_json::json!({"model":"mock-video","prompt":"secret"});
        let digest = [7_u8; 32];

        store.put("job-1", "user-1", &digest, &body).unwrap();
        let ciphertext = std::fs::read(store.path_for("job-1")).unwrap();
        assert!(!String::from_utf8_lossy(&ciphertext).contains("secret"));
        assert_eq!(store.get("job-1", "user-1", &digest).unwrap(), Some(body));
        assert!(store.get("job-1", "user-2", &digest).is_err());
        assert!(store.get("job-1", "user-1", &[8_u8; 32]).is_err());
        store.remove("job-1").unwrap();
        assert_eq!(store.get("job-1", "user-1", &digest).unwrap(), None);

        let _ = std::fs::remove_dir_all(dir);
    }
}
