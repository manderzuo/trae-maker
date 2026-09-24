use std::{collections::BTreeSet, fs, path::Path, sync::Mutex, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use serde_json::{Map, Value};
use subtle::ConstantTimeEq;

use crate::{
    schema::{
        SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V6_FINISH,
        SCHEMA_V7, SCHEMA_V8, SCHEMA_V9, SCHEMA_V10, SCHEMA_V11, SCHEMA_V12, SCHEMA_V13, SCHEMA_V14,
        SCHEMA_V16, SCHEMA_V17, SCHEMA_V19, SCHEMA_V20,
    },
    upstream::{
        account_health_decision, audit_hash, audit_identifier, audit_label,
        normalize_health_category, validate_observation_summary, validate_opaque_credentials_ref,
        validate_required,
    },
    ApiKeySecretRecord, AuthError, CoreApiKeyAdminView, CoreError, CoreQuotaBalanceView, CoreUserAdminView, IssuedApiKey, LegacyMigrationBatch, LegacyMigrationResult, LeaseState,
    AdminCredentialRecord, AssetState, CoreAsset, CreateAssetInput, NewAdminCredential, NewUser, ObservationStatus, Principal, RegisterUpstreamAccount, UpstreamAccount,
    UpstreamAccountState, QuotaMigrationState, VideoBillingControl, VideoBillingControlInput,
    VideoBillingMode, VideoDiagnosticClaim,
    UpstreamLease, UpstreamObservation, User,
};

pub const CORE_DB_FILE: &str = "core.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 20;
pub const DEFAULT_API_KEY_MAX_CONCURRENCY: i64 = 32;

pub struct CoreStore {
    pub(crate) connection: Mutex<Connection>,
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
        let version_before_migration = {
            let schema_meta_exists: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta')",
                [],
                |row| row.get(0),
            )?;
            if !schema_meta_exists {
                0
            } else {
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
                        .map_err(|_| CoreError::InvalidSchemaVersion { value })?,
                    None => 0,
                }
            }
        };
        // v7 rebuilds the requests parent table while upstream_leases already
        // references it. SQLite cannot drop that parent with foreign keys on;
        // disable enforcement only for the migration transaction and restore
        // it immediately after commit/rollback.
        let relax_foreign_keys = version_before_migration <= 6;
        if relax_foreign_keys {
            connection.pragma_update(None, "foreign_keys", "OFF")?;
        }

        let migration_result = (|| {
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
                        params!["1"],
                    )
                    .map_err(CoreError::migration)?;
                Self::migrate_v1_to_v2(&transaction)?;
                Self::migrate_v2_to_v3(&transaction)?;
                Self::migrate_v3_to_v4(&transaction)?;
                Self::migrate_v4_to_v5(&transaction)?;
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            1 => {
                Self::migrate_v1_to_v2(&transaction)?;
                Self::migrate_v2_to_v3(&transaction)?;
                Self::migrate_v3_to_v4(&transaction)?;
                Self::migrate_v4_to_v5(&transaction)?;
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            2 => {
                Self::migrate_v2_to_v3(&transaction)?;
                Self::migrate_v3_to_v4(&transaction)?;
                Self::migrate_v4_to_v5(&transaction)?;
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            3 => {
                Self::migrate_v3_to_v4(&transaction)?;
                Self::migrate_v4_to_v5(&transaction)?;
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            4 => {
                Self::migrate_v4_to_v5(&transaction)?;
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            5 => {
                Self::migrate_v5_to_v6(&transaction)?;
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            6 => {
                Self::migrate_v6_to_v7(&transaction)?;
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            7 => {
                Self::migrate_v7_to_v8(&transaction)?;
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            8 => {
                Self::migrate_v8_to_v9(&transaction)?;
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            9 => {
                Self::migrate_v9_to_v10(&transaction)?;
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            10 => {
                Self::migrate_v10_to_v11(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            11 => {}
            12 => {}
            13 => {}
            14 | 15 | 16 | 17 | 18 | 19 | CURRENT_SCHEMA_VERSION => Self::harden_v6_records(&transaction)?,
                version => return Err(CoreError::UnsupportedSchemaVersion { version }),
            }

            if version < 12 {
                Self::migrate_v11_to_v12(&transaction)?;
            }
            if version < 13 {
                Self::migrate_v12_to_v13(&transaction)?;
            }
            if version < 14 {
                Self::migrate_v13_to_v14(&transaction)?;
            }
            if version < 15 {
                Self::migrate_v14_to_v15(&transaction)?;
            }
            if version < 16 {
                Self::migrate_v15_to_v16(&transaction)?;
            }
            if version < 17 {
                Self::migrate_v16_to_v17(&transaction)?;
            }
            if version < 18 {
                Self::migrate_v17_to_v18(&transaction)?;
            }
            if version < 19 {
                Self::migrate_v18_to_v19(&transaction)?;
            }
            if version < 20 {
                Self::migrate_v19_to_v20(&transaction)?;
            }
            if version < CURRENT_SCHEMA_VERSION {
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }

            transaction.commit().map_err(CoreError::migration)
        })();

        if relax_foreign_keys {
            let restore_result = connection
                .pragma_update(None, "foreign_keys", "ON")
                .map_err(CoreError::from);
            if let Err(error) = migration_result {
                let _ = restore_result;
                return Err(error);
            }
            restore_result?;
        }
        migration_result
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

    pub fn video_billing_control(&self) -> Result<VideoBillingControl, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection
            .query_row(
                "SELECT mode, reason, diagnostic_key_id, diagnostic_request_hash,
                        diagnostic_claimed_at_ms, updated_at_ms
                 FROM video_billing_control WHERE id = 1",
                [],
                |row| {
                    let mode_value: String = row.get(0)?;
                    let mode = VideoBillingMode::from_db(&mode_value).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            0,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("invalid video billing mode: {mode_value}"),
                            )),
                        )
                    })?;
                    Ok(VideoBillingControl {
                        mode,
                        reason: row.get(1)?,
                        diagnostic_key_id: row.get(2)?,
                        diagnostic_request_hash: row.get(3)?,
                        diagnostic_claimed_at_ms: row.get(4)?,
                        updated_at_ms: row.get(5)?,
                    })
                },
            )
            .map_err(CoreError::from)
    }

    pub fn set_video_billing_control(
        &self,
        input: VideoBillingControlInput,
    ) -> Result<(), CoreError> {
        validate_video_billing_control_input(&input)?;
        let now = Utc::now().timestamp_millis();
        let diagnostic_key_id = if input.mode == VideoBillingMode::DiagnosticOnce {
            input.diagnostic_key_id.as_deref()
        } else {
            None
        };
        let diagnostic_request_hash = if input.mode == VideoBillingMode::DiagnosticOnce {
            input.diagnostic_request_hash.as_deref()
        } else {
            None
        };
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection.execute(
            "UPDATE video_billing_control
             SET mode = ?1, reason = ?2, diagnostic_key_id = ?3,
                 diagnostic_request_hash = ?4, diagnostic_claimed_at_ms = NULL,
                 updated_at_ms = ?5
             WHERE id = 1",
            params![
                input.mode.as_str(),
                input.reason.trim(),
                diagnostic_key_id,
                diagnostic_request_hash,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn claim_video_diagnostic(
        &self,
        key_id: &str,
        request_hash: &str,
    ) -> Result<Option<VideoDiagnosticClaim>, CoreError> {
        validate_video_diagnostic_values(key_id, request_hash)?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let control = transaction
            .query_row(
                "SELECT mode, diagnostic_key_id, diagnostic_request_hash,
                        diagnostic_claimed_at_ms
                 FROM video_billing_control WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((mode, configured_key, configured_hash, claimed_at_ms)) = control else {
            return Err(CoreError::InvalidConfiguration {
                key: "video_billing_control".into(),
                value: "control row is missing".into(),
            });
        };
        if mode != VideoBillingMode::DiagnosticOnce.as_str()
            || claimed_at_ms.is_some()
            || configured_key.as_deref() != Some(key_id)
            || configured_hash.as_deref() != Some(request_hash)
        {
            transaction.commit()?;
            return Ok(None);
        }

        let claim_id = Self::new_id("video-diagnostic");
        transaction.execute(
            "INSERT INTO video_diagnostic_claims (claim_id, key_id, request_hash, claimed_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![claim_id, key_id, request_hash, now],
        )?;
        transaction.execute(
            "UPDATE video_billing_control
             SET mode = 'paused', diagnostic_claimed_at_ms = ?1, updated_at_ms = ?1
             WHERE id = 1",
            [now],
        )?;
        transaction.commit()?;
        Ok(Some(VideoDiagnosticClaim { claimed_at_ms: now }))
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

    pub fn table_exists(&self, table_name: &str) -> Result<bool, CoreError> {
        Ok(self.table_count(table_name)? == 1)
    }

    pub fn find_admin_credential(&self, username: &str) -> Result<Option<AdminCredentialRecord>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection
            .query_row(
                "SELECT user_id, username, password_hash, salt, iterations, must_change_password
                 FROM admin_credentials WHERE username = ?1",
                [username],
                |row| {
                    Ok(AdminCredentialRecord {
                        user_id: row.get(0)?,
                        username: row.get(1)?,
                        password_hash: row.get(2)?,
                        salt: row.get(3)?,
                        iterations: row.get(4)?,
                        must_change_password: row.get::<_, i64>(5)? != 0,
                    })
                },
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub fn upsert_admin_credential(&self, input: NewAdminCredential) -> Result<(), CoreError> {
        if input.user_id.trim().is_empty() || input.username.trim().is_empty() || input.password_hash.trim().is_empty() || input.salt.trim().is_empty() {
            return Err(CoreError::Validation { field: "admin_credential".into(), reason: "user_id, username, password_hash and salt are required".into() });
        }
        if input.iterations < 100_000 {
            return Err(CoreError::Validation { field: "iterations".into(), reason: "must be at least 100000".into() });
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let is_active_admin: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1 AND status = 'active' AND role = 'admin')",
            [&input.user_id],
            |row| row.get(0),
        )?;
        if !is_active_admin {
            return Err(CoreError::AdminRequired);
        }
        let now_ms = Utc::now().timestamp_millis();
        transaction.execute(
            "INSERT INTO admin_credentials (user_id, username, password_hash, salt, iterations, must_change_password, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
             ON CONFLICT(user_id) DO UPDATE SET username = excluded.username, password_hash = excluded.password_hash,
               salt = excluded.salt, iterations = excluded.iterations, must_change_password = excluded.must_change_password,
               updated_at_ms = excluded.updated_at_ms",
            params![input.user_id, input.username, input.password_hash, input.salt, input.iterations, input.must_change_password as i64, now_ms],
        )?;
        transaction.commit().map_err(CoreError::from)
    }

    pub fn mark_admin_password_changed(&self, username: &str, password_hash: String, salt: String, iterations: u32) -> Result<(), CoreError> {
        if password_hash.trim().is_empty() || salt.trim().is_empty() || iterations < 100_000 {
            return Err(CoreError::Validation { field: "admin_credential".into(), reason: "password hash, salt and iterations are invalid".into() });
        }
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let updated = connection.execute(
            "UPDATE admin_credentials SET password_hash = ?1, salt = ?2, iterations = ?3, must_change_password = 0, updated_at_ms = ?4 WHERE username = ?5",
            params![password_hash, salt, iterations, Utc::now().timestamp_millis(), username],
        )?;
        if updated == 1 { Ok(()) } else { Err(CoreError::UserNotFound { user_id: username.to_owned() }) }
    }

    pub fn count_rows(&self, table_name: &str) -> Result<u64, CoreError> {
        if table_name.is_empty()
            || !table_name
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(CoreError::Validation {
                field: "table_name".into(),
                reason: "must be an SQLite identifier".into(),
            });
        }
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let count = connection.query_row(
            &format!("SELECT COUNT(*) FROM {table_name}"),
            [],
            |row| row.get::<_, u64>(0),
        )?;
        Ok(count)
    }

    pub fn create_asset(
        &self,
        principal: &Principal,
        input: CreateAssetInput,
    ) -> Result<CoreAsset, CoreError> {
        validate_asset_input(&input)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        transaction.execute(
            "INSERT INTO assets
             (id, user_id, filename, mime_type, extension, size, sha256, storage_ref,
              content_token_digest, created_at_ms, expires_at_ms, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 'active')",
            params![
                &input.id,
                &principal.user_id,
                &input.filename,
                &input.mime_type,
                &input.extension,
                input.size,
                &input.sha256,
                &input.storage_ref,
                &input.content_token_digest,
                input.created_at_ms,
                input.expires_at_ms,
            ],
        )?;
        let now = Utc::now().timestamp_millis();
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "asset.create",
            "asset",
            &audit_hash(&input.id),
            serde_json::json!({
                "asset": audit_hash(&input.id),
                "user": audit_hash(&principal.user_id),
                "mime_type": audit_label(&input.mime_type),
                "size": input.size,
                "result": "created",
            }),
            now,
        )?;
        transaction.commit()?;
        Ok(CoreAsset {
            id: input.id,
            user_id: principal.user_id.clone(),
            filename: input.filename,
            mime_type: input.mime_type,
            extension: input.extension,
            size: input.size,
            sha256: input.sha256,
            storage_ref: input.storage_ref,
            content_token_digest: input.content_token_digest,
            created_at_ms: input.created_at_ms,
            expires_at_ms: input.expires_at_ms,
            state: AssetState::Active,
        })
    }

    pub fn asset_for_user(
        &self,
        principal: &Principal,
        asset_id: &str,
    ) -> Result<Option<CoreAsset>, CoreError> {
        validate_asset_identifier(asset_id)?;
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::ensure_principal_on_connection(&connection, principal)?;
        connection
            .query_row(
                "SELECT id, user_id, filename, mime_type, extension, size, sha256,
                        storage_ref, content_token_digest, created_at_ms, expires_at_ms, state
                 FROM assets WHERE id = ?1 AND user_id = ?2",
                params![asset_id.trim(), &principal.user_id],
                Self::asset_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub fn asset_by_content_token(
        &self,
        asset_id: &str,
        token_digest: &[u8],
        now_ms: i64,
    ) -> Result<Option<CoreAsset>, CoreError> {
        validate_asset_identifier(asset_id)?;
        if token_digest.len() != 32 {
            return Err(CoreError::Validation {
                field: "content_token_digest".into(),
                reason: "must be exactly 32 bytes".into(),
            });
        }
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let asset = connection
            .query_row(
                "SELECT id, user_id, filename, mime_type, extension, size, sha256,
                        storage_ref, content_token_digest, created_at_ms, expires_at_ms, state
                 FROM assets
                 WHERE id = ?1 AND state = 'active' AND expires_at_ms > ?2",
                params![asset_id.trim(), now_ms],
                Self::asset_from_row,
            )
            .optional()?;
        Ok(asset.filter(|asset| asset.content_token_digest.ct_eq(token_digest).into()))
    }

    pub fn expire_assets(&self, now_ms: i64) -> Result<Vec<String>, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let assets = {
            let mut statement = transaction.prepare(
                "SELECT id, storage_ref FROM assets WHERE state = 'active' AND expires_at_ms <= ?1",
            )?;
            let rows = statement
                .query_map([now_ms], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        for (id, _) in &assets {
            transaction.execute(
                "UPDATE assets SET state = 'expired' WHERE id = ?1 AND state = 'active'",
                [id],
            )?;
            Self::insert_audit_event(
                &transaction,
                "system",
                "asset.expire",
                "asset",
                &audit_hash(id),
                serde_json::json!({"asset": audit_hash(id), "result": "expired"}),
                now_ms,
            )?;
        }
        transaction.commit()?;
        Ok(assets.into_iter().map(|(_, storage_ref)| storage_ref).collect())
    }

    pub fn upsert_upstream_account(
        &self,
        input: RegisterUpstreamAccount,
        principal: &Principal,
    ) -> Result<UpstreamAccount, CoreError> {
        Self::validate_upstream_account(&input)?;
        let capabilities_json = serde_json::to_string(&input.capabilities)?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let created_at_ms = transaction
            .query_row(
                "SELECT created_at_ms FROM upstream_accounts WHERE id = ?1",
                [&input.id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(now);
        transaction.execute(
            "INSERT INTO upstream_accounts
             (id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency,
              state, cooldown_until_ms, cooldown_reason, consecutive_errors, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(id) DO UPDATE SET
               provider = excluded.provider, credentials_ref = excluded.credentials_ref,
               region = excluded.region, capabilities_json = excluded.capabilities_json,
               enabled = excluded.enabled, max_concurrency = excluded.max_concurrency,
               state = excluded.state, cooldown_until_ms = excluded.cooldown_until_ms,
               cooldown_reason = excluded.cooldown_reason,
               consecutive_errors = excluded.consecutive_errors, updated_at_ms = excluded.updated_at_ms",
            params![
                &input.id, &input.provider, &input.credentials_ref, &input.region, capabilities_json,
                if input.enabled { 1_i64 } else { 0_i64 }, input.max_concurrency,
                input.state.as_str(), input.cooldown_until_ms, &input.cooldown_reason,
                input.consecutive_errors, created_at_ms, now,
            ],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "upstream.account_upsert",
            "upstream_account",
            &audit_hash(&input.id),
            serde_json::json!({
                "account_hash": audit_hash(&input.id),
                "provider": audit_label(&input.provider),
                "result": "upserted"
            }),
            now,
        )?;
        transaction.commit()?;
        Ok(UpstreamAccount {
            id: input.id, provider: input.provider, credentials_ref: input.credentials_ref,
            region: input.region, capabilities: input.capabilities, enabled: input.enabled,
            max_concurrency: input.max_concurrency, state: input.state,
            cooldown_until_ms: input.cooldown_until_ms, cooldown_reason: input.cooldown_reason,
            consecutive_errors: input.consecutive_errors, created_at_ms, updated_at_ms: now,
        })
    }

    pub fn append_upstream_observation(
        &self,
        mut observation: UpstreamObservation,
    ) -> Result<UpstreamObservation, CoreError> {
        Self::validate_upstream_observation(&observation)?;
        let summary_json = validate_observation_summary(&observation.summary)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (provider, credentials_ref) = transaction
            .query_row(
                "SELECT provider, credentials_ref FROM upstream_accounts WHERE id = ?1",
                [&observation.account_ref],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::Validation {
                field: "account_ref".into(),
                reason: "must identify a registered upstream account".into(),
            })?;
        if summary_json.contains(&credentials_ref) {
            return Err(CoreError::Validation {
                field: "summary".into(),
                reason: "must not contain credentials_ref".into(),
            });
        }
        if observation.status == ObservationStatus::Failed {
            let previous = transaction
                .query_row(
                    "SELECT observed_value, value_scale FROM upstream_observations
                     WHERE account_ref = ?1 AND resource_kind = ?2
                       AND status = 'fresh' AND observed_value IS NOT NULL
                       AND observed_at_ms <= ?3
                     ORDER BY observed_at_ms DESC, id DESC
                     LIMIT 1",
                    params![
                        &observation.account_ref,
                        &observation.resource_kind,
                        observation.observed_at_ms,
                    ],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            // The integer and its scale are one measurement. A later null-valued
            // sample may use another scale and must not reinterpret this value.
            match previous {
                Some((value, scale)) => {
                    observation.observed_value = Some(value);
                    observation.value_scale = scale;
                }
                None => observation.observed_value = None,
            }
        }
        transaction.execute(
            "INSERT INTO upstream_observations
             (id, account_ref, resource_kind, observed_value, value_scale, source, status,
              observed_at_ms, stale_at_ms, summary_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &observation.id, &observation.account_ref, &observation.resource_kind,
                observation.observed_value, observation.value_scale, &observation.source,
                observation.status.as_str(), observation.observed_at_ms, observation.stale_at_ms,
                summary_json,
            ],
        )?;
        let action = if observation.status == ObservationStatus::Failed {
            "upstream.observation_failed"
        } else {
            "upstream.observation_recorded"
        };
        Self::insert_audit_event(
            &transaction,
            "system",
            action,
            "upstream_observation",
            &audit_hash(&observation.id),
            serde_json::json!({
                "account_hash": audit_hash(&observation.account_ref),
                "provider": audit_label(&provider),
                "resource_kind": audit_label(&observation.resource_kind),
                "observation": audit_hash(&observation.id),
                "status": observation.status.as_str(),
                "source": audit_label(&observation.source)
            }),
            observation.observed_at_ms,
        )?;
        transaction.commit()?;
        Ok(observation)
    }

    pub fn get_latest_observation(
        &self,
        account_ref: &str,
        resource_kind: &str,
    ) -> Result<Option<UpstreamObservation>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection.query_row(
            "SELECT id, account_ref, resource_kind, observed_value, value_scale, source, status,
                    observed_at_ms, stale_at_ms, summary_json
             FROM upstream_observations
             WHERE account_ref = ?1 AND resource_kind = ?2
             ORDER BY observed_at_ms DESC, id DESC LIMIT 1",
            params![account_ref, resource_kind],
            Self::upstream_observation_from_row,
        ).optional().map_err(CoreError::from)
    }

    pub fn list_recoverable_leases(&self) -> Result<Vec<UpstreamLease>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id,
                    state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind,
                    created_at_ms, updated_at_ms, settled_at_ms
             FROM upstream_leases WHERE state IN ('held', 'active', 'unknown')
             ORDER BY created_at_ms, id",
        )?;
        let leases = statement
            .query_map([], Self::upstream_lease_from_row)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(CoreError::from)?;
        Ok(leases)
    }

    /// Persist one adapter/reader health observation without storing the
    /// upstream response or any credential. The account row is the durable
    /// source of truth used by the next scheduler selection.
    #[allow(clippy::too_many_arguments)]
    pub fn record_upstream_health_transition(
        &self,
        account_ref: &str,
        request_id: Option<&str>,
        lease_id: Option<&str>,
        resource_kind: &str,
        observation_id: Option<&str>,
        category: &str,
        now_ms: i64,
    ) -> Result<UpstreamAccount, CoreError> {
        validate_required("account_ref", account_ref)?;
        validate_required("resource_kind", resource_kind)?;
        let category = normalize_health_category(category);
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = transaction
            .query_row(
                "SELECT provider, enabled, state, cooldown_until_ms, cooldown_reason,
                        consecutive_errors, created_at_ms, updated_at_ms, region,
                        capabilities_json, max_concurrency
                 FROM upstream_accounts WHERE id = ?1",
                [account_ref],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? != 0,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, Option<String>>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, i64>(10)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::Validation {
                field: "account_ref".into(),
                reason: "must identify a registered upstream account".into(),
            })?;
        let previous_state = Self::upstream_account_state_from_db(&existing.2).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "upstream_accounts.state".into(),
                value: existing.2.clone(),
            }
        })?;
        let decision = account_health_decision(category, existing.5, now_ms);
        let preserve_health = category == "reader_failure"
            || (category == "success"
                && matches!(previous_state, UpstreamAccountState::Disabled | UpstreamAccountState::Forbidden));
        let (enabled, state, cooldown_until_ms, cooldown_reason, consecutive_errors) = if preserve_health {
            (
                existing.1,
                previous_state,
                existing.3,
                existing.4.clone(),
                existing.5,
            )
        } else {
            (
                decision.enabled,
                decision.state,
                decision.cooldown_until_ms,
                decision.cooldown_reason.map(str::to_owned),
                decision.consecutive_errors,
            )
        };
        let updated_at_ms = now_ms;
        transaction.execute(
            "UPDATE upstream_accounts
             SET enabled = ?1, state = ?2, cooldown_until_ms = ?3,
                 cooldown_reason = ?4, consecutive_errors = ?5, updated_at_ms = ?6
             WHERE id = ?7",
            params![
                if enabled { 1_i64 } else { 0_i64 },
                state.as_str(),
                cooldown_until_ms,
                cooldown_reason,
                consecutive_errors,
                updated_at_ms,
                account_ref,
            ],
        )?;
        let observation = observation_id.map(audit_hash);
        Self::insert_audit_event(
            &transaction,
            "system",
            "upstream.health_transition",
            "upstream_account",
            &audit_hash(account_ref),
            serde_json::json!({
                "request_id": audit_identifier(request_id),
                "lease_id": audit_identifier(lease_id),
                "account_hash": audit_hash(account_ref),
                "provider": audit_label(&existing.0),
                "resource_kind": audit_label(resource_kind),
                "observation": observation,
                "error_category": decision.category,
            }),
            now_ms,
        )?;
        let account = Self::upstream_account_in_transaction(&transaction, account_ref)?;
        transaction.commit()?;
        Ok(account)
    }

    /// Return only aggregate scheduler diagnostics to an authorized
    /// administrator. Account identifiers, credentials and user-owned quota
    /// rows are intentionally absent from this projection.
    pub fn scheduler_status_for_admin(
        &self,
        principal: &Principal,
        now_ms: i64,
    ) -> Result<serde_json::Value, CoreError> {
        self.authorize_admin_principal(principal)?;
        self.scheduler_status_counts(now_ms)
    }

    /// Internal/runtime status projection. Callers that expose it externally
    /// must apply their own administrator authorization first.
    pub fn scheduler_status_counts(&self, now_ms: i64) -> Result<serde_json::Value, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let count = |sql: &str| -> Result<u64, CoreError> {
            Ok(connection.query_row(sql, [], |row| row.get::<_, u64>(0))?)
        };
        let count_now = |sql: &str| -> Result<u64, CoreError> {
            Ok(connection.query_row(sql, [now_ms], |row| row.get::<_, u64>(0))?)
        };
        let accounts = count("SELECT COUNT(*) FROM upstream_accounts")?;
        let enabled_accounts = count("SELECT COUNT(*) FROM upstream_accounts WHERE enabled = 1")?;
        let fresh_observations = count_now(
            "WITH latest AS (
                 SELECT observation.account_ref, observation.resource_kind,
                        observation.status, observation.observed_at_ms, observation.stale_at_ms
                 FROM upstream_observations AS observation
                 WHERE NOT EXISTS (
                     SELECT 1 FROM upstream_observations AS newer
                     WHERE newer.account_ref = observation.account_ref
                       AND newer.resource_kind = observation.resource_kind
                       AND (newer.observed_at_ms > observation.observed_at_ms
                            OR (newer.observed_at_ms = observation.observed_at_ms
                                AND newer.id > observation.id))
                 )
             )
             SELECT COUNT(*) FROM latest
             WHERE status = 'fresh' AND observed_at_ms <= ?1 AND stale_at_ms > ?1",
        )?;
        let stale_observations = count_now(
            "WITH latest AS (
                 SELECT observation.account_ref, observation.resource_kind,
                        observation.status, observation.observed_at_ms, observation.stale_at_ms
                 FROM upstream_observations AS observation
                 WHERE NOT EXISTS (
                     SELECT 1 FROM upstream_observations AS newer
                     WHERE newer.account_ref = observation.account_ref
                       AND newer.resource_kind = observation.resource_kind
                       AND (newer.observed_at_ms > observation.observed_at_ms
                            OR (newer.observed_at_ms = observation.observed_at_ms
                                AND newer.id > observation.id))
                 )
             )
             SELECT COUNT(*) FROM latest
             WHERE status <> 'fresh' OR stale_at_ms <= ?1",
        )?;
        let reader_failures = count(
            "SELECT COUNT(*) FROM upstream_observations WHERE status = 'failed'",
        )?;
        let active_leases = count(
            "SELECT COUNT(*) FROM upstream_leases WHERE state IN ('held', 'active')",
        )?;
        let unknown_leases = count(
            "SELECT COUNT(*) FROM upstream_leases WHERE state = 'unknown'",
        )?;
        let slot_saturated = count(
            "SELECT COUNT(*) FROM upstream_accounts AS account
             WHERE account.enabled = 1 AND account.state = 'available'
               AND (SELECT COUNT(*) FROM upstream_leases AS lease
                    WHERE lease.account_ref = account.id
                       AND lease.state IN ('held', 'active', 'unknown')) >= account.max_concurrency",
        )?;
        Ok(serde_json::json!({
            "schema_version": CURRENT_SCHEMA_VERSION,
            "accounts": accounts,
            "enabled_accounts": enabled_accounts,
            "fresh_observations": fresh_observations,
            "stale_observations": stale_observations,
            "active_leases": active_leases,
            "unknown_leases": unknown_leases,
            "slot_saturated": slot_saturated,
            "reader_failures": reader_failures,
        }))
    }

    pub fn create_user(&self, input: NewUser, actor: &str) -> Result<User, CoreError> {
        self.create_user_inner(input, actor, false)
    }

    pub fn create_user_as_admin(
        &self,
        input: NewUser,
        principal: &Principal,
    ) -> Result<User, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let user = Self::insert_user_in_transaction(&transaction, input, &principal.user_id)?;
        transaction.commit()?;
        Ok(user)
    }

    pub fn list_users_as_admin(
        &self,
        principal: &Principal,
    ) -> Result<Vec<CoreUserAdminView>, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let users = {
            let mut statement = transaction.prepare(
                "SELECT id, name, role, status, created_at_ms, updated_at_ms
                 FROM users ORDER BY created_at_ms, id",
            )?;
            let rows = statement
                .query_map([], |row| {
                    Ok(CoreUserAdminView {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        role: row.get(2)?,
                        status: row.get(3)?,
                        created_at_ms: row.get(4)?,
                        updated_at_ms: row.get(5)?,
                    })
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        transaction.commit()?;
        Ok(users)
    }

    pub fn list_api_keys_as_admin(
        &self,
        principal: &Principal,
        user_id: Option<&str>,
    ) -> Result<Vec<CoreApiKeyAdminView>, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let key_records = {
            let mut statement = if user_id.is_some() {
                transaction.prepare(
                    "SELECT api_keys.id, api_keys.user_id, users.name, api_keys.name, api_keys.prefix,
                            api_keys.scopes_json, api_keys.status, api_keys.max_concurrency,
                            api_keys.created_at_ms, api_keys.revoked_at_ms,
                            EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = api_keys.id)
                     FROM api_keys INNER JOIN users ON users.id = api_keys.user_id
                     WHERE api_keys.user_id = ?1 ORDER BY api_keys.created_at_ms, api_keys.id",
                )?
            } else {
                transaction.prepare(
                    "SELECT api_keys.id, api_keys.user_id, users.name, api_keys.name, api_keys.prefix,
                            api_keys.scopes_json, api_keys.status, api_keys.max_concurrency,
                            api_keys.created_at_ms, api_keys.revoked_at_ms,
                            EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = api_keys.id)
                     FROM api_keys INNER JOIN users ON users.id = api_keys.user_id
                     WHERE users.role <> 'admin'
                     ORDER BY api_keys.created_at_ms, api_keys.id",
                )?
            };
            let rows = if let Some(user_id) = user_id {
                statement.query([user_id])?
            } else {
                statement.query([])?
            };
            rows.mapped(|row| {
                let scopes_json: String = row.get(5)?;
                let scopes = serde_json::from_str(&scopes_json).map_err(|error| {
                    rusqlite::Error::FromSqlConversionFailure(
                        5,
                        rusqlite::types::Type::Text,
                        Box::new(error),
                    )
                })?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    scopes,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<i64>>(9)?,
                    row.get::<_, bool>(10)?,
                ))
            })
            .collect::<Result<Vec<_>, _>>()?
        };
        let keys = key_records
            .into_iter()
            .map(|(id, user_id, user_name, name, prefix, scopes, status, max_concurrency, created_at_ms, revoked_at_ms, billing_blocked)| {
                let current_concurrency = transaction.query_row(
                    "SELECT COUNT(*) FROM requests
                     WHERE api_key_id = ?1
                       AND state IN ('received','validating','reserved','queued','dispatched','completing','unknown')",
                    [&id],
                    |row| row.get::<_, i64>(0),
                )?;
                let resource_kinds = {
                    let mut statement = transaction.prepare(
                        "SELECT resource_kind FROM quota_ledger WHERE user_id = ?1
                         UNION SELECT resource_kind FROM quota_reservations WHERE user_id = ?1
                         ORDER BY resource_kind",
                    )?;
                    let rows = statement.query_map([&user_id], |row| row.get::<_, String>(0))?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                let usage = resource_kinds
                    .into_iter()
                    .map(|resource_kind| {
                        let (available, held, settled) = transaction.query_row(
                            "SELECT
                               COALESCE((SELECT SUM(delta) FROM quota_ledger
                                         WHERE user_id = ?1 AND resource_kind = ?2
                                           AND (budget_account_id IS NULL OR budget_account_id IN
                                                (SELECT id FROM quota_budget_accounts WHERE scope = 'user_cap'))), 0),
                               COALESCE((SELECT SUM(amount) FROM quota_reservations
                                         WHERE user_id = ?1 AND resource_kind = ?2
                                           AND state IN ('held', 'unknown')
                                           AND (key_budget_account_id IS NULL OR user_cap_account_id IS NOT NULL)), 0),
                               COALESCE((SELECT SUM(amount) FROM quota_ledger
                                         WHERE user_id = ?1 AND resource_kind = ?2 AND event_kind = 'commit'
                                           AND (budget_account_id IS NULL OR budget_account_id IN
                                                (SELECT id FROM quota_budget_accounts WHERE scope = 'user_cap'))), 0)",
                            params![&user_id, &resource_kind],
                            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?),),
                        )?;
                        Ok(CoreQuotaBalanceView { resource_kind, available, held, settled })
                    })
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                let key_resource_kinds = {
                    let mut statement = transaction.prepare(
                        "SELECT resource_kind FROM quota_budget_accounts
                         WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2
                         ORDER BY resource_kind",
                    )?;
                    let rows = statement.query_map(params![&id, &user_id], |row| row.get::<_, String>(0))?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                let key_quota = key_resource_kinds
                    .iter()
                    .map(|resource_kind| {
                        let account_id = transaction.query_row(
                            "SELECT id FROM quota_budget_accounts
                             WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = ?3
                             ORDER BY id LIMIT 1",
                            params![&id, &user_id, resource_kind],
                            |row| row.get::<_, String>(0),
                        )?;
                        Self::budget_balance_in_transaction(&transaction, &account_id)
                            .map(|balance| CoreQuotaBalanceView {
                                resource_kind: balance.resource_kind,
                                available: balance.available,
                                held: balance.held,
                                settled: balance.settled,
                            })
                            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))
                    })
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                let verified_credit_spent = transaction.query_row(
                    "SELECT COALESCE(SUM(s.actual_credits), 0)
                     FROM billing_settlements s
                     INNER JOIN billing_receipts r ON r.receipt_id = s.receipt_id
                     INNER JOIN requests q ON q.id = s.request_id
                     WHERE q.api_key_id = ?1
                       AND r.status IN ('final','failed_no_charge')",
                    [&id],
                    |row| row.get::<_, i64>(0),
                )?;
                let pool_allocatable = usage
                    .iter()
                    .map(|pool| {
                        let allocated = Self::allocated_key_quota_in_transaction(
                            &transaction,
                            &user_id,
                            &pool.resource_kind,
                        )
                        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
                        Ok(CoreQuotaBalanceView {
                            resource_kind: pool.resource_kind.clone(),
                            available: pool.available.saturating_sub(allocated).max(0),
                            held: pool.held,
                            settled: pool.settled,
                        })
                    })
                    .collect::<Result<Vec<_>, rusqlite::Error>>()?;
                Ok(CoreApiKeyAdminView {
                    id,
                    user_id,
                    user_name,
                    name,
                    prefix,
                    scopes,
                    status,
                    billing_blocked,
                    max_concurrency,
                    current_concurrency,
                    usage,
                    key_quota,
                    verified_credit_spent,
                    pool_allocatable,
                    created_at_ms,
                    revoked_at_ms,
                })
            })
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        transaction.commit()?;
        Ok(keys)
    }

    pub fn issue_api_key_for_new_user_as_admin(
        &self,
        principal: &Principal,
        display_name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
    ) -> Result<IssuedApiKey, CoreError> {
        self.issue_api_key_for_new_user_as_admin_inner(
            principal,
            display_name,
            scopes,
            max_concurrency,
            Self::new_id("key"),
            Self::new_api_key(),
            None,
        )
    }

    pub fn issue_api_key_for_new_user_as_admin_with_encrypted_copy<F>(
        &self,
        principal: &Principal,
        display_name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
        encrypt_copy: F,
    ) -> Result<IssuedApiKey, CoreError>
    where
        F: FnOnce(&str, &str) -> Result<(Vec<u8>, u32), CoreError>,
    {
        let key_id = Self::new_id("key");
        let plaintext = Self::new_api_key();
        let encrypted_copy = encrypt_copy(&key_id, &plaintext)?;
        self.issue_api_key_for_new_user_as_admin_inner(
            principal,
            display_name,
            scopes,
            max_concurrency,
            key_id,
            plaintext,
            Some(encrypted_copy),
        )
    }

    fn issue_api_key_for_new_user_as_admin_inner(
        &self,
        principal: &Principal,
        display_name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
        key_id: String,
        plaintext: String,
        encrypted_copy: Option<(Vec<u8>, u32)>,
    ) -> Result<IssuedApiKey, CoreError> {
        let display_name = display_name.trim();
        if display_name.is_empty() {
            return Err(CoreError::Validation { field: "display_name".into(), reason: "must not be empty".into() });
        }
        Self::validate_max_concurrency(max_concurrency)?;
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let user_id = Self::new_id("user");
        let scopes = Self::normalize_api_key_scopes(scopes);
        let scopes_json = serde_json::to_string(&scopes)?;
        let (secret_ciphertext, secret_key_version) = match encrypted_copy {
            Some((ciphertext, version)) if !ciphertext.is_empty() && version > 0 => {
                (Some(ciphertext), Some(i64::from(version)))
            }
            Some(_) => return Err(CoreError::ApiKeyEncryptionUnavailable),
            None => (None, None),
        };
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::insert_user_in_transaction(
            &transaction,
            NewUser { id: user_id.clone(), name: display_name.to_owned(), role: crate::UserRole::User },
            &principal.user_id,
        )?;
        transaction.execute(
            "INSERT INTO api_keys (id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, status, created_at_ms, secret_ciphertext, secret_key_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, ?10)",
            params![&key_id, &user_id, display_name, &prefix, &key_digest, &scopes_json, max_concurrency, now, secret_ciphertext, secret_key_version],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.issue_with_user",
            "api_key",
            &key_id,
            serde_json::json!({ "id": key_id, "user_id": user_id, "scopes": scopes, "result": "issued" }),
            now,
        )?;
        transaction.commit()?;

        Ok(IssuedApiKey { id: key_id, plaintext, prefix, user_id, scopes })
    }

    pub fn update_api_key_as_admin(
        &self,
        principal: &Principal,
        key_id: &str,
        enabled: bool,
        max_concurrency: i64,
    ) -> Result<(), CoreError> {
        Self::validate_max_concurrency(max_concurrency)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let role: String = transaction
            .query_row(
                "SELECT users.role FROM api_keys INNER JOIN users ON users.id = api_keys.user_id WHERE api_keys.id = ?1",
                [key_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::ApiKeyNotFound { api_key_id: key_id.into() })?;
        if role == "admin" {
            return Err(CoreError::AdminRequired);
        }
        let status = if enabled { "active" } else { "revoked" };
        let now = Utc::now().timestamp_millis();
        let changed = transaction.execute(
            "UPDATE api_keys SET status = ?1, max_concurrency = ?2, revoked_at_ms = ?3 WHERE id = ?4",
            params![status, max_concurrency, if enabled { None } else { Some(now) }, key_id],
        )?;
        if changed == 0 {
            return Err(CoreError::ApiKeyNotFound { api_key_id: key_id.into() });
        }
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.update",
            "api_key",
            key_id,
            serde_json::json!({ "id": key_id, "status": status, "max_concurrency": max_concurrency }),
            now,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn rotate_api_key_as_admin(
        &self,
        principal: &Principal,
        key_id: &str,
    ) -> Result<IssuedApiKey, CoreError> {
        self.rotate_api_key_as_admin_inner(
            principal,
            key_id,
            Self::new_id("key"),
            Self::new_api_key(),
            None,
        )
    }

    pub fn rotate_api_key_as_admin_with_encrypted_copy<F>(
        &self,
        principal: &Principal,
        key_id: &str,
        encrypt_copy: F,
    ) -> Result<IssuedApiKey, CoreError>
    where
        F: FnOnce(&str, &str) -> Result<(Vec<u8>, u32), CoreError>,
    {
        let new_key_id = Self::new_id("key");
        let plaintext = Self::new_api_key();
        let encrypted_copy = encrypt_copy(&new_key_id, &plaintext)?;
        self.rotate_api_key_as_admin_inner(
            principal,
            key_id,
            new_key_id,
            plaintext,
            Some(encrypted_copy),
        )
    }

    fn rotate_api_key_as_admin_inner(
        &self,
        principal: &Principal,
        key_id: &str,
        new_key_id: String,
        plaintext: String,
        encrypted_copy: Option<(Vec<u8>, u32)>,
    ) -> Result<IssuedApiKey, CoreError> {
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let (secret_ciphertext, secret_key_version) = match encrypted_copy {
            Some((ciphertext, version)) if !ciphertext.is_empty() && version > 0 => {
                (Some(ciphertext), Some(i64::from(version)))
            }
            Some(_) => return Err(CoreError::ApiKeyEncryptionUnavailable),
            None => (None, None),
        };
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let target = transaction
            .query_row(
                "SELECT api_keys.user_id, users.name, users.role, api_keys.scopes_json, api_keys.max_concurrency
                 FROM api_keys INNER JOIN users ON users.id = api_keys.user_id
                 WHERE api_keys.id = ?1",
                [key_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::ApiKeyNotFound { api_key_id: key_id.into() })?;
        if target.2 == "admin" {
            return Err(CoreError::AdminRequired);
        }
        let scopes: BTreeSet<String> = Self::normalize_api_key_scopes(serde_json::from_str(&target.3)?);
        let scopes_json = serde_json::to_string(&scopes)?;
        transaction.execute(
            "UPDATE api_keys SET status = 'revoked', revoked_at_ms = ?1 WHERE id = ?2",
            params![now, key_id],
        )?;
        transaction.execute(
            "INSERT INTO api_keys (id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, status, created_at_ms, secret_ciphertext, secret_key_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, ?10)",
            params![&new_key_id, &target.0, &target.1, &prefix, &key_digest, &scopes_json, target.4, now, secret_ciphertext, secret_key_version],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.rotate",
            "api_key",
            key_id,
            serde_json::json!({ "old_id": key_id, "new_id": new_key_id, "user_id": target.0, "result": "rotated" }),
            now,
        )?;
        transaction.commit()?;

        Ok(IssuedApiKey { id: new_key_id, plaintext, prefix, user_id: target.0, scopes })
    }

    pub fn api_key_secret_as_admin(
        &self,
        principal: &Principal,
        key_id: &str,
    ) -> Result<ApiKeySecretRecord, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let secret = transaction
            .query_row(
                "SELECT secret_ciphertext, secret_key_version FROM api_keys WHERE id = ?1",
                [key_id],
                |row| Ok((row.get::<_, Option<Vec<u8>>>(0)?, row.get::<_, Option<i64>>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::ApiKeyNotFound { api_key_id: key_id.into() })?;
        let (Some(ciphertext), Some(version)) = secret else {
            return Err(CoreError::ApiKeySecretUnavailable { api_key_id: key_id.into() });
        };
        let key_version = u32::try_from(version)
            .ok()
            .filter(|version| *version > 0)
            .filter(|_| !ciphertext.is_empty())
            .ok_or_else(|| CoreError::ApiKeySecretUnavailable { api_key_id: key_id.into() })?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.copy",
            "api_key",
            key_id,
            serde_json::json!({ "key_id": key_id, "key_version": key_version, "result": "requested" }),
            Utc::now().timestamp_millis(),
        )?;
        transaction.commit()?;
        Ok(ApiKeySecretRecord {
            key_id: key_id.into(),
            ciphertext,
            key_version,
        })
    }

    pub fn rewrap_api_key_secrets_as_admin<F>(
        &self,
        principal: &Principal,
        target_version: u32,
        mut rewrap: F,
    ) -> Result<usize, CoreError>
    where
        F: FnMut(&str, u32, &[u8]) -> Result<(Vec<u8>, u32), CoreError>,
    {
        if target_version == 0 {
            return Err(CoreError::ApiKeyEncryptionUnavailable);
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT id, secret_ciphertext, secret_key_version FROM api_keys
                 WHERE secret_ciphertext IS NOT NULL OR secret_key_version IS NOT NULL
                 ORDER BY id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        let mut rewrapped = 0_usize;
        for (key_id, ciphertext, version) in rows {
            let (Some(ciphertext), Some(version)) = (ciphertext, version) else {
                return Err(CoreError::ApiKeySecretUnavailable { api_key_id: key_id });
            };
            let version = u32::try_from(version)
                .ok()
                .filter(|version| *version > 0)
                .filter(|_| !ciphertext.is_empty())
                .ok_or_else(|| CoreError::ApiKeySecretUnavailable { api_key_id: key_id.clone() })?;
            if version == target_version {
                continue;
            }
            let (new_ciphertext, new_version) = rewrap(&key_id, version, &ciphertext)?;
            if new_ciphertext.is_empty() || new_version != target_version {
                return Err(CoreError::ApiKeyEncryptionUnavailable);
            }
            transaction.execute(
                "UPDATE api_keys SET secret_ciphertext = ?1, secret_key_version = ?2 WHERE id = ?3",
                params![new_ciphertext, i64::from(new_version), &key_id],
            )?;
            rewrapped = rewrapped.saturating_add(1);
        }
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.vault_rewrap",
            "api_key_vault",
            "all",
            serde_json::json!({ "records_rewrapped": rewrapped, "key_version": target_version, "result": "rewrapped" }),
            Utc::now().timestamp_millis(),
        )?;
        transaction.commit()?;
        Ok(rewrapped)
    }

    pub fn set_user_status_as_admin(
        &self,
        principal: &Principal,
        user_id: &str,
        active: bool,
    ) -> Result<CoreUserAdminView, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let target = transaction
            .query_row(
                "SELECT role, status FROM users WHERE id = ?1",
                [user_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::UserNotFound { user_id: user_id.into() })?;
        if !active && user_id == principal.user_id {
            return Err(CoreError::AdminRequired);
        }
        if !active && target.0 == "admin" {
            let active_admins: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM users WHERE role = 'admin' AND status = 'active'",
                [],
                |row| row.get(0),
            )?;
            if active_admins <= 1 {
                return Err(CoreError::AdminRequired);
            }
        }
        let next_status = if active { "active" } else { "disabled" };
        if target.1 != next_status {
            let now = Utc::now().timestamp_millis();
            transaction.execute(
                "UPDATE users SET status = ?1, updated_at_ms = ?2 WHERE id = ?3",
                params![next_status, now, user_id],
            )?;
            Self::insert_audit_event(
                &transaction,
                &principal.user_id,
                "user.status",
                "user",
                user_id,
                serde_json::json!({ "user_id": user_id, "status": next_status }),
                now,
            )?;
        }
        let view = transaction.query_row(
            "SELECT id, name, role, status, created_at_ms, updated_at_ms FROM users WHERE id = ?1",
            [user_id],
            |row| {
                Ok(CoreUserAdminView {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    role: row.get(2)?,
                    status: row.get(3)?,
                    created_at_ms: row.get(4)?,
                    updated_at_ms: row.get(5)?,
                })
            },
        )?;
        transaction.commit()?;
        Ok(view)
    }

    /// The only bootstrap path: an empty Core database may create exactly one admin.
    pub fn create_bootstrap_admin(&self, input: NewUser, actor: &str) -> Result<User, CoreError> {
        if actor != "bootstrap" || input.role != crate::UserRole::Admin {
            return Err(CoreError::AdminRequired);
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let user_count: i64 = transaction.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        if user_count != 0 {
            return Err(CoreError::AdminRequired);
        }
        let user = Self::insert_user_in_transaction(&transaction, input, actor)?;
        transaction.commit()?;
        Ok(user)
    }

    fn create_user_inner(
        &self,
        input: NewUser,
        actor: &str,
        require_admin: bool,
    ) -> Result<User, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if require_admin {
            Self::authorize_admin_in_transaction(&transaction, actor)?;
        }
        let user = Self::insert_user_in_transaction(&transaction, input, actor)?;
        transaction.commit()?;
        Ok(user)
    }

    fn insert_user_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        input: NewUser,
        actor: &str,
    ) -> Result<User, CoreError> {
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
        self.issue_api_key_with_max_concurrency(
            user_id,
            name,
            scopes,
            DEFAULT_API_KEY_MAX_CONCURRENCY,
            actor,
        )
    }

    pub fn issue_api_key_with_max_concurrency(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
        actor: &str,
    ) -> Result<IssuedApiKey, CoreError> {
        self.issue_api_key_inner(user_id, name, scopes, max_concurrency, actor, false)
    }

    pub fn issue_api_key_as_admin(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        principal: &Principal,
    ) -> Result<IssuedApiKey, CoreError> {
        self.issue_api_key_as_admin_with_max_concurrency(
            user_id,
            name,
            scopes,
            DEFAULT_API_KEY_MAX_CONCURRENCY,
            principal,
        )
    }

    pub fn issue_api_key_as_admin_with_max_concurrency(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
        principal: &Principal,
    ) -> Result<IssuedApiKey, CoreError> {
        Self::validate_max_concurrency(max_concurrency)?;
        let plaintext = Self::new_api_key();
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let key_id = Self::new_id("key");
        let scopes = Self::normalize_api_key_scopes(scopes);
        let scopes_json = serde_json::to_string(&scopes)?;
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, user_id)?;
        transaction.execute(
            "INSERT INTO api_keys \
             (id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8)",
            params![key_id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, now],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
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

    fn issue_api_key_inner(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        max_concurrency: i64,
        actor: &str,
        require_admin: bool,
    ) -> Result<IssuedApiKey, CoreError> {
        Self::validate_max_concurrency(max_concurrency)?;
        let plaintext = Self::new_api_key();
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let key_id = Self::new_id("key");
        let scopes = Self::normalize_api_key_scopes(scopes);
        let scopes_json = serde_json::to_string(&scopes)?;
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if require_admin {
            Self::authorize_admin_in_transaction(&transaction, actor)?;
        }
        Self::ensure_active_user(&transaction, user_id)?;
        transaction.execute(
            "INSERT INTO api_keys \
             (id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8)",
            params![key_id, user_id, name, prefix, key_digest, scopes_json, max_concurrency, now],
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

    fn validate_max_concurrency(max_concurrency: i64) -> Result<(), CoreError> {
        if !(1..=1024).contains(&max_concurrency) {
            return Err(CoreError::Validation {
                field: "max_concurrency".into(),
                reason: "must be between 1 and 1024".into(),
            });
        }
        Ok(())
    }

    fn normalize_api_key_scopes(mut scopes: BTreeSet<String>) -> BTreeSet<String> {
        if scopes.contains("videos:submit") {
            scopes.insert("videos:read".into());
        }
        scopes
    }

    pub fn revoke_api_key(&self, key_id: &str, actor: &str) -> Result<(), CoreError> {
        self.revoke_api_key_inner(key_id, actor, false)
    }

    pub fn revoke_api_key_as_admin(
        &self,
        principal: &Principal,
        key_id: &str,
    ) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1)",
            [key_id],
            |row| row.get(0),
        )?;
        if !exists {
            return Err(CoreError::ApiKeyNotFound { api_key_id: key_id.into() });
        }
        let now = Utc::now().timestamp_millis();
        let changed = transaction.execute(
            "UPDATE api_keys SET status = 'revoked', revoked_at_ms = ?1
             WHERE id = ?2 AND status = 'active'",
            params![now, key_id],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "api_key.revoke",
            "api_key",
            key_id,
            serde_json::json!({
                "id": key_id,
                "result": if changed == 1 { "revoked" } else { "already_revoked" },
            }),
            now,
        )?;
        transaction.commit().map_err(CoreError::from)
    }

    fn revoke_api_key_inner(&self, key_id: &str, actor: &str, require_admin: bool) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if require_admin {
            Self::authorize_admin_in_transaction(&transaction, actor)?;
        }
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
            "SELECT api_keys.id, api_keys.user_id, api_keys.key_digest, api_keys.scopes_json \
             FROM api_keys INNER JOIN users ON users.id = api_keys.user_id \
             WHERE api_keys.status = 'active' AND users.status = 'active'",
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

    /// Management authorization is deliberately checked inside the write transaction.
    pub fn authorize_admin(&self, actor_user_id: &str) -> Result<(), CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::authorize_admin_in_connection(&connection, actor_user_id)
    }

    pub fn authorize_admin_principal(&self, principal: &Principal) -> Result<(), CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        transaction.commit().map_err(CoreError::from)
    }

    pub fn legacy_key_is_disabled(data_dir: &Path, presented: &str) -> Result<bool, CoreError> {
        let database_path = data_dir.join("data").join(CORE_DB_FILE);
        if !database_path.is_file() {
            return Ok(false);
        }
        let connection = Connection::open(database_path)?;
        let registry_exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'legacy_key_registry')",
            [],
            |row| row.get(0),
        )?;
        if !registry_exists {
            return Ok(false);
        }
        let digest = Self::digest_api_key(presented);
        let disabled = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM legacy_key_registry WHERE key_digest = ?1 AND status IN ('migration_legacy','disabled'))",
                [digest],
                |row| row.get::<_, bool>(0),
            )
            .optional()?;
        Ok(disabled.unwrap_or(false))
    }

    /// Import all validated legacy records in one SQLite `BEGIN IMMEDIATE` transaction.
    /// The legacy plaintext keys are accepted only as transient input for digesting.
    pub fn apply_legacy_migration(
        &self,
        batch: LegacyMigrationBatch,
    ) -> Result<LegacyMigrationResult, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, &batch.actor)?;
        Self::validate_migration_batch(&transaction, &batch)?;

        let now = Utc::now().timestamp_millis();
        for (source_file, source_hash) in &batch.source_hashes {
            transaction.execute(
                "INSERT INTO legacy_migration_records (migration_id, source_file, source_hash, actor_user_id, reason, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![batch.migration_id, source_file, source_hash, batch.actor.user_id, batch.reason, now],
            )?;
        }
        Self::insert_audit_event(
            &transaction,
            &batch.actor.user_id,
            "legacy.migration",
            "legacy_migration",
            &batch.migration_id,
            serde_json::json!({"source_files": batch.source_hashes.len(), "reason": batch.reason}),
            now,
        )?;

        let mut issued_keys = Vec::with_capacity(batch.keys.len());
        let scopes = Self::normalize_api_key_scopes(batch.scopes.clone());
        for key in &batch.keys {
            let plaintext = Self::new_api_key();
            let prefix = plaintext[..16].to_owned();
            let key_digest = Self::digest_api_key(&plaintext);
            let key_id = Self::new_id("key");
            let scopes_json = serde_json::to_string(&scopes)?;
            transaction.execute(
                "INSERT INTO api_keys (id, user_id, name, prefix, key_digest, scopes_json, status, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
                params![key_id, key.user_id, "legacy migration", prefix, key_digest, scopes_json, now],
            )?;
            transaction.execute(
                "INSERT INTO legacy_key_registry (legacy_key_id, key_digest, migrated_user_id, status, migration_id, actor_user_id, reason, disabled_at_ms) VALUES (?1, ?2, ?3, 'disabled', ?4, ?5, ?6, ?7)",
                params![key.legacy_key_id, Self::digest_api_key(&key.legacy_key), key.user_id, batch.migration_id, batch.actor.user_id, batch.reason, now],
            )?;
            Self::insert_audit_event(
                &transaction,
                &batch.actor.user_id,
                "api_key.issue",
                "api_key",
                &key_id,
                serde_json::json!({"migration_id": batch.migration_id, "reason": batch.reason, "result": "issued"}),
                now,
            )?;
            Self::insert_audit_event(
                &transaction,
                &batch.actor.user_id,
                "legacy.key_migrate",
                "legacy_key",
                &key.legacy_key_id,
                serde_json::json!({"migration_id": batch.migration_id, "reason": batch.reason, "status": "disabled"}),
                now,
            )?;
            issued_keys.push(IssuedApiKey {
                id: key_id,
                plaintext,
                prefix,
                user_id: key.user_id.clone(),
                scopes: scopes.clone(),
            });
        }

        for asset in &batch.assets {
            transaction.execute(
                "INSERT INTO legacy_assets (id, owner_key_id, user_id, filename, mime_type, extension, size, content_sha256, created_at_ms, expires_at_ms, storage_ref, migration_status, migration_id, actor_user_id, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
                params![asset.id, asset.owner_key_id, asset.user_id, asset.filename, asset.mime_type, asset.extension, asset.size, asset.content_sha256, asset.created_at_ms, asset.expires_at_ms, asset.storage_ref, asset.migration_status, batch.migration_id, batch.actor.user_id, batch.reason],
            )?;
            Self::insert_audit_event(&transaction, &batch.actor.user_id, "legacy.asset_import", "legacy_asset", &asset.id, serde_json::json!({"migration_id": batch.migration_id, "reason": batch.reason}), now)?;
        }

        for job in &batch.jobs {
            let reconcile_required = i64::from(job.status == "processing");
            let status = if job.status == "processing" { "unknown" } else { job.status.as_str() };
            transaction.execute(
                "INSERT INTO legacy_jobs (id, owner_key_id, user_id, status, reconcile_required, created_at_ms, updated_at_ms, migration_id, actor_user_id, reason) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![job.id, job.owner_key_id, job.user_id, status, reconcile_required, job.created_at_ms, job.updated_at_ms, batch.migration_id, batch.actor.user_id, batch.reason],
            )?;
            Self::insert_audit_event(&transaction, &batch.actor.user_id, "legacy.job_import", "legacy_job", &job.id, serde_json::json!({"migration_id": batch.migration_id, "reason": batch.reason, "reconcile_required": reconcile_required == 1}), now)?;
        }

        for observation in &batch.observations {
            transaction.execute(
                "INSERT INTO legacy_observations (id, account_ref, resource_kind, value_json, observed_value, summary_json, source, observed_at_ms, migration_id, actor_user_id, reason) VALUES (?1, ?2, ?3, 'null', ?4, ?5, 'json_cache', ?6, ?7, ?8, ?9)",
                params![observation.id, observation.account_ref, observation.resource_kind, observation.observed_value, observation.summary_json, observation.observed_at_ms, batch.migration_id, batch.actor.user_id, batch.reason],
            )?;
            Self::insert_audit_event(&transaction, &batch.actor.user_id, "legacy.observation_import", "legacy_observation", &observation.id, serde_json::json!({"migration_id": batch.migration_id, "reason": batch.reason, "source": "json_cache"}), now)?;
        }

        transaction.commit()?;
        Ok(LegacyMigrationResult {
            migration_id: batch.migration_id,
            issued_keys,
        })
    }

    pub(crate) fn authorize_admin_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        actor_user_id: &str,
    ) -> Result<(), CoreError> {
        let role = transaction
            .query_row(
                "SELECT role FROM users WHERE id = ?1 AND status = 'active'",
                [actor_user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if role.as_deref() == Some("admin") {
            Ok(())
        } else {
            Err(CoreError::AdminRequired)
        }
    }

    pub(crate) fn authorize_admin_principal_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        principal: &Principal,
    ) -> Result<(), CoreError> {
        if principal.key_id.starts_with("admin_session:") {
            let authorized = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1 AND status = 'active' AND role = 'admin')",
                [&principal.user_id],
                |row| row.get(0),
            )?;
            return if authorized { Ok(()) } else { Err(CoreError::AdminRequired) };
        }
        let authorized = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys INNER JOIN users ON users.id = api_keys.user_id WHERE api_keys.id = ?1 AND api_keys.user_id = ?2 AND api_keys.status = 'active' AND users.status = 'active' AND users.role = 'admin')",
            params![principal.key_id, principal.user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if authorized {
            Ok(())
        } else {
            Err(CoreError::AdminRequired)
        }
    }

    fn authorize_admin_in_connection(
        connection: &Connection,
        actor_user_id: &str,
    ) -> Result<(), CoreError> {
        let role = connection
            .query_row(
                "SELECT role FROM users WHERE id = ?1 AND status = 'active'",
                [actor_user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if role.as_deref() == Some("admin") {
            Ok(())
        } else {
            Err(CoreError::AdminRequired)
        }
    }

    pub(crate) fn ensure_active_user(
        transaction: &rusqlite::Transaction<'_>,
        user_id: &str,
    ) -> Result<(), CoreError> {
        let active = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1 AND status = 'active')",
            [user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if active { Ok(()) } else { Err(CoreError::UserNotActive) }
    }

    pub(crate) fn ensure_principal_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        principal: &Principal,
    ) -> Result<(), CoreError> {
        let valid = transaction.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM api_keys
               INNER JOIN users ON users.id = api_keys.user_id
               WHERE api_keys.id = ?1 AND api_keys.user_id = ?2
                 AND api_keys.status = 'active' AND users.status = 'active'
             )",
            params![&principal.key_id, &principal.user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if valid {
            Ok(())
        } else {
            Err(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: principal.key_id.clone(),
            })
        }
    }

    pub(crate) fn ensure_principal_on_connection(
        connection: &Connection,
        principal: &Principal,
    ) -> Result<(), CoreError> {
        let valid = connection.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM api_keys
               INNER JOIN users ON users.id = api_keys.user_id
               WHERE api_keys.id = ?1 AND api_keys.user_id = ?2
                 AND api_keys.status = 'active' AND users.status = 'active'
             )",
            params![&principal.key_id, &principal.user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if valid {
            Ok(())
        } else {
            Err(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: principal.key_id.clone(),
            })
        }
    }

    fn validate_migration_batch(
        transaction: &rusqlite::Transaction<'_>,
        batch: &LegacyMigrationBatch,
    ) -> Result<(), CoreError> {
        if batch.migration_id.trim().is_empty() || batch.reason.trim().is_empty() {
            return Err(CoreError::MigrationValidation { reason: "migration id and reason are required".into() });
        }
        if batch.source_hashes.is_empty() || batch.source_hashes.values().any(|hash| hash == "missing" || hash.is_empty()) {
            return Err(CoreError::MigrationValidation { reason: "all source files must be present and hashed".into() });
        }
        let existing: Option<String> = transaction.query_row("SELECT migration_id FROM legacy_migration_records WHERE migration_id = ?1 LIMIT 1", [&batch.migration_id], |row| row.get(0)).optional()?;
        if existing.is_some() {
            return Err(CoreError::MigrationValidation { reason: "migration id already exists".into() });
        }
        let mut seen = BTreeSet::new();
        for key in &batch.keys {
            if key.legacy_key_id.trim().is_empty() || key.legacy_key.is_empty() {
                return Err(CoreError::MigrationValidation { reason: "legacy key record is incomplete".into() });
            }
            if !seen.insert(&key.legacy_key_id) {
                return Err(CoreError::MigrationValidation { reason: "duplicate legacy key id".into() });
            }
            Self::ensure_active_user(transaction, &key.user_id)?;
            let exists: Option<String> = transaction.query_row("SELECT legacy_key_id FROM legacy_key_registry WHERE legacy_key_id = ?1", [&key.legacy_key_id], |row| row.get(0)).optional()?;
            if exists.is_some() {
                return Err(CoreError::MigrationValidation { reason: "legacy key was already migrated".into() });
            }
        }
        seen.clear();
        for asset in &batch.assets {
            if !seen.insert(&asset.id) { return Err(CoreError::MigrationValidation { reason: "duplicate legacy asset id".into() }); }
            let valid_storage_ref = Self::valid_legacy_storage_ref(&asset.storage_ref);
            if (asset.migration_status == "verified" && !valid_storage_ref)
                || (asset.migration_status != "verified"
                    && !asset.storage_ref.is_empty()
                    && !valid_storage_ref)
            {
                return Err(CoreError::MigrationValidation { reason: "legacy asset storage_ref is invalid for migration status".into() });
            }
            if !matches!(asset.migration_status.as_str(), "verified" | "legacy_unverified" | "reconcile_required") {
                return Err(CoreError::MigrationValidation { reason: "unknown legacy asset migration status".into() });
            }
            Self::ensure_active_user(transaction, &asset.user_id)?;
        }
        seen.clear();
        for job in &batch.jobs {
            if !seen.insert(&job.id) { return Err(CoreError::MigrationValidation { reason: "duplicate legacy job id".into() }); }
            if job.status == "processing" {
                return Err(CoreError::MigrationValidation { reason: "processing legacy job requires reconciliation".into() });
            }
            if !matches!(job.status.as_str(), "queued" | "processing" | "completed" | "failed") {
                return Err(CoreError::MigrationValidation { reason: "unknown legacy job status".into() });
            }
            Self::ensure_active_user(transaction, &job.user_id)?;
        }
        seen.clear();
        for observation in &batch.observations {
            if !seen.insert(&observation.id) { return Err(CoreError::MigrationValidation { reason: "duplicate legacy observation id".into() }); }
            serde_json::from_str::<serde_json::Value>(&observation.summary_json)?;
        }
        Ok(())
    }

    fn new_api_key() -> String {
        let mut material = [0_u8; 32];
        OsRng.fill_bytes(&mut material);
        format!("aw_live_{}", URL_SAFE_NO_PAD.encode(material))
    }

    fn valid_legacy_storage_ref(storage_ref: &str) -> bool {
        !storage_ref.is_empty()
            && storage_ref == storage_ref.trim()
            && storage_ref.starts_with("assets/")
            && storage_ref.len() > "assets/".len()
            && !storage_ref.contains("..")
            && !storage_ref.contains('\\')
            && !storage_ref.contains("//")
            && !storage_ref.chars().any(|character| character.is_control())
    }

    fn validate_upstream_account(input: &RegisterUpstreamAccount) -> Result<(), CoreError> {
        validate_required("upstream account id", &input.id)?;
        validate_required("upstream account provider", &input.provider)?;
        validate_opaque_credentials_ref(&input.credentials_ref)?;
        if input.max_concurrency <= 0 {
            return Err(CoreError::Validation { field: "max_concurrency".into(), reason: "must be positive".into() });
        }
        if input.consecutive_errors < 0 {
            return Err(CoreError::Validation { field: "consecutive_errors".into(), reason: "must not be negative".into() });
        }
        Ok(())
    }

    fn validate_upstream_observation(observation: &UpstreamObservation) -> Result<(), CoreError> {
        validate_required("upstream observation id", &observation.id)?;
        validate_required("account_ref", &observation.account_ref)?;
        validate_required("resource_kind", &observation.resource_kind)?;
        validate_required("source", &observation.source)?;
        if observation.value_scale <= 0 {
            return Err(CoreError::Validation { field: "value_scale".into(), reason: "must be positive".into() });
        }
        if observation.stale_at_ms < observation.observed_at_ms {
            return Err(CoreError::Validation { field: "stale_at_ms".into(), reason: "must not precede observed_at_ms".into() });
        }
        Ok(())
    }

    fn upstream_account_state_from_db(value: &str) -> Option<UpstreamAccountState> {
        match value {
            "available" => Some(UpstreamAccountState::Available),
            "cooling" => Some(UpstreamAccountState::Cooling),
            "forbidden" => Some(UpstreamAccountState::Forbidden),
            "disabled" => Some(UpstreamAccountState::Disabled),
            _ => None,
        }
    }

    fn upstream_account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UpstreamAccount> {
        let capabilities_json: String = row.get(4)?;
        let capabilities = serde_json::from_str(&capabilities_json).map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                4,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })?;
        let state_value: String = row.get(7)?;
        let state = Self::upstream_account_state_from_db(&state_value).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                "invalid upstream account state".into(),
            )
        })?;
        Ok(UpstreamAccount {
            id: row.get(0)?,
            provider: row.get(1)?,
            credentials_ref: row.get(2)?,
            region: row.get(3)?,
            capabilities,
            enabled: row.get::<_, i64>(5)? != 0,
            max_concurrency: row.get(6)?,
            state,
            cooldown_until_ms: row.get(8)?,
            cooldown_reason: row.get(9)?,
            consecutive_errors: row.get(10)?,
            created_at_ms: row.get(11)?,
            updated_at_ms: row.get(12)?,
        })
    }

    fn upstream_account_in_transaction(
        transaction: &rusqlite::Transaction<'_>,
        account_ref: &str,
    ) -> Result<UpstreamAccount, CoreError> {
        transaction
            .query_row(
                "SELECT id, provider, credentials_ref, region, capabilities_json,
                        enabled, max_concurrency, state, cooldown_until_ms,
                        cooldown_reason, consecutive_errors, created_at_ms,
                        updated_at_ms
                 FROM upstream_accounts WHERE id = ?1",
                [account_ref],
                Self::upstream_account_from_row,
            )
            .optional()?
            .ok_or_else(|| CoreError::Validation {
                field: "account_ref".into(),
                reason: "must identify a registered upstream account".into(),
            })
    }

    fn upstream_observation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UpstreamObservation> {
        let status: String = row.get(6)?;
        let status = ObservationStatus::from_db(&status).ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(
            6, rusqlite::types::Type::Text, "invalid upstream observation status".into(),
        ))?;
        let summary_json: String = row.get(9)?;
        let summary = serde_json::from_str(&summary_json).map_err(|error| rusqlite::Error::FromSqlConversionFailure(
            9, rusqlite::types::Type::Text, Box::new(error),
        ))?;
        Ok(UpstreamObservation {
            id: row.get(0)?, account_ref: row.get(1)?, resource_kind: row.get(2)?,
            observed_value: row.get(3)?, value_scale: row.get(4)?, source: row.get(5)?,
            status, observed_at_ms: row.get(7)?, stale_at_ms: row.get(8)?, summary,
        })
    }

    fn asset_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoreAsset> {
        let state_value: String = row.get(11)?;
        let state = AssetState::from_db(&state_value).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                11,
                rusqlite::types::Type::Text,
                "invalid asset state".into(),
            )
        })?;
        Ok(CoreAsset {
            id: row.get(0)?,
            user_id: row.get(1)?,
            filename: row.get(2)?,
            mime_type: row.get(3)?,
            extension: row.get(4)?,
            size: row.get(5)?,
            sha256: row.get(6)?,
            storage_ref: row.get(7)?,
            content_token_digest: row.get(8)?,
            created_at_ms: row.get(9)?,
            expires_at_ms: row.get(10)?,
            state,
        })
    }

    pub(crate) fn upstream_lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<UpstreamLease> {
        let state: String = row.get(6)?;
        let state = LeaseState::from_db(&state).ok_or_else(|| rusqlite::Error::FromSqlConversionFailure(
            6, rusqlite::types::Type::Text, "invalid upstream lease state".into(),
        ))?;
        Ok(UpstreamLease {
            id: row.get(0)?, request_id: row.get(1)?, account_ref: row.get(2)?, resource_kind: row.get(3)?,
            predicted_units: row.get(4)?, observation_id: row.get(5)?, state,
            lease_expires_at_ms: row.get(7)?, reconcile_until_ms: row.get(8)?,
            upstream_request_ref: row.get(9)?, error_kind: row.get(10)?,
            created_at_ms: row.get(11)?, updated_at_ms: row.get(12)?, settled_at_ms: row.get(13)?,
        })
    }

    pub(crate) fn new_id(kind: &str) -> String {
        let mut material = [0_u8; 16];
        OsRng.fill_bytes(&mut material);
        format!("{kind}_{}", URL_SAFE_NO_PAD.encode(material))
    }

    fn digest_api_key(plaintext: &str) -> Vec<u8> {
        Sha256::digest(plaintext.as_bytes()).to_vec()
    }

    pub(crate) fn insert_audit_event(
        transaction: &rusqlite::Transaction<'_>,
        actor: &str,
        action: &str,
        target_type: &str,
        target_id: &str,
        metadata: serde_json::Value,
        now: i64,
    ) -> Result<(), CoreError> {
        let metadata = Self::sanitize_audit_metadata(metadata);
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

    /// Keep the audit table useful for operations without making it a second
    /// credential or account-identifier store. Older call sites still pass
    /// legacy metadata keys, so sanitization belongs at this single write
    /// boundary rather than relying on every caller to remember the policy.
    fn sanitize_audit_metadata(value: Value) -> Value {
        match value {
            Value::Object(object) => {
                let mut sanitized = Map::new();
                for (key, value) in object {
                    match key.as_str() {
                        "account_ref" => {
                            if let Some(raw) = value.as_str() {
                                sanitized.insert("account_hash".into(), Value::String(audit_hash(raw)));
                            }
                        }
                        "observation_id" => {
                            if let Some(raw) = value.as_str() {
                                if let Some(safe) = audit_identifier(Some(raw)) {
                                    sanitized.insert("observation_id".into(), Value::String(safe));
                                } else {
                                    sanitized.insert("observation".into(), Value::String(audit_hash(raw)));
                                }
                            }
                        }
                        "credentials_ref" | "credential" | "jwt" | "token" | "cookie"
                        | "authorization" | "prompt" | "body" | "request_body" | "response_body" => {}
                        "provider" | "resource_kind" | "source" | "error_category" | "error_kind" => {
                            if let Some(raw) = value.as_str() {
                                sanitized.insert(key, Value::String(audit_label(raw)));
                            } else {
                                sanitized.insert(key, Self::sanitize_audit_metadata(value));
                            }
                        }
                        "request_id" | "lease_id" | "reservation_id" => {
                            if let Some(raw) = value.as_str() {
                                if let Some(safe) = audit_identifier(Some(raw)) {
                                    sanitized.insert(key, Value::String(safe));
                                }
                            } else {
                                sanitized.insert(key, Self::sanitize_audit_metadata(value));
                            }
                        }
                        _ => {
                            sanitized.insert(key, Self::sanitize_audit_metadata(value));
                        }
                    }
                }
                Value::Object(sanitized)
            }
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .map(Self::sanitize_audit_metadata)
                    .collect(),
            ),
            other => other,
        }
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

    fn migrate_v1_to_v2(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let request_indexes = {
            let mut statement = transaction
                .prepare(
                    "SELECT sql FROM sqlite_master \
                     WHERE type = 'index' AND tbl_name IN ('requests', 'idempotency_keys') AND sql IS NOT NULL",
                )
                .map_err(CoreError::migration)?;
            let indexes = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(CoreError::migration)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CoreError::migration)?;
            indexes
        };

        transaction.execute_batch(SCHEMA_V2).map_err(CoreError::migration)?;
        for index_sql in request_indexes {
            transaction.execute_batch(&index_sql).map_err(CoreError::migration)?;
        }
        Ok(())
    }

    fn migrate_v2_to_v3(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V3).map_err(CoreError::migration)
    }

    fn migrate_v3_to_v4(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V4).map_err(CoreError::migration)
    }

    fn migrate_v4_to_v5(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction
            .execute(
                "UPDATE legacy_assets
                 SET migration_status = 'legacy_unverified',
                     storage_ref = CASE
                       WHEN storage_ref = '' THEN storage_ref
                       ELSE ''
                     END
                 WHERE migration_status = 'verified'
                   AND (
                     storage_ref = ''
                     OR storage_ref NOT GLOB 'assets/*'
                     OR length(storage_ref) <= length('assets/')
                     OR trim(storage_ref) <> storage_ref
                     OR instr(storage_ref, '..') > 0
                     OR instr(storage_ref, char(92)) > 0
                     OR instr(storage_ref, '//') > 0
                     OR instr(storage_ref, char(9)) > 0
                     OR instr(storage_ref, char(10)) > 0
                     OR instr(storage_ref, char(13)) > 0
                   )",
                [],
            )
            .map_err(CoreError::migration)?;
        transaction.execute_batch(SCHEMA_V5).map_err(CoreError::migration)
    }

    fn migrate_v7_to_v8(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V8).map_err(CoreError::migration)
    }

    fn migrate_v8_to_v9(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V9).map_err(CoreError::migration)
    }

    fn migrate_v9_to_v10(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V10).map_err(CoreError::migration)
    }

    fn migrate_v10_to_v11(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V11).map_err(CoreError::migration)
    }

    fn migrate_v11_to_v12(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V12).map_err(CoreError::migration)?;

        // A few early installations created placeholder quota tables before
        // the quota schema was finalized. Make those empty placeholders
        // query-compatible, but never silently reinterpret rows whose owner
        // or resource cannot be established.
        for (table, required_columns) in [
            (
                "quota_ledger",
                &[
                    ("user_id", "user_id TEXT"),
                    ("resource_kind", "resource_kind TEXT"),
                    ("event_kind", "event_kind TEXT"),
                    ("amount", "amount INTEGER"),
                    ("delta", "delta INTEGER"),
                    ("request_id", "request_id TEXT"),
                    ("actor_user_id", "actor_user_id TEXT"),
                    ("reason", "reason TEXT"),
                    ("created_at_ms", "created_at_ms INTEGER"),
                ][..],
            ),
            (
                "quota_reservations",
                &[
                    ("user_id", "user_id TEXT"),
                    ("request_id", "request_id TEXT"),
                    ("resource_kind", "resource_kind TEXT"),
                    ("amount", "amount INTEGER"),
                    ("state", "state TEXT"),
                    ("expires_at_ms", "expires_at_ms INTEGER"),
                    ("created_at_ms", "created_at_ms INTEGER"),
                    ("settled_at_ms", "settled_at_ms INTEGER"),
                ][..],
            ),
        ] {
            let mut missing = Vec::new();
            for &(column, declaration) in required_columns {
                let exists: bool = transaction.query_row(
                    "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                    params![table, column],
                    |row| row.get(0),
                )?;
                if !exists {
                    missing.push((column, declaration));
                }
            }
            if missing.is_empty() {
                continue;
            }

            let row_count: i64 = transaction.query_row(
                &format!("SELECT COUNT(*) FROM {table}"),
                [],
                |row| row.get(0),
            )?;
            if row_count > 0 {
                return Err(CoreError::MigrationValidation {
                    reason: format!(
                        "cannot migrate non-empty sparse {table} table without its owner/resource columns"
                    ),
                });
            }
            for (_, declaration) in missing {
                transaction.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {declaration}"),
                    [],
                )?;
            }
        }

        for (table, column, declaration) in [
            ("quota_ledger", "budget_account_id", "ALTER TABLE quota_ledger ADD COLUMN budget_account_id TEXT REFERENCES quota_budget_accounts(id)"),
            ("quota_ledger", "event_group_id", "ALTER TABLE quota_ledger ADD COLUMN event_group_id TEXT"),
            ("quota_ledger", "api_key_id", "ALTER TABLE quota_ledger ADD COLUMN api_key_id TEXT REFERENCES api_keys(id)"),
            ("quota_ledger", "budget_version", "ALTER TABLE quota_ledger ADD COLUMN budget_version INTEGER"),
            ("quota_reservations", "api_key_id", "ALTER TABLE quota_reservations ADD COLUMN api_key_id TEXT REFERENCES api_keys(id)"),
            ("quota_reservations", "key_budget_account_id", "ALTER TABLE quota_reservations ADD COLUMN key_budget_account_id TEXT REFERENCES quota_budget_accounts(id)"),
            ("quota_reservations", "user_cap_account_id", "ALTER TABLE quota_reservations ADD COLUMN user_cap_account_id TEXT REFERENCES quota_budget_accounts(id)"),
            ("quota_reservations", "event_group_id", "ALTER TABLE quota_reservations ADD COLUMN event_group_id TEXT"),
        ] {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2)",
                params![table, column],
                |row| row.get(0),
            )?;
            if !exists {
                transaction.execute(declaration, [])?;
            }
        }

        let mut account_statement = transaction.prepare(
            "SELECT user_id, resource_kind FROM quota_ledger
             WHERE user_id IS NOT NULL AND resource_kind IS NOT NULL
             UNION
             SELECT user_id, resource_kind FROM quota_reservations
             WHERE user_id IS NOT NULL AND resource_kind IS NOT NULL
             ORDER BY user_id, resource_kind",
        )?;
        let mut account_keys = account_statement
            .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(account_statement);
        for (user_id, resource_kind) in account_keys.drain(..) {
            let account_id = transaction
                .query_row(
                    "SELECT id FROM quota_budget_accounts
                     WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = ?2",
                    params![&user_id, &resource_kind],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let unresolved: bool = transaction.query_row(
                "SELECT EXISTS(
                   SELECT 1 FROM quota_reservations
                   WHERE user_id = ?1 AND resource_kind = ?2 AND state IN ('held', 'unknown')
                 )",
                params![&user_id, &resource_kind],
                |row| row.get(0),
            )?;
            let account_id = match account_id {
                Some(account_id) => account_id,
                None => {
                    let id = Self::new_id("budget");
                    let now = Utc::now().timestamp_millis();
                    transaction.execute(
                        "INSERT INTO quota_budget_accounts
                         (id, scope, user_id, api_key_id, resource_kind, enabled, version,
                          migration_state, created_at_ms, updated_at_ms)
                         VALUES (?1, 'user_cap', ?2, NULL, ?3, 1, 1, ?4, ?5, ?5)",
                        params![
                            &id,
                            &user_id,
                            &resource_kind,
                            if unresolved {
                                QuotaMigrationState::ReconcileRequired.as_str()
                            } else {
                                QuotaMigrationState::LegacyUnassigned.as_str()
                            },
                            now,
                        ],
                    )?;
                    id
                }
            };

            transaction.execute(
                "UPDATE quota_ledger
                 SET budget_account_id = ?1,
                     event_group_id = COALESCE(event_group_id, 'legacy-' || entry_id),
                     budget_version = COALESCE(budget_version, 1)
                 WHERE user_id = ?2 AND resource_kind = ?3",
                params![&account_id, &user_id, &resource_kind],
            )?;

            let reservations = {
                let mut statement = transaction.prepare(
                    "SELECT id, request_id FROM quota_reservations
                     WHERE user_id = ?1 AND resource_kind = ?2",
                )?;
                let rows = statement
                    .query_map(params![&user_id, &resource_kind], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                rows
            };
            for (reservation_id, request_id) in reservations {
                let request_key = transaction
                    .query_row(
                        "SELECT api_key_id FROM requests WHERE id = ?1",
                        [&request_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                if let Some(api_key_id) = request_key {
                    let owner_matches: bool = transaction.query_row(
                        "SELECT EXISTS(
                           SELECT 1 FROM api_keys
                           WHERE id = ?1 AND user_id = ?2
                         )",
                        params![&api_key_id, &user_id],
                        |row| row.get(0),
                    )?;
                    if !owner_matches {
                        return Err(CoreError::MigrationValidation {
                            reason: format!(
                                "quota reservation {reservation_id} references a Key owned by another user"
                            ),
                        });
                    }
                    transaction.execute(
                        "UPDATE quota_reservations
                         SET api_key_id = ?1,
                             user_cap_account_id = ?2,
                             event_group_id = COALESCE(event_group_id, 'legacy-reservation-' || id)
                         WHERE id = ?3",
                        params![&api_key_id, &account_id, &reservation_id],
                    )?;
                } else {
                    transaction.execute(
                        "UPDATE quota_reservations
                         SET user_cap_account_id = ?1,
                             event_group_id = COALESCE(event_group_id, 'legacy-reservation-' || id)
                         WHERE id = ?2",
                        params![&account_id, &reservation_id],
                    )?;
                }
            }
        }
        Ok(())
    }

    fn migrate_v12_to_v13(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let table_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'api_keys')",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(());
        }
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') WHERE name = 'max_concurrency')",
            [],
            |row| row.get(0),
        )?;
        if exists {
            Ok(())
        } else {
            transaction.execute_batch(SCHEMA_V13).map_err(CoreError::migration)
        }
    }

    fn migrate_v13_to_v14(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V14).map_err(CoreError::migration)
    }

    fn migrate_v14_to_v15(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let table_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'api_keys')",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(());
        }
        let scopes_column_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('api_keys') WHERE name = 'scopes_json')",
            [],
            |row| row.get(0),
        )?;
        if !scopes_column_exists {
            return Ok(());
        }
        let mut updates = Vec::new();
        {
            let mut statement = transaction.prepare("SELECT id, scopes_json FROM api_keys")?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            for row in rows {
                let (key_id, scopes_json) = row?;
                let scopes: BTreeSet<String> = serde_json::from_str(&scopes_json)?;
                let normalized = Self::normalize_api_key_scopes(scopes.clone());
                if normalized != scopes {
                    updates.push((key_id, serde_json::to_string(&normalized)?));
                }
            }
        }
        for (key_id, scopes_json) in updates {
            transaction.execute(
                "UPDATE api_keys SET scopes_json = ?1 WHERE id = ?2",
                params![scopes_json, key_id],
            )?;
        }
        Ok(())
    }

    fn migrate_v15_to_v16(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V16).map_err(CoreError::migration)
    }

    fn migrate_v16_to_v17(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V17).map_err(CoreError::migration)
    }

    fn migrate_v17_to_v18(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        for (table, id_column, value_column, nullable) in [
            ("quota_ledger", "entry_id", "amount", false),
            ("quota_ledger", "entry_id", "delta", false),
            ("quota_reservations", "id", "amount", false),
            ("cost_policies", "id", "reserve_amount", false),
            ("cost_policies", "id", "max_actual_amount", true),
        ] {
            Self::scale_credit_column_to_microcredits(
                transaction,
                table,
                id_column,
                value_column,
                nullable,
            )?;
        }
        Ok(())
    }

    fn migrate_v18_to_v19(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        transaction.execute_batch(SCHEMA_V19).map_err(CoreError::migration)?;
        let idempotency_table_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'idempotency_keys')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let requests_table_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'requests')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if idempotency_table_exists && requests_table_exists {
            let mappings = {
                let mut statement = transaction.prepare(
                    "SELECT idempotency_keys.scope, idempotency_keys.client_key,
                            idempotency_keys.request_id, requests.user_id,
                            requests.api_key_id, requests.endpoint
                     FROM idempotency_keys
                     INNER JOIN requests ON requests.id = idempotency_keys.request_id
                     ORDER BY idempotency_keys.scope, idempotency_keys.client_key",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (old_scope, client_key, request_id, user_id, api_key_id, endpoint) in mappings {
                let new_scope = format!("{user_id}:{api_key_id}:{endpoint}");
                if old_scope != new_scope {
                    transaction.execute(
                        "UPDATE idempotency_keys SET scope = ?1 WHERE scope = ?2 AND client_key = ?3",
                        params![new_scope, old_scope, client_key],
                    ).map_err(|error| {
                        CoreError::MigrationValidation {
                            reason: format!(
                                "cannot re-key legacy idempotency record for request {request_id}: {error}"
                            ),
                        }
                    })?;
                }
            }
        }
        Ok(())
    }

    fn migrate_v19_to_v20(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let api_keys_exists = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'api_keys'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !api_keys_exists {
            return Ok(());
        }
        transaction.execute_batch(SCHEMA_V20).map_err(CoreError::migration)
    }

    fn scale_credit_column_to_microcredits(
        transaction: &rusqlite::Transaction<'_>,
        table: &str,
        id_column: &str,
        value_column: &str,
        nullable: bool,
    ) -> Result<(), CoreError> {
        let table_exists = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            [table],
            |row| row.get::<_, bool>(0),
        )?;
        if !table_exists {
            return Ok(());
        }

        let rows = {
            let mut statement = transaction.prepare(&format!(
                "SELECT {id_column}, {value_column} FROM {table} \
                 WHERE resource_kind = 'credits' ORDER BY {id_column}"
            ))?;
            let rows = statement
                .query_map([], |row| {
                    let (value_type, integer_value) = match row.get_ref(1)? {
                        rusqlite::types::ValueRef::Null => ("null", None),
                        rusqlite::types::ValueRef::Integer(value) => ("integer", Some(value)),
                        rusqlite::types::ValueRef::Real(_) => ("real", None),
                        rusqlite::types::ValueRef::Text(_) => ("text", None),
                        rusqlite::types::ValueRef::Blob(_) => ("blob", None),
                    };
                    Ok((
                        row.get::<_, String>(0)?,
                        value_type,
                        integer_value,
                    ))
                })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        for (row_id, value_type, integer_value) in rows {
            let amount = match (value_type, integer_value) {
                ("null", _) if nullable => continue,
                ("integer", Some(value)) => value,
                _ => {
                    return Err(CoreError::MigrationValidation {
                        reason: format!(
                            "cannot convert {table}.{value_column} for row {row_id}: expected an integer, found {value_type}"
                        ),
                    });
                }
            };
            let scaled = amount.checked_mul(1_000_000).ok_or_else(|| {
                CoreError::MigrationValidation {
                    reason: format!(
                        "cannot convert {table}.{value_column} for row {row_id}: microcredit scaling overflows"
                    ),
                }
            })?;
            transaction.execute(
                &format!("UPDATE {table} SET {value_column} = ?1 WHERE {id_column} = ?2"),
                rusqlite::params![scaled, row_id],
            )?;
        }

        Ok(())
    }

    fn migrate_v5_to_v6(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let observations_table_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'upstream_observations')",
            [],
            |row| row.get(0),
        ).map_err(CoreError::migration)?;
        if !observations_table_exists {
            transaction.execute_batch(
                "CREATE TABLE upstream_observations (
                   id TEXT PRIMARY KEY,
                   account_ref TEXT NOT NULL,
                   resource_kind TEXT NOT NULL,
                   observed_value INTEGER,
                   source TEXT NOT NULL,
                   observed_at_ms INTEGER NOT NULL,
                   stale_at_ms INTEGER,
                   summary_json TEXT NOT NULL
                 );",
            ).map_err(CoreError::migration)?;
        }
        transaction.execute_batch(SCHEMA_V6).map_err(CoreError::migration)?;
        let legacy_accounts = {
            let mut statement = transaction
                .prepare("SELECT account_ref, MIN(observed_at_ms) FROM upstream_observations GROUP BY account_ref")
                .map_err(CoreError::migration)?;
            let accounts = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
                .map_err(CoreError::migration)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CoreError::migration)?;
            accounts
        };
        for (account_ref, observed_at_ms) in legacy_accounts {
            transaction.execute(
                "INSERT INTO upstream_accounts
                 (id, provider, credentials_ref, region, capabilities_json, enabled,
                  max_concurrency, state, cooldown_until_ms, cooldown_reason,
                  consecutive_errors, created_at_ms, updated_at_ms)
                 VALUES (?1, 'legacy', ?2, NULL, '[]', 0, 1, 'disabled', NULL,
                         'legacy observation requires reconciliation', 0, ?3, ?3)
                 ON CONFLICT(id) DO NOTHING",
                params![&account_ref, format!("legacy://{account_ref}"), observed_at_ms],
            ).map_err(CoreError::migration)?;
        }
        transaction.execute(
            "INSERT INTO upstream_observations_next
             (id, account_ref, resource_kind, observed_value, value_scale, source, status,
              observed_at_ms, stale_at_ms, summary_json)
             SELECT id, account_ref, resource_kind, observed_value, 1, source, 'stale',
                    observed_at_ms, COALESCE(stale_at_ms, observed_at_ms), summary_json
             FROM upstream_observations",
            [],
        ).map_err(CoreError::migration)?;
        transaction.execute_batch(
            "DROP TABLE upstream_observations;
             ALTER TABLE upstream_observations_next RENAME TO upstream_observations;",
        ).map_err(CoreError::migration)?;
        transaction
            .execute_batch(SCHEMA_V6_FINISH)
            .map_err(CoreError::migration)?;

        // A few pre-v6 databases carried an intentionally inert requests
        // table containing only an id. It has no data that can be mapped into
        // the Core state machine. Quarantine that shape during the known
        // v5->v6 path, but refuse to do the same for a database that already
        // claims to be v6 (migrate_v6_to_v7 below fails closed there).
        let requests_exists: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'requests')",
                [],
                |row| row.get(0),
            )
            .map_err(CoreError::migration)?;
        let requests_have_state = requests_exists && {
            let mut statement = transaction
                .prepare("PRAGMA table_info(requests)")
                .map_err(CoreError::migration)?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(CoreError::migration)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CoreError::migration)?;
            columns.iter().any(|column| column == "state")
        };
        if requests_exists && !requests_have_state {
            let request_count: i64 = transaction
                .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
                .map_err(CoreError::migration)?;
            if request_count != 0 {
                return Err(CoreError::MigrationValidation {
                    reason: "legacy inert requests table contains rows that cannot be imported".into(),
                });
            }
            transaction
                .execute_batch(
                    "DROP TABLE requests;
                     CREATE TABLE requests (
                       id TEXT PRIMARY KEY,
                       user_id TEXT NOT NULL,
                       api_key_id TEXT NOT NULL,
                       protocol TEXT NOT NULL,
                       endpoint TEXT NOT NULL,
                       model TEXT NOT NULL,
                       request_hash BLOB NOT NULL,
                       state TEXT NOT NULL,
                       result_status INTEGER,
                       error_code TEXT,
                       created_at_ms INTEGER NOT NULL,
                       updated_at_ms INTEGER NOT NULL
                     );",
                )
                .map_err(CoreError::migration)?;
        } else if !requests_exists {
            transaction
                .execute_batch(
                    "CREATE TABLE requests (
                       id TEXT PRIMARY KEY,
                       user_id TEXT NOT NULL,
                       api_key_id TEXT NOT NULL,
                       protocol TEXT NOT NULL,
                       endpoint TEXT NOT NULL,
                       model TEXT NOT NULL,
                       request_hash BLOB NOT NULL,
                       state TEXT NOT NULL,
                       result_status INTEGER,
                       error_code TEXT,
                       created_at_ms INTEGER NOT NULL,
                       updated_at_ms INTEGER NOT NULL
                     );",
                )
                .map_err(CoreError::migration)?;
        }
        Self::harden_v6_records(transaction)
    }

    fn migrate_v6_to_v7(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let requests_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'requests')",
            [],
            |row| row.get(0),
        ).map_err(CoreError::migration)?;
        if !requests_exists {
            return Ok(());
        }

        let requests_have_state = {
            let mut statement = transaction
                .prepare("PRAGMA table_info(requests)")
                .map_err(CoreError::migration)?;
            let columns = statement
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(CoreError::migration)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CoreError::migration)?;
            columns.iter().any(|column| column == "state")
        };
        // A requests table without the v7 state machine is not an inert table
        // that can safely be marked as migrated: the rest of Core would read
        // state/result columns and idempotency rows from it. Fail the
        // transaction instead of silently recording schema version 7 over a
        // partial database.
        if !requests_have_state {
            return Err(CoreError::MigrationValidation {
                reason: "requests table is missing the v7 state column".into(),
            });
        }

        let idempotency_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'idempotency_keys')",
            [],
            |row| row.get(0),
        ).map_err(CoreError::migration)?;
        let request_indexes = {
            let mut statement = transaction
                .prepare(
                    "SELECT sql FROM sqlite_master \
                     WHERE type = 'index' AND tbl_name IN ('requests', 'idempotency_keys') AND sql IS NOT NULL",
                )
                .map_err(CoreError::migration)?;
            let indexes = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(CoreError::migration)?
                .collect::<Result<Vec<_>, _>>()
                .map_err(CoreError::migration)?;
            indexes
        };

        if !idempotency_exists {
            // Older fixtures may have request rows but no idempotency table.
            // Create an empty legacy-shaped table so the canonical v7 rebuild
            // still creates the authoritative idempotency table.
            transaction
                .execute_batch(
                    "CREATE TABLE idempotency_keys (\
                       scope TEXT NOT NULL,\
                       client_key TEXT NOT NULL,\
                       request_hash BLOB NOT NULL,\
                       request_id TEXT NOT NULL REFERENCES requests(id),\
                       created_at_ms INTEGER NOT NULL,\
                       PRIMARY KEY(scope, client_key)\
                     );",
                )
                .map_err(CoreError::migration)?;
        }
        transaction.execute_batch(SCHEMA_V7).map_err(CoreError::migration)?;
        for index_sql in request_indexes {
            transaction.execute_batch(&index_sql).map_err(CoreError::migration)?;
        }
        Ok(())
    }

    fn harden_v6_records(transaction: &rusqlite::Transaction<'_>) -> Result<(), CoreError> {
        let accounts = {
            let mut statement = transaction.prepare(
                "SELECT id, credentials_ref FROM upstream_accounts",
            )?;
            let records = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            records
        };
        let now = Utc::now().timestamp_millis();
        for (id, credentials_ref) in accounts {
            if validate_opaque_credentials_ref(&credentials_ref).is_err() {
                let redacted_ref = format!(
                    "opaque://redacted/{}",
                    URL_SAFE_NO_PAD.encode(Self::digest_api_key(&id))
                );
                transaction.execute(
                    "UPDATE upstream_accounts
                     SET credentials_ref = ?1, enabled = 0, state = 'disabled',
                         cooldown_reason = 'credentials_ref_redacted', updated_at_ms = ?2
                     WHERE id = ?3",
                    params![redacted_ref, now, id],
                )?;
            }
        }

        let summaries = {
            let mut statement = transaction.prepare(
                "SELECT id, summary_json FROM upstream_observations",
            )?;
            let records = statement
                .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?
                .collect::<Result<Vec<_>, _>>()?;
            records
        };
        for (id, summary_json) in summaries {
            let is_safe = serde_json::from_str::<serde_json::Value>(&summary_json)
                .ok()
                .and_then(|summary| validate_observation_summary(&summary).ok())
                .is_some();
            if !is_safe {
                transaction.execute(
                    "UPDATE upstream_observations SET summary_json = '{}' WHERE id = ?1",
                    [id],
                )?;
            }
        }
        transaction.execute(
            "UPDATE upstream_observations AS failed
             SET observed_value = (
               SELECT fresh.observed_value
               FROM upstream_observations AS fresh
               WHERE fresh.account_ref = failed.account_ref
                 AND fresh.resource_kind = failed.resource_kind
                 AND fresh.status = 'fresh'
                 AND fresh.observed_value IS NOT NULL
                 AND fresh.observed_at_ms <= failed.observed_at_ms
               ORDER BY fresh.observed_at_ms DESC, fresh.id DESC
               LIMIT 1
             )
             WHERE failed.status = 'failed'",
            [],
        )?;
        Ok(())
    }
}

fn validate_video_billing_control_input(
    input: &VideoBillingControlInput,
) -> Result<(), CoreError> {
    if input.reason.trim().is_empty()
        || input.reason.len() > 256
        || input.reason.chars().any(char::is_control)
    {
        return Err(CoreError::Validation {
            field: "video_billing.reason".into(),
            reason: "must be a non-empty reason without control characters".into(),
        });
    }
    match input.mode {
        VideoBillingMode::DiagnosticOnce => {
            let key_id = input.diagnostic_key_id.as_deref().unwrap_or("");
            let request_hash = input.diagnostic_request_hash.as_deref().unwrap_or("");
            validate_video_diagnostic_values(key_id, request_hash)?;
        }
        VideoBillingMode::Paused | VideoBillingMode::Active => {
            if input.diagnostic_key_id.is_some() || input.diagnostic_request_hash.is_some() {
                return Err(CoreError::Validation {
                    field: "video_billing.diagnostic".into(),
                    reason: "diagnostic fields require diagnostic_once mode".into(),
                });
            }
        }
    }
    Ok(())
}

fn validate_video_diagnostic_values(key_id: &str, request_hash: &str) -> Result<(), CoreError> {
    if key_id.trim().is_empty()
        || key_id.len() > 160
        || key_id.chars().any(char::is_control)
        || request_hash.len() != 64
        || !request_hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(CoreError::Validation {
            field: "video_billing.diagnostic".into(),
            reason: "key id and request hash are invalid".into(),
        });
    }
    Ok(())
}

fn validate_asset_identifier(value: &str) -> Result<(), CoreError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 160
        || !value
            .bytes()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, b'-' | b'_'))
    {
        return Err(CoreError::Validation {
            field: "asset_id".into(),
            reason: "must be a safe opaque identifier".into(),
        });
    }
    Ok(())
}

fn validate_asset_input(input: &CreateAssetInput) -> Result<(), CoreError> {
    validate_asset_identifier(&input.id)?;
    if input.filename.trim().is_empty()
        || input.filename.len() > 128
        || input.filename.chars().any(char::is_control)
    {
        return Err(CoreError::Validation {
            field: "filename".into(),
            reason: "must be a short non-control filename".into(),
        });
    }
    if input.mime_type.trim().is_empty()
        || input.mime_type.len() > 128
        || input.mime_type.chars().any(char::is_whitespace)
    {
        return Err(CoreError::Validation {
            field: "mime_type".into(),
            reason: "must be a compact MIME type".into(),
        });
    }
    if input.extension.is_empty()
        || input.extension.len() > 8
        || !input.extension.bytes().all(|character| character.is_ascii_alphanumeric())
    {
        return Err(CoreError::Validation {
            field: "extension".into(),
            reason: "must be an alphanumeric extension".into(),
        });
    }
    if input.size <= 0 {
        return Err(CoreError::Validation {
            field: "size".into(),
            reason: "must be positive".into(),
        });
    }
    if input.sha256.len() != 64 || !input.sha256.bytes().all(|character| character.is_ascii_hexdigit()) {
        return Err(CoreError::Validation {
            field: "sha256".into(),
            reason: "must be a SHA-256 hex digest".into(),
        });
    }
    let storage_name = input.storage_ref.strip_prefix("assets/").unwrap_or("");
    if storage_name.is_empty()
        || storage_name.contains('/')
        || storage_name.contains('\\')
        || storage_name.contains("..")
        || storage_name.chars().any(char::is_whitespace)
    {
        return Err(CoreError::Validation {
            field: "storage_ref".into(),
            reason: "must name one safe file below assets/".into(),
        });
    }
    if input.content_token_digest.len() != 32 {
        return Err(CoreError::Validation {
            field: "content_token_digest".into(),
            reason: "must be exactly 32 bytes".into(),
        });
    }
    if input.created_at_ms < 0 || input.expires_at_ms <= input.created_at_ms {
        return Err(CoreError::Validation {
            field: "expires_at_ms".into(),
            reason: "must be after created_at_ms".into(),
        });
    }
    Ok(())
}

impl CoreError {
    fn migration(source: rusqlite::Error) -> Self {
        Self::Migration { source }
    }
}
