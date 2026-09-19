use std::{collections::BTreeSet, fs, path::Path, sync::Mutex, time::Duration};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::{
    schema::{SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V6_FINISH},
    upstream::{validate_observation_summary, validate_opaque_credentials_ref, validate_required},
    AuthError, CoreError, IssuedApiKey, LegacyMigrationBatch, LegacyMigrationResult, LeaseState,
    NewUser, ObservationStatus, Principal, RegisterUpstreamAccount, UpstreamAccount,
    UpstreamLease, UpstreamObservation, User,
};

pub const CORE_DB_FILE: &str = "core.sqlite3";
pub const CURRENT_SCHEMA_VERSION: u32 = 6;

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
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            5 => {
                Self::migrate_v5_to_v6(&transaction)?;
                transaction
                    .execute(
                        "UPDATE schema_meta SET value = ?1 WHERE key = 'schema_version'",
                        params![CURRENT_SCHEMA_VERSION.to_string()],
                    )
                    .map_err(CoreError::migration)?;
            }
            CURRENT_SCHEMA_VERSION => Self::harden_v6_records(&transaction)?,
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

    pub fn table_exists(&self, table_name: &str) -> Result<bool, CoreError> {
        Ok(self.table_count(table_name)? == 1)
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
            &input.id,
            serde_json::json!({"account_ref": input.id, "provider": input.provider, "result": "upserted"}),
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
        let credentials_ref = transaction
            .query_row(
                "SELECT credentials_ref FROM upstream_accounts WHERE id = ?1",
                [&observation.account_ref],
                |row| row.get::<_, String>(0),
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
            observation.observed_value = transaction
                .query_row(
                    "SELECT observed_value FROM upstream_observations
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
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
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
        self.issue_api_key_inner(user_id, name, scopes, actor, false)
    }

    pub fn issue_api_key_as_admin(
        &self,
        user_id: &str,
        name: &str,
        scopes: BTreeSet<String>,
        principal: &Principal,
    ) -> Result<IssuedApiKey, CoreError> {
        let plaintext = Self::new_api_key();
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let key_id = Self::new_id("key");
        let scopes_json = serde_json::to_string(&scopes)?;
        let now = Utc::now().timestamp_millis();

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, user_id)?;
        transaction.execute(
            "INSERT INTO api_keys \
             (id, user_id, name, prefix, key_digest, scopes_json, status, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
            params![key_id, user_id, name, prefix, key_digest, scopes_json, now],
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
        actor: &str,
        require_admin: bool,
    ) -> Result<IssuedApiKey, CoreError> {
        let plaintext = Self::new_api_key();
        let prefix = plaintext[..16].to_owned();
        let key_digest = Self::digest_api_key(&plaintext);
        let key_id = Self::new_id("key");
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
        self.revoke_api_key_inner(key_id, actor, false)
    }

    pub fn revoke_api_key_as_admin(&self, key_id: &str, actor: &str) -> Result<(), CoreError> {
        self.revoke_api_key_inner(key_id, actor, true)
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
        for key in &batch.keys {
            let plaintext = Self::new_api_key();
            let prefix = plaintext[..16].to_owned();
            let key_digest = Self::digest_api_key(&plaintext);
            let key_id = Self::new_id("key");
            let scopes_json = serde_json::to_string(&batch.scopes)?;
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
                scopes: batch.scopes.clone(),
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
        Self::harden_v6_records(transaction)
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

impl CoreError {
    fn migration(source: rusqlite::Error) -> Self {
        Self::Migration { source }
    }
}
