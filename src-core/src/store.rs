use std::{collections::BTreeSet, fs, path::Path, sync::Mutex, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{schema::SCHEMA_V1, AuthError, CoreError, IssuedApiKey, NewUser, Principal, User};

pub const CORE_DB_FILE: &str = "core.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

pub struct CoreStore {
    connection: Mutex<Connection>,
}

impl CoreStore {
    pub fn open(data_dir: &Path) -> Result<Self, CoreError> {
        let database_dir = data_dir.join("data");
        fs::create_dir_all(&database_dir)?;

        let connection = Connection::open(database_dir.join(CORE_DB_FILE))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.busy_timeout(Duration::from_secs(5))?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    pub fn migrate(&self) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(CoreError::migration)?;
        let version = Self::schema_version_in_transaction(&transaction)?;

        match version {
            0 => {
                transaction.execute_batch(SCHEMA_V1).map_err(CoreError::migration)?;
                transaction
                    .execute(
                        "INSERT INTO schema_meta (key, value) VALUES ('schema_version', ?1)",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            CURRENT_SCHEMA_VERSION => {}
            version => return Err(CoreError::UnsupportedSchemaVersion { version }),
        }

        transaction.commit().map_err(CoreError::migration)
    }

    pub fn schema_version(&self) -> Result<u32, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let value = connection
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match value {
            Some(value) => value
                .parse()
                .map_err(|_| CoreError::InvalidSchemaVersion { value }),
            None => Ok(0),
        }
    }

    pub fn foreign_keys_enabled(&self) -> Result<bool, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let enabled = connection.query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?;
        Ok(enabled == 1)
    }

    pub fn table_count(&self, table_name: &str) -> Result<u32, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let count = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table_name],
            |row| row.get::<_, u32>(0),
        )?;
        Ok(count)
    }

    pub fn create_user(&self, input: NewUser, actor: &str) -> Result<User, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().timestamp_millis();

        transaction.execute(
            "INSERT INTO users (id, name, role, status, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, 'active', ?4, ?4)",
            params![input.id, input.name, input.role.as_str(), now],
        )?;
        Self::insert_audit_event(
            &transaction,
            actor,
            "user.create",
            "user",
            &input.id,
            serde_json::json!({"id": input.id, "role": input.role.as_str(), "result": "created"}),
            now,
        )?;
        transaction.commit()?;

        Ok(User {
            id: input.id,
            name: input.name,
            role: input.role,
        })
    }

    pub fn issue_api_key(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        actor: &str,
    ) -> Result<IssuedApiKey, CoreError> {
        let plaintext = Self::new_api_key();
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let key_id = Self::new_id("key");
        let scopes_json = serde_json::to_string(&scopes)?;
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO api_keys \
             (id, user_id, name, prefix, key_digest, scopes_json, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![key_id, user_id, name, prefix, key_digest, scopes_json, now],
        )?;
        Self::insert_audit_event(
            &transaction,
            actor,
            "api_key.issue",
            "api_key",
            &key_id,
            serde_json::json!({"id": key_id, "scopes": scopes, "result": "issued"}),
            now,
        )?;
        transaction.commit()?;

        Ok(IssuedApiKey {
            id: key_id,
            plaintext,
            prefix,
            user_id: user_id.to_owned(),
            scopes,
        })
    }

    pub fn revoke_api_key(&self, key_id: &str, actor: &str) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = Utc::now().timestamp_millis();
        let changed = transaction.execute(
            "UPDATE api_keys SET status = 'revoked', revoked_at_ms = ?1 \
             WHERE id = ?2 AND status = 'active'",
            params![now, key_id],
        )?;
        let result = if changed == 1 { "revoked" } else { "already_revoked_or_missing" };
        Self::insert_audit_event(
            &transaction,
            actor,
            "api_key.revoke",
            "api_key",
            key_id,
            serde_json::json!({"id": key_id, "result": result}),
            now,
        )?;
        transaction.commit()
            .map_err(CoreError::from)
    }

    pub fn authenticate_api_key(&self, presented: &str) -> Result<Principal, AuthError> {
        let digest = Self::digest_api_key(presented);
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id, user_id, key_digest, scopes_json FROM api_keys WHERE status = 'active'",
        ).map_err(CoreError::from)?;
        let candidates = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, String>(3)?,
            ))
        }).map_err(CoreError::from)?;

        for candidate in candidates {
            let (key_id, user_id, stored_digest, scopes_json) = candidate.map_err(CoreError::from)?;
            if stored_digest.ct_eq(&digest).into() {
                let scopes = serde_json::from_str(&scopes_json).map_err(CoreError::from)?;
                return Ok(Principal {
                    user_id,
                    key_id,
                    scopes,
                });
            }
        }
        Err(AuthError::InvalidApiKey)
    }

    #[doc(hidden)]
    pub fn raw_text_search(&self, table_name: &str, needle: &str) -> Result<Vec<String>, CoreError> {
        let query = match table_name {
            "api_keys" => {
                "SELECT id FROM api_keys WHERE \
                 instr(id, ?1) > 0 OR instr(user_id, ?1) > 0 OR instr(name, ?1) > 0 OR \
                 instr(prefix, ?1) > 0 OR instr(CAST(key_digest AS TEXT), ?1) > 0 OR \
                 instr(scopes_json, ?1) > 0 OR instr(status, ?1) > 0"
            }
            "audit_events" => {
                "SELECT id FROM audit_events WHERE \
                 instr(id, ?1) > 0 OR instr(actor_user_id, ?1) > 0 OR instr(action, ?1) > 0 OR \
                 instr(target_type, ?1) > 0 OR instr(target_id, ?1) > 0 OR \
                 instr(request_id, ?1) > 0 OR instr(metadata_json, ?1) > 0"
            }
            _ => {
                return Err(CoreError::UnsupportedDiagnosticTable {
                    table: table_name.to_owned(),
                })
            }
        };
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(query)?;
        let matches = statement
            .query_map([needle], |row| row.get(0))?
            .collect::<Result<Vec<String>, _>>()
            .map_err(CoreError::from)?;
        Ok(matches)
    }

    fn new_api_key() -> String {
        let mut material = [0_u8; 32];
        OsRng.fill_bytes(&mut material);
        format!("aw_live_{}", URL_SAFE_NO_PAD.encode(material))
    }

    fn new_id(kind: &str) -> String {
        let mut material = [0_u8; 16];
        OsRng.fill_bytes(&mut material);
        format!("{kind}_{}", URL_SAFE_NO_PAD.encode(material))
    }

    fn digest_api_key(plaintext: &str) -> Vec<u8> {
        Sha256::digest(plaintext.as_bytes()).to_vec()
    }

    fn insert_audit_event(
        transaction: &rusqlite::Transaction<'_>,
        actor: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
        metadata: serde_json::Value,
        now: i64,
    ) -> Result<(), CoreError> {
        transaction.execute(
            "INSERT INTO audit_events \
             (id, actor_user_id, action, target_type, target_id, metadata_json, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                Self::new_id("audit"),
                actor,
                action,
                target_type,
                target_id,
                serde_json::to_string(&metadata)?,
                now,
            ],
        )?;
        Ok(())
    }

    fn schema_version_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
    ) -> Result<u32, CoreError> {
        let schema_meta_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !schema_meta_exists {
            return Ok(0);
        }

        let value = transaction
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'schema_version'",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()?;

        match value {
            Some(value) => value
                .parse()
                .map_err(|_| CoreError::InvalidSchemaVersion { value }),
            None => Ok(0),
        }
    }
}

impl CoreError {
    fn migration(source: rusqlite::Error) -> Self {
        Self::Migration { source }
    }
}
