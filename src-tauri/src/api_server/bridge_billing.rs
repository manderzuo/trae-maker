use std::{fs, path::Path, time::Duration};

use aiwork_core::CreditAmount;
use rusqlite::Transaction;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const BILLING_DB_FILE: &str = "bridge-billing.sqlite3";
const BRIDGE_SCHEMA_META_TABLE: &str = "bridge_schema_meta";
const BRIDGE_SCHEMA_VERSION: i64 = 6;
const BRIDGE_ACTIVE_OWNER_PREFIX: &str = "bridge-active-v1-";
const BRIDGE_RECOVERY_REQUIRED_PREFIX: &str = "bridge-recovery-required-v1-";

/// The caller may identify a request, but can never supply the quote amount.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BridgeQuoteRequest {
    pub request_id: String,
    pub endpoint: String,
    pub model: String,
    pub request_fingerprint: String,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct BridgeQuoteResponse {
    pub request_id: String,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quote_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_credits: Option<String>,
    pub unit: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_ref: Option<String>,
    pub error_code: &'static str,
    pub message: &'static str,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum BillingReceiptStatus {
    Pending,
    Final,
    Unknown,
    Unverified,
    Conflict,
}

impl BillingReceiptStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Final => "final",
            Self::Unknown => "unknown",
            Self::Unverified => "unverified",
            Self::Conflict => "conflict",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "pending" => Ok(Self::Pending),
            "final" => Ok(Self::Final),
            "unknown" => Ok(Self::Unknown),
            "unverified" => Ok(Self::Unverified),
            "conflict" => Ok(Self::Conflict),
            _ => Err("stored bridge billing status is invalid".into()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct BillingReceipt {
    pub request_id: String,
    pub status: BillingReceiptStatus,
    pub actual_credits: Option<CreditAmount>,
    pub unit: Option<String>,
    pub source_ref: Option<String>,
    pub task_ref: Option<String>,
    pub observed_at_ms: i64,
}

/// A deliberately minimal mirror of a Core API Key. Never add credentials,
/// prefixes, user ids, scopes or quota values to this bridge contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CoreKeyMetadata {
    pub id: String,
    pub display_name: String,
    pub active: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CoreKeyRegistrySnapshot {
    pub version: i64,
    pub keys: Vec<CoreKeyMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum RegistryUpdate {
    Applied,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoreRequestRecord {
    Created,
    Duplicate,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoreBillingMode {
    Quoted,
    LegacyOneShot,
    ControlledUnquoted,
}

impl CoreBillingMode {
    fn as_db_value(self) -> &'static str {
        match self {
            Self::Quoted => "quoted",
            Self::LegacyOneShot => "legacy_one_shot",
            Self::ControlledUnquoted => "controlled_unquoted",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoreSessionLinkResult {
    Created,
    Duplicate,
    Conflict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CoreSessionLookup {
    Unique { request_id: String, core_key_id: String },
    Ambiguous,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum CoreBillingSessionLookup {
    Missing,
    Ambiguous,
    Unique {
        core_key_id: String,
        account_ref: String,
        session_id: String,
        associated_at_ms: i64,
    },
}

/// Created by auth middleware only after the bridge credential is verified
/// and the immutable request_id -> Core Key association is persisted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CoreRequestAttribution {
    pub request_id: String,
    pub core_key_id: String,
    pub billing_mode: CoreBillingMode,
    pub operation_id: Option<String>,
}

/// TRAE usage history aggregates credits by session, so Core traffic gets a
/// deterministic session per Core request and upstream account. Keeping this
/// separate from the client's conversation ID prevents adjacent API requests
/// from being merged into one session-level usage row.
pub(crate) fn core_usage_session_id(request_id: &str, account_ref: &str) -> String {
    use sha2::{Digest, Sha256};

    let input = format!("ai-work-core-usage-session-v1\0{request_id}\0{account_ref}");
    let digest = Sha256::digest(input.as_bytes());
    let hex: String = digest[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
    )
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub(super) struct CoreKeyUsage {
    pub key_id: String,
    pub display_name: String,
    pub active: bool,
    /// Sum of request-scoped receipts accepted by the verified source adapter.
    pub verified_credits: String,
    pub pending_requests: u64,
    pub conflict_requests: u64,
}

impl BillingReceipt {
    fn unknown(request_id: String) -> Self {
        Self {
            request_id,
            status: BillingReceiptStatus::Unknown,
            actual_credits: None,
            unit: Some("credits".into()),
            source_ref: None,
            task_ref: None,
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        }
    }
}

/// Trust is assigned only by in-process source adapters; it is never accepted
/// from HTTP input. No current Trae/Tare adapter satisfies the verified contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EvidenceTrust {
    CandidateOnly,
    VerifiedSourceContract,
    AuthorizedCoreVideoSession,
    AuthorizedCoreChatSession,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum PersistReceiptResult {
    Created,
    Duplicate,
    Conflict,
}

pub(super) struct BridgeBillingStore {
    pub(super) connection: Connection,
    pub(super) data_dir: std::path::PathBuf,
}

impl BridgeBillingStore {
    /// This can only be called on a route that is returning before video task
    /// creation. The transaction also refuses zero-cost evidence if an upstream
    /// session was already attributed to this request.
    pub(super) fn record_controlled_pre_dispatch_no_charge(
        &mut self,
        request_id: &str,
    ) -> Result<PersistReceiptResult, String> {
        if !valid_request_id(request_id) { return Err("controlled request id is invalid".into()); }
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("pre-dispatch billing transaction unavailable: {error}"))?;
        let controlled: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_core_requests request
             JOIN bridge_core_request_modes mode ON mode.request_id = request.request_id
             WHERE request.request_id = ?1 AND request.conflict = 0
             AND mode.billing_mode = 'controlled_unquoted' AND mode.operation_id IS NOT NULL)",
            [request_id], |row| row.get(0),
        ).map_err(|error| format!("controlled request lookup failed: {error}"))?;
        let attempted: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_core_upstream_sessions WHERE request_id = ?1)",
            [request_id], |row| row.get(0),
        ).map_err(|error| format!("upstream attempt lookup failed: {error}"))?;
        if !controlled || attempted {
            return Err("request is not an unattempted controlled operation".into());
        }
        let source_ref = format!("aiwork-pre-dispatch-no-charge:{request_id}");
        let existing: Option<(String, Option<i64>, Option<String>)> = transaction.query_row(
            "SELECT status, actual_microcredits, source_ref FROM bridge_billing_receipts WHERE request_id = ?1",
            [request_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).optional().map_err(|error| format!("pre-dispatch receipt lookup failed: {error}"))?;
        if let Some((status, actual, source)) = existing {
            return if status == "final" && actual == Some(0) && source.as_deref() == Some(&source_ref) {
                Ok(PersistReceiptResult::Duplicate)
            } else {
                Err("request already has a different billing receipt".into())
            };
        }
        let receipt = BillingReceipt {
            request_id: request_id.into(), status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("0", "credits")
                .map_err(|error| format!("zero credit amount invalid: {error}"))?),
            unit: Some("credits".into()), source_ref: Some(source_ref), task_ref: None,
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        insert_receipt(&transaction, &receipt, BillingReceiptStatus::Final, Some(0))?;
        transaction.commit().map_err(|error| format!("pre-dispatch receipt commit failed: {error}"))?;
        Ok(PersistReceiptResult::Created)
    }
    pub(super) fn controlled_request_key(&self, request_id: &str) -> Result<Option<String>, String> {
        if !valid_request_id(request_id) { return Ok(None); }
        self.connection.query_row(
            "SELECT request.core_key_id FROM bridge_core_requests request
             JOIN bridge_core_request_modes mode ON mode.request_id = request.request_id
             WHERE request.request_id = ?1 AND request.conflict = 0
             AND mode.billing_mode = 'controlled_unquoted' AND mode.operation_id IS NOT NULL",
            [request_id], |row| row.get(0),
        ).optional().map_err(|error| format!("controlled Core request lookup failed: {error}"))
    }
    pub(super) fn open(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir)
            .map_err(|error| format!("bridge billing data directory unavailable: {error}"))?;
        let path = data_dir.join(BILLING_DB_FILE);
        let mut connection = Connection::open(path)
            .map_err(|error| format!("bridge billing database unavailable: {error}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("bridge billing database busy: {error}"))?;
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .map_err(|error| format!("bridge billing journal mode unavailable: {error}"))?;
        if !journal_mode.eq_ignore_ascii_case("wal") {
            connection
                .execute_batch("PRAGMA journal_mode = WAL;")
                .map_err(|error| format!("bridge billing WAL mode unavailable: {error}"))?;
        }
        connection
            .execute_batch(
                "PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|error| format!("bridge billing database pragmas unavailable: {error}"))?;
        initialize_bridge_schema(&mut connection, |_| Ok(()))?;
        Ok(Self { connection, data_dir:data_dir.to_path_buf() })
    }

    pub(super) fn bridge_identity(&self) -> Result<(String, String), String> {
        let (version, instance_id, generation): (i64, String, String) = self.connection
            .query_row(
                "SELECT schema_version, bridge_instance_id, event_generation FROM bridge_schema_meta WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|error| format!("bridge billing identity unavailable: {error}"))?;
        if version != BRIDGE_SCHEMA_VERSION {
            return Err("bridge billing identity has an unsupported schema version".into());
        }
        validate_bridge_identity(&instance_id, &generation)?;
        Ok((instance_id, generation))
    }

    pub(super) fn mark_recovery_required(
        &self,
        expected_instance_id: &str,
        expected_generation: &str,
    ) -> Result<String, String> {
        let (instance_id, current_generation) = self.bridge_identity()?;
        if instance_id != expected_instance_id {
            return Err("bridge instance changed before recovery marker could be written".into());
        }
        if is_recovery_required_generation(&current_generation) {
            return Ok(current_generation);
        }
        if current_generation != expected_generation {
            return Err("bridge generation changed before recovery marker could be written".into());
        }

        let recovery_generation = format!(
            "{BRIDGE_RECOVERY_REQUIRED_PREFIX}{:032x}",
            rand::random::<u128>()
        );
        let updated = self.connection.execute(
            "UPDATE bridge_schema_meta SET event_generation = ?1
             WHERE singleton = 1 AND schema_version = ?2
               AND bridge_instance_id = ?3 AND event_generation = ?4",
            params![
                recovery_generation,
                BRIDGE_SCHEMA_VERSION,
                expected_instance_id,
                expected_generation,
            ],
        ).map_err(|error| format!("bridge recovery marker write failed: {error}"))?;
        if updated == 1 {
            return Ok(recovery_generation);
        }

        let (instance_id, current_generation) = self.bridge_identity()?;
        if instance_id == expected_instance_id
            && is_recovery_required_generation(&current_generation)
        {
            return Ok(current_generation);
        }
        Err("bridge recovery marker lost its identity compare-and-swap".into())
    }

    pub(super) fn mark_active_owner(
        &self,
        expected_instance_id: &str,
        expected_generation: &str,
    ) -> Result<String, String> {
        let (instance_id, current_generation) = self.bridge_identity()?;
        if instance_id != expected_instance_id
            || current_generation != expected_generation
            || is_dirty_bridge_generation(&current_generation)
        {
            return Err("bridge identity is not clean before active-owner marker write".into());
        }
        let active_generation = new_active_owner_generation();
        let updated = self.connection.execute(
            "UPDATE bridge_schema_meta SET event_generation = ?1
             WHERE singleton = 1 AND schema_version = ?2
               AND bridge_instance_id = ?3 AND event_generation = ?4",
            params![
                active_generation,
                BRIDGE_SCHEMA_VERSION,
                expected_instance_id,
                expected_generation,
            ],
        ).map_err(|error| format!("bridge active-owner marker write failed: {error}"))?;
        if updated != 1 {
            return Err("bridge active-owner marker lost its identity compare-and-swap".into());
        }
        Ok(active_generation)
    }

    pub(super) fn finish_activity_lease_cleanly<F>(
        &self,
        lease: &mut super::bridge_budget_lease::BridgeBudgetLease,
        confirm_workers_stopped: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let (instance_id, generation) = self.bridge_identity()?;
        if !lease.is_active()
            || lease.is_closed()
            || lease.recovery_required()
            || lease.instance_id() != instance_id
            || lease.generation() != generation
            || !is_active_owner_generation(&generation)
        {
            return Err("clean close requires this active lease's persisted owner marker".into());
        }
        lease.begin_clean_close();
        confirm_workers_stopped()?;

        let idle_generation = format!("bridge-generation-v1-{:032x}", rand::random::<u128>());
        let updated = self.connection.execute(
            "UPDATE bridge_schema_meta SET event_generation = ?1
             WHERE singleton = 1 AND schema_version = ?2
               AND bridge_instance_id = ?3 AND event_generation = ?4",
            params![idle_generation, BRIDGE_SCHEMA_VERSION, instance_id, generation],
        ).map_err(|error| format!("bridge clean-close marker update failed: {error}"))?;
        if updated != 1 {
            return Err("bridge clean close lost its identity compare-and-swap".into());
        }
        lease.record_cleanly_closed_generation(idle_generation.clone());
        Ok(idle_generation)
    }

    pub(super) fn rotate_event_generation_for_recovery<F>(
        &mut self,
        lease: &mut super::bridge_budget_lease::BridgeBudgetLease,
        confirm_workers_stopped: F,
    ) -> Result<String, String>
    where
        F: FnOnce() -> Result<(), String>,
    {
        let (instance_id, generation) = self.bridge_identity()?;
        if !lease.is_active()
            || lease.charge_ready()
            || !lease.recovery_required()
            || !is_recovery_required_generation(&generation)
            || lease.instance_id() != instance_id
            || lease.generation() != generation
        {
            return Err("recovery requires an active lease with a persisted recovery marker".into());
        }
        // The caller must have stopped and joined the old charging workers.
        // This internal confirmation is explicit; 3b must supply the real join.
        confirm_workers_stopped()?;
        let transaction = self.connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("bridge generation recovery transaction unavailable: {error}"))?;
        let (version, current_instance, current_generation) = read_bridge_schema_metadata(&transaction)?;
        if version != BRIDGE_SCHEMA_VERSION
            || current_instance != instance_id
            || current_generation != generation
        {
            return Err("bridge generation changed before recovery could commit".into());
        }
        let next_generation = new_active_owner_generation();
        let updated = transaction.execute(
            "UPDATE bridge_schema_meta SET event_generation = ?1
             WHERE singleton = 1 AND schema_version = ?2
               AND bridge_instance_id = ?3 AND event_generation = ?4",
            params![next_generation, BRIDGE_SCHEMA_VERSION, instance_id, generation],
        ).map_err(|error| format!("bridge generation recovery update failed: {error}"))?;
        if updated != 1 {
            return Err("bridge generation recovery lost its identity CAS".into());
        }
        transaction.commit()
            .map_err(|error| format!("bridge generation recovery commit failed: {error}"))?;
        lease.record_recovered_generation(next_generation.clone());
        Ok(next_generation)
    }

    pub(super) fn replace_core_key_registry(
        &mut self,
        snapshot: &CoreKeyRegistrySnapshot,
    ) -> Result<RegistryUpdate, String> {
        if snapshot.version <= 0 || snapshot.keys.len() > 10_000 {
            return Err("Core Key registry snapshot is outside the allowed bounds".into());
        }
        let mut keys = snapshot.keys.clone();
        keys.sort_by(|left, right| left.id.cmp(&right.id));
        let mut seen = std::collections::HashSet::with_capacity(keys.len());
        for key in &keys {
            if !valid_core_key_id(&key.id)
                || key.display_name.trim().is_empty()
                || key.display_name.chars().count() > 128
                || key.display_name.chars().any(char::is_control)
                || !seen.insert(key.id.as_str())
            {
                return Err("Core Key registry snapshot contains invalid or duplicate metadata".into());
            }
        }

        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("Core Key registry transaction unavailable: {error}"))?;
        let current_version = transaction.query_row(
            "SELECT version FROM bridge_core_key_registry_state WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        ).optional().map_err(|error| format!("Core Key registry version lookup failed: {error}"))?;
        if current_version.is_some_and(|version| snapshot.version < version) {
            return Err("stale Core Key registry snapshot rejected".into());
        }
        if current_version == Some(snapshot.version) {
            let existing = read_registry(&transaction)?;
            if existing == keys {
                return Ok(RegistryUpdate::Unchanged);
            }
            return Err("Core Key registry version was reused with different content".into());
        }

        transaction.execute(
            "UPDATE bridge_core_api_keys SET active = 0, snapshot_version = ?1
             WHERE snapshot_version > 0",
            params![snapshot.version],
        ).map_err(|error| format!("Core Key registry deactivation failed: {error}"))?;
        for key in &keys {
            transaction.execute(
                "INSERT INTO bridge_core_api_keys(key_id, display_name, active, snapshot_version)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key_id) DO UPDATE SET
                   display_name = excluded.display_name,
                   active = excluded.active,
                   snapshot_version = excluded.snapshot_version",
                params![key.id, key.display_name, key.active, snapshot.version],
            ).map_err(|error| format!("Core Key registry update failed: {error}"))?;
        }
        transaction.execute(
            "INSERT INTO bridge_core_key_registry_state(singleton, version, updated_at_ms)
             VALUES (1, ?1, ?2)
             ON CONFLICT(singleton) DO UPDATE SET version = excluded.version, updated_at_ms = excluded.updated_at_ms",
            params![snapshot.version, chrono::Utc::now().timestamp_millis()],
        ).map_err(|error| format!("Core Key registry version update failed: {error}"))?;
        transaction.commit().map_err(|error| format!("Core Key registry commit failed: {error}"))?;
        Ok(RegistryUpdate::Applied)
    }

    pub(super) fn record_core_request(
        &mut self,
        request_id: &str,
        core_key_id: &str,
    ) -> Result<CoreRequestRecord, String> {
        self.record_core_request_with_mode(request_id, core_key_id, false)
    }

    pub(super) fn record_core_request_with_mode(
        &mut self,
        request_id: &str,
        core_key_id: &str,
        one_shot_test: bool,
    ) -> Result<CoreRequestRecord, String> {
        let mode = if one_shot_test { CoreBillingMode::LegacyOneShot } else { CoreBillingMode::Quoted };
        self.record_core_request_with_billing_mode(request_id, core_key_id, mode, None)
    }

    pub(super) fn record_core_request_with_billing_mode(
        &mut self,
        request_id: &str,
        core_key_id: &str,
        mode: CoreBillingMode,
        operation_id: Option<&str>,
    ) -> Result<CoreRequestRecord, String> {
        if !valid_request_id(request_id) || !valid_core_key_id(core_key_id) {
            return Err("Core request attribution identifiers are invalid".into());
        }
        if (mode == CoreBillingMode::ControlledUnquoted && !operation_id.is_some_and(valid_request_id))
            || (mode != CoreBillingMode::ControlledUnquoted && operation_id.is_some())
        {
            return Err("Core controlled operation identifier is invalid".into());
        }
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("Core request attribution transaction unavailable: {error}"))?;
        transaction.execute(
            "INSERT OR IGNORE INTO bridge_core_api_keys(key_id, display_name, active, snapshot_version)
             VALUES (?1, '等待 Core 同步', 0, 0)",
            params![core_key_id],
        ).map_err(|error| format!("Core request key placeholder insert failed: {error}"))?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO bridge_core_requests(request_id, core_key_id, associated_at_ms)
             VALUES (?1, ?2, ?3)",
            params![request_id, core_key_id, chrono::Utc::now().timestamp_millis()],
        ).map_err(|error| format!("Core request attribution insert failed: {error}"))?;
        let result = if inserted == 1 {
            if mode == CoreBillingMode::LegacyOneShot {
                transaction.execute(
                    "INSERT INTO bridge_core_one_shot_test_requests(request_id, authorized_at_ms)
                     VALUES (?1, ?2)",
                    params![request_id, chrono::Utc::now().timestamp_millis()],
                ).map_err(|error| format!("Core one-shot request authorization insert failed: {error}"))?;
            }
            transaction.execute(
                "INSERT INTO bridge_core_request_modes(request_id, billing_mode, operation_id) VALUES (?1, ?2, ?3)",
                params![request_id, mode.as_db_value(), operation_id],
            ).map_err(|error| format!("Core request billing mode insert failed: {error}"))?;
            CoreRequestRecord::Created
        } else {
            let (existing_key, conflicted) = transaction.query_row(
                "SELECT core_key_id, conflict FROM bridge_core_requests WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
            ).map_err(|error| format!("Core request attribution lookup failed: {error}"))?;
            let stored_mode: Option<(String, Option<String>)> = transaction.query_row(
                "SELECT billing_mode, operation_id FROM bridge_core_request_modes WHERE request_id = ?1",
                params![request_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(|error| format!("Core request billing mode lookup failed: {error}"))?;
            let (existing_mode, existing_operation) = match stored_mode {
                Some(values) => values,
                None => {
                    let legacy_one_shot: bool = transaction.query_row(
                        "SELECT EXISTS(SELECT 1 FROM bridge_core_one_shot_test_requests WHERE request_id = ?1)",
                        params![request_id],
                        |row| row.get(0),
                    ).map_err(|error| format!("Core one-shot request authorization lookup failed: {error}"))?;
                    (if legacy_one_shot { "legacy_one_shot" } else { "quoted" }.into(), None)
                }
            };
            if !conflicted && existing_key == core_key_id && existing_mode == mode.as_db_value()
                && existing_operation.as_deref() == operation_id
            {
                CoreRequestRecord::Duplicate
            } else {
                CoreRequestRecord::Conflict
            }
        };
        if result == CoreRequestRecord::Conflict {
            transaction.execute(
                "UPDATE bridge_core_requests SET conflict = 1 WHERE request_id = ?1",
                params![request_id],
            ).map_err(|error| format!("Core request attribution conflict mark failed: {error}"))?;
        }
        transaction.commit().map_err(|error| format!("Core request attribution commit failed: {error}"))?;
        Ok(result)
    }

    pub(super) fn core_session_for_request(
        &self,
        request_id: &str,
    ) -> Result<CoreBillingSessionLookup, String> {
        if !valid_request_id(request_id) {
            return Err("Core request attribution identifier is invalid".into());
        }
        let request = self.connection.query_row(
            "SELECT core_key_id, conflict FROM bridge_core_requests WHERE request_id = ?1",
            params![request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        ).optional().map_err(|error| format!("Core request lookup failed: {error}"))?;
        let core_key_id = match request {
            Some((core_key_id, false)) => core_key_id,
            Some((_, true)) => return Ok(CoreBillingSessionLookup::Ambiguous),
            None => return Ok(CoreBillingSessionLookup::Missing),
        };
        let mut statement = self.connection.prepare(
            "SELECT account_ref, session_id, conflict, associated_at_ms
             FROM bridge_core_upstream_sessions WHERE request_id = ?1 ORDER BY account_ref, session_id",
        ).map_err(|error| format!("Core request session query unavailable: {error}"))?;
        let rows = statement.query_map(params![request_id], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, bool>(2)?, row.get::<_, i64>(3)?,
        ))).map_err(|error| format!("Core request session query failed: {error}"))?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row.map_err(|error| format!("Core request session row invalid: {error}"))?);
        }
        if sessions.is_empty() {
            return Ok(CoreBillingSessionLookup::Missing);
        }
        if sessions.len() != 1 || sessions[0].2 {
            return Ok(CoreBillingSessionLookup::Ambiguous);
        }
        let (account_ref, session_id, _, associated_at_ms) = sessions.pop().unwrap();
        Ok(CoreBillingSessionLookup::Unique {
            core_key_id, account_ref, session_id, associated_at_ms,
        })
    }

    pub(super) fn record_core_session_receipt(
        &mut self,
        receipt: &BillingReceipt,
        account_ref: &str,
        session_id: &str,
        core_key_id: &str,
    ) -> Result<PersistReceiptResult, String> {
        let expected_source = format!("trae-usage-session:{session_id}");
        if receipt.source_ref.as_deref() != Some(expected_source.as_str()) {
            return Err("Core billing receipt source does not match its upstream session".into());
        }
        match self.core_session_for_request(&receipt.request_id)? {
            CoreBillingSessionLookup::Unique {
                core_key_id: stored_key,
                account_ref: stored_account,
                session_id: stored_session,
                ..
            } if stored_key == core_key_id
                && stored_account == account_ref
                && stored_session == session_id => {}
            _ => return Err("Core billing receipt is not tied to one unique request session".into()),
        }
        let trust = if receipt.task_ref.is_some() {
            EvidenceTrust::AuthorizedCoreVideoSession
        } else {
            EvidenceTrust::AuthorizedCoreChatSession
        };
        self.record_receipt(receipt, trust)
    }

    /// Bind one actual upstream attempt to its Core request. Reusing the same
    /// account/session pair for another Core request permanently blocks matching.
    pub(super) fn record_core_session_attempt(
        &mut self,
        request_id: &str,
        account_ref: &str,
        session_id: &str,
    ) -> Result<CoreSessionLinkResult, String> {
        if !valid_request_id(request_id)
            || !valid_internal_account_ref(account_ref)
            || !valid_upstream_session_id(session_id)
        {
            return Err("upstream session attribution identifiers are invalid".into());
        }
        let transaction = self.connection.transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("upstream session transaction unavailable: {error}"))?;
        let request_conflict = transaction.query_row(
            "SELECT conflict FROM bridge_core_requests WHERE request_id = ?1",
            params![request_id],
            |row| row.get::<_, bool>(0),
        ).optional().map_err(|error| format!("upstream session request lookup failed: {error}"))?;
        match request_conflict {
            None => return Err("upstream session has no Core request attribution".into()),
            Some(true) => return Err("upstream session Core request attribution is conflicted".into()),
            Some(false) => {}
        }

        let settled: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_billing_receipts
             WHERE request_id = ?1 AND status IN ('final', 'conflict'))",
            params![request_id],
            |row| row.get(0),
        ).map_err(|error| format!("upstream session receipt lookup failed: {error}"))?;
        if settled {
            return Err("Core request already has a terminal billing receipt".into());
        }

        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO bridge_core_upstream_sessions
             (request_id, account_ref, session_id, associated_at_ms)
             VALUES (?1, ?2, ?3, ?4)",
            params![request_id, account_ref, session_id, chrono::Utc::now().timestamp_millis()],
        ).map_err(|error| format!("upstream session attribution insert failed: {error}"))?;
        let distinct_requests: i64 = transaction.query_row(
            "SELECT COUNT(DISTINCT request_id) FROM bridge_core_upstream_sessions
             WHERE account_ref = ?1 AND session_id = ?2",
            params![account_ref, session_id],
            |row| row.get(0),
        ).map_err(|error| format!("upstream session uniqueness lookup failed: {error}"))?;
        let already_conflicted: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_core_upstream_sessions
             WHERE account_ref = ?1 AND session_id = ?2 AND conflict = 1)",
            params![account_ref, session_id],
            |row| row.get(0),
        ).map_err(|error| format!("upstream session conflict lookup failed: {error}"))?;

        let result = if distinct_requests > 1 {
            transaction.execute(
                "UPDATE bridge_core_upstream_sessions SET conflict = 1
                 WHERE account_ref = ?1 AND session_id = ?2",
                params![account_ref, session_id],
            ).map_err(|error| format!("upstream session conflict update failed: {error}"))?;
            CoreSessionLinkResult::Conflict
        } else if already_conflicted {
            CoreSessionLinkResult::Conflict
        } else if inserted == 1 {
            CoreSessionLinkResult::Created
        } else {
            CoreSessionLinkResult::Duplicate
        };
        transaction.commit().map_err(|error| format!("upstream session transaction commit failed: {error}"))?;
        Ok(result)
    }

    pub(super) fn record_core_upstream_attempt(
        data_dir: &Path,
        attribution: Option<&CoreRequestAttribution>,
        account_ref: &str,
        session_id: &str,
    ) -> Result<Option<CoreSessionLinkResult>, String> {
        let Some(attribution) = attribution else {
            return Ok(None);
        };
        let mut store = Self::open(data_dir)?;
        store
            .record_core_session_attempt(&attribution.request_id, account_ref, session_id)
            .map(Some)
    }

    pub(super) fn record_core_upstream_attempt_from_payload(
        data_dir: &Path,
        attribution: Option<&CoreRequestAttribution>,
        account_ref: &str,
        payload: &serde_json::Value,
    ) -> Result<Option<CoreSessionLinkResult>, String> {
        let Some(attribution) = attribution else {
            return Ok(None);
        };
        let session_id = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| "upstream attempt has no stable session_id".to_string())?;
        Self::record_core_upstream_attempt(data_dir, Some(attribution), account_ref, session_id)
    }

    pub(super) fn lookup_core_session_attribution(
        &self,
        account_ref: &str,
        session_id: &str,
    ) -> Result<Option<CoreSessionLookup>, String> {
        if !valid_internal_account_ref(account_ref) || !valid_upstream_session_id(session_id) {
            return Err("upstream session lookup identifiers are invalid".into());
        }
        let mut statement = self.connection.prepare(
            "SELECT s.request_id, r.core_key_id, s.conflict, r.conflict
             FROM bridge_core_upstream_sessions s
             JOIN bridge_core_requests r ON r.request_id = s.request_id
             WHERE s.account_ref = ?1 AND s.session_id = ?2
             ORDER BY s.request_id",
        ).map_err(|error| format!("upstream session attribution query unavailable: {error}"))?;
        let rows = statement.query_map(params![account_ref, session_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, bool>(3)?,
            ))
        }).map_err(|error| format!("upstream session attribution query failed: {error}"))?;
        let mut matches = Vec::new();
        for row in rows {
            matches.push(row.map_err(|error| format!("upstream session attribution row invalid: {error}"))?);
        }
        match self.budget_session_attribution(account_ref,session_id)? {
            Some(CoreSessionLookup::Unique {request_id,core_key_id})=>matches.push((request_id,core_key_id,false,false)),
            Some(CoreSessionLookup::Ambiguous)=>return Ok(Some(CoreSessionLookup::Ambiguous)),
            None=>{},
        }
        if matches.is_empty() {
            return Ok(None);
        }
        if matches.len() != 1 || matches[0].2 || matches[0].3 {
            return Ok(Some(CoreSessionLookup::Ambiguous));
        }
        let (request_id, core_key_id, _, _) = matches.pop().expect("one session mapping");
        Ok(Some(CoreSessionLookup::Unique { request_id, core_key_id }))
    }

    /// Return only account references with a non-conflicted upstream attempt
    /// for a request that has not received a verified final receipt. Callers
    /// use this allowlist to avoid polling unrelated accounts.
    pub(super) fn pending_core_session_accounts(&self) -> Result<Vec<(String, i64)>, String> {
        let mut statement = self.connection.prepare(
            "SELECT account_ref, MIN(associated_at_ms) FROM (
             SELECT s.account_ref, s.associated_at_ms
             FROM bridge_core_upstream_sessions s
             JOIN bridge_core_requests r ON r.request_id = s.request_id
             LEFT JOIN bridge_billing_receipts b ON b.request_id = r.request_id
             WHERE s.conflict = 0 AND r.conflict = 0
               AND (b.request_id IS NULL OR b.status IN ('pending','unknown','unverified'))
             UNION ALL
             SELECT e.account_ref, e.started_at_ms AS associated_at_ms
             FROM bridge_budget_executions e
             JOIN bridge_capacity_slots c ON c.budget_id=e.budget_id
             LEFT JOIN bridge_budget_receipts v2_receipt ON v2_receipt.budget_id=e.budget_id
             WHERE c.stage IN ('P','R') AND v2_receipt.budget_id IS NULL
               AND NOT EXISTS(SELECT 1 FROM bridge_core_requests r WHERE r.request_id=e.request_id)
               AND NOT EXISTS(SELECT 1 FROM bridge_core_upstream_sessions s WHERE s.account_ref=e.account_ref AND s.session_id=e.session_ref)
             ) GROUP BY account_ref ORDER BY account_ref",
        ).map_err(|error| format!("pending Core session account query unavailable: {error}"))?;
        let rows = statement.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))
            .map_err(|error| format!("pending Core session account query failed: {error}"))?;
        rows.map(|row| row.map_err(|error| format!("pending Core session account row invalid: {error}")))
            .collect()
    }

    pub(super) fn core_key_usage(&self) -> Result<Vec<CoreKeyUsage>, String> {
        let mut statement = self.connection.prepare(
            "WITH key_ids AS (
               SELECT key_id FROM bridge_core_api_keys
               UNION SELECT core_key_id FROM bridge_core_requests
             )
             SELECT ids.key_id,
                    COALESCE(keys.display_name, '等待 Core 同步'),
                    COALESCE(keys.active, 0),
                    COALESCE(SUM(CASE WHEN requests.conflict = 0 AND receipts.status = 'final' AND receipts.unit = 'credits'
                                      THEN receipts.actual_microcredits ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN requests.request_id IS NOT NULL AND requests.conflict = 0
                                      AND (receipts.request_id IS NULL OR receipts.status IN ('pending','unknown','unverified'))
                                      THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN requests.conflict = 1 OR receipts.status = 'conflict' THEN 1 ELSE 0 END), 0)
             FROM key_ids ids
             LEFT JOIN bridge_core_api_keys keys ON keys.key_id = ids.key_id
             LEFT JOIN bridge_core_requests requests ON requests.core_key_id = ids.key_id
             LEFT JOIN bridge_billing_receipts receipts ON receipts.request_id = requests.request_id
             GROUP BY ids.key_id, keys.display_name, keys.active
             ORDER BY lower(COALESCE(keys.display_name, '')), ids.key_id",
        ).map_err(|error| format!("Core Key usage query unavailable: {error}"))?;
        let rows = statement.query_map([], |row| {
            let key_id: String = row.get(0)?;
            let display_name: String = row.get(1)?;
            let active: bool = row.get(2)?;
            let verified_microcredits: i64 = row.get(3)?;
            let pending_requests: u64 = row.get(4)?;
            let conflict_requests: u64 = row.get(5)?;
            Ok((key_id, display_name, active, verified_microcredits, pending_requests, conflict_requests))
        }).map_err(|error| format!("Core Key usage query failed: {error}"))?;
        rows.map(|row| {
            let (key_id, display_name, active, microcredits, pending_requests, conflict_requests) =
                row.map_err(|error| format!("Core Key usage row is invalid: {error}"))?;
            Ok(CoreKeyUsage {
                key_id,
                display_name,
                active,
                verified_credits: credit_amount_from_microcredits(microcredits)?.to_string(),
                pending_requests,
                conflict_requests,
            })
        }).collect()
    }

    pub(super) fn record_receipt(
        &mut self,
        receipt: &BillingReceipt,
        trust: EvidenceTrust,
    ) -> Result<PersistReceiptResult, String> {
        if !valid_request_id(&receipt.request_id) {
            return Err("billing request_id is invalid".into());
        }

        let (status, actual_microcredits) = match trust {
            EvidenceTrust::CandidateOnly => (BillingReceiptStatus::Unverified, None),
            EvidenceTrust::VerifiedSourceContract => {
                if receipt.status != BillingReceiptStatus::Final
                    || receipt.unit.as_deref() != Some("credits")
                    || receipt.actual_credits.is_none()
                    || receipt.source_ref.as_deref().map_or(true, str::is_empty)
                    || receipt.observed_at_ms <= 0
                {
                    return Err("verified billing receipt is incomplete".into());
                }
                (
                    BillingReceiptStatus::Final,
                    receipt.actual_credits.map(CreditAmount::as_microcredits),
                )
            }
            EvidenceTrust::AuthorizedCoreVideoSession => {
                if receipt.status != BillingReceiptStatus::Final
                    || receipt.unit.as_deref() != Some("credits")
                    || receipt.actual_credits.is_none()
                    || receipt.source_ref.as_deref().map_or(true, |source| !source.starts_with("trae-usage-session:"))
                    || receipt.task_ref.as_deref().map_or(true, str::is_empty)
                    || receipt.observed_at_ms <= 0
                {
                    return Err("authorized Core video session receipt is incomplete".into());
                }
                (
                    BillingReceiptStatus::Final,
                    receipt.actual_credits.map(CreditAmount::as_microcredits),
                )
            }
            EvidenceTrust::AuthorizedCoreChatSession => {
                if receipt.status != BillingReceiptStatus::Final
                    || receipt.unit.as_deref() != Some("credits")
                    || receipt.actual_credits.is_none()
                    || receipt.source_ref.as_deref().map_or(true, |source| !source.starts_with("trae-usage-session:"))
                    || receipt.task_ref.is_some()
                    || receipt.observed_at_ms <= 0
                {
                    return Err("authorized Core chat session receipt is incomplete".into());
                }
                (
                    BillingReceiptStatus::Final,
                    receipt.actual_credits.map(CreditAmount::as_microcredits),
                )
            }
        };

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| format!("bridge billing transaction unavailable: {error}"))?;
        let existing = transaction
            .query_row(
                "SELECT status, actual_microcredits, unit, source_ref, task_ref
                 FROM bridge_billing_receipts WHERE request_id = ?1",
                params![receipt.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("bridge billing lookup failed: {error}"))?;

        let result = match existing {
            None => {
                insert_receipt(
                    &transaction,
                    receipt,
                    status,
                    actual_microcredits,
                )?;
                PersistReceiptResult::Created
            }
            Some((existing_status, existing_amount, existing_unit, existing_source, existing_task)) => {
                let existing_status = BillingReceiptStatus::parse(&existing_status)?;
                if existing_status == BillingReceiptStatus::Conflict {
                    PersistReceiptResult::Conflict
                } else if existing_status == BillingReceiptStatus::Final {
                    let same_receipt = status == BillingReceiptStatus::Final
                        && existing_amount == actual_microcredits
                        && existing_unit.as_deref() == receipt.unit.as_deref()
                        && existing_source.as_deref() == receipt.source_ref.as_deref()
                        && existing_task.as_deref() == receipt.task_ref.as_deref();
                    if same_receipt {
                        PersistReceiptResult::Duplicate
                    } else if status == BillingReceiptStatus::Unverified {
                        // A weak candidate must never replace or downgrade a verified final.
                        PersistReceiptResult::Duplicate
                    } else {
                        mark_conflict(&transaction, &receipt.request_id)?;
                        PersistReceiptResult::Conflict
                    }
                } else if existing_status == BillingReceiptStatus::Unverified
                    && status == BillingReceiptStatus::Unverified
                {
                    let same_source = existing_unit.as_deref() == receipt.unit.as_deref()
                        && existing_source.as_deref() == receipt.source_ref.as_deref()
                        && existing_task.as_deref() == receipt.task_ref.as_deref();
                    if same_source {
                        PersistReceiptResult::Duplicate
                    } else {
                        mark_conflict(&transaction, &receipt.request_id)?;
                        PersistReceiptResult::Conflict
                    }
                } else {
                    // A verified final receipt may promote a stored candidate/unknown row.
                    update_receipt(
                        &transaction,
                        receipt,
                        status,
                        actual_microcredits,
                    )?;
                    PersistReceiptResult::Created
                }
            }
        };
        transaction
            .commit()
            .map_err(|error| format!("bridge billing commit failed: {error}"))?;
        Ok(result)
    }

    pub(super) fn get_receipt(&self, request_id: &str) -> Result<Option<BillingReceipt>, String> {
        if !valid_request_id(request_id) {
            return Err("billing request_id is invalid".into());
        }
        let row = self
            .connection
            .query_row(
                "SELECT status, actual_microcredits, unit, source_ref, task_ref, observed_at_ms
                 FROM bridge_billing_receipts WHERE request_id = ?1",
                params![request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<i64>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                },
            )
            .optional()
            .map_err(|error| format!("bridge billing lookup failed: {error}"))?;

        row.map(|(status, amount, unit, source_ref, task_ref, observed_at_ms)| {
            let status = BillingReceiptStatus::parse(&status)?;
            let actual_credits = amount
                .map(credit_amount_from_microcredits)
                .transpose()?;
            Ok(BillingReceipt {
                request_id: request_id.into(),
                status,
                actual_credits,
                unit,
                source_ref,
                task_ref,
                observed_at_ms,
            })
        })
        .transpose()
    }
}

fn read_registry(transaction: &Transaction<'_>) -> Result<Vec<CoreKeyMetadata>, String> {
    let mut statement = transaction.prepare(
        "SELECT key_id, display_name, active FROM bridge_core_api_keys
         WHERE snapshot_version > 0 ORDER BY key_id",
    ).map_err(|error| format!("Core Key registry read failed: {error}"))?;
    let rows = statement.query_map([], |row| {
        Ok(CoreKeyMetadata {
            id: row.get(0)?,
            display_name: row.get(1)?,
            active: row.get(2)?,
        })
    }).map_err(|error| format!("Core Key registry query failed: {error}"))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(|error| format!("Core Key registry row is invalid: {error}"))
}

pub(super) fn valid_core_key_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

pub(super) fn quote_unavailable(request_id: &str) -> BridgeQuoteResponse {
    BridgeQuoteResponse {
        request_id: request_id.into(),
        status: "unavailable",
        quote_id: None,
        request_fingerprint: None,
        endpoint: None,
        model: None,
        max_credits: None,
        unit: "credits",
        expires_at_ms: None,
        source_ref: None,
        error_code: "quote_unavailable",
        message: "The upstream does not provide a verified per-request credit upper bound; paid requests are blocked.",
    }
}

pub(super) fn unknown_receipt(request_id: String) -> BillingReceipt {
    BillingReceipt::unknown(request_id)
}

pub(super) fn valid_request_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

pub(crate) fn pending_core_session_accounts_for_poll(
    data_dir: &Path,
) -> Result<Vec<(String, i64)>, String> {
    BridgeBillingStore::open(data_dir)?.pending_core_session_accounts()
}

pub(crate) fn persist_core_request_attribution(
    data_dir: &Path,
    request_id: &str,
    core_key_id: &str,
) -> Result<CoreRequestRecord, String> {
    BridgeBillingStore::open(data_dir)?.record_core_request(request_id, core_key_id)
}

pub(crate) fn persist_core_request_attribution_with_mode(
    data_dir: &Path,
    request_id: &str,
    core_key_id: &str,
    one_shot_test: bool,
) -> Result<CoreRequestRecord, String> {
    BridgeBillingStore::open(data_dir)?.record_core_request_with_mode(request_id, core_key_id, one_shot_test)
}

pub(crate) fn persist_core_session_attempt(
    data_dir: &Path,
    request_id: &str,
    account_ref: &str,
    session_id: &str,
) -> Result<CoreSessionLinkResult, String> {
    BridgeBillingStore::open(data_dir)?.record_core_session_attempt(request_id, account_ref, session_id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum CoreUsageSessionMatch {
    Unique { request_id: String, core_key_id: String },
    Ambiguous,
}

pub(crate) fn match_core_usage_sessions(
    data_dir: &Path,
    account_sessions: &[(String, String)],
) -> Result<std::collections::HashMap<(String, String), CoreUsageSessionMatch>, String> {
    let store = BridgeBillingStore::open(data_dir)?;
    let mut matches = std::collections::HashMap::new();
    for (account_ref, session_id) in account_sessions {
        match store.lookup_core_session_attribution(account_ref, session_id)? {
            Some(CoreSessionLookup::Unique { request_id, core_key_id }) => {
                matches.insert(
                    (account_ref.clone(), session_id.clone()),
                    CoreUsageSessionMatch::Unique { request_id, core_key_id },
                );
            }
            Some(CoreSessionLookup::Ambiguous) => {
                matches.insert((account_ref.clone(), session_id.clone()), CoreUsageSessionMatch::Ambiguous);
            }
            None => {}
        }
    }
    Ok(matches)
}

fn valid_internal_account_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn valid_upstream_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
}

fn insert_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &BillingReceipt,
    status: BillingReceiptStatus,
    amount: Option<i64>,
) -> Result<(), String> {
    transaction
        .execute(
            "INSERT INTO bridge_billing_receipts
             (request_id, status, actual_microcredits, unit, source_ref, task_ref, observed_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                receipt.request_id,
                status.as_str(),
                amount,
                receipt.unit,
                receipt.source_ref,
                receipt.task_ref,
                receipt.observed_at_ms,
                chrono::Utc::now().timestamp_millis(),
            ],
        )
        .map_err(|error| format!("bridge billing insert failed: {error}"))?;
    Ok(())
}

fn update_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &BillingReceipt,
    status: BillingReceiptStatus,
    amount: Option<i64>,
) -> Result<(), String> {
    transaction
        .execute(
            "UPDATE bridge_billing_receipts
             SET status = ?2, actual_microcredits = ?3, unit = ?4, source_ref = ?5,
                 task_ref = ?6, observed_at_ms = ?7, updated_at_ms = ?8
             WHERE request_id = ?1",
            params![
                receipt.request_id,
                status.as_str(),
                amount,
                receipt.unit,
                receipt.source_ref,
                receipt.task_ref,
                receipt.observed_at_ms,
                chrono::Utc::now().timestamp_millis(),
            ],
        )
        .map_err(|error| format!("bridge billing update failed: {error}"))?;
    Ok(())
}

fn mark_conflict(
    transaction: &rusqlite::Transaction<'_>,
    request_id: &str,
) -> Result<(), String> {
    transaction
        .execute(
            "UPDATE bridge_billing_receipts
             SET status = 'conflict', actual_microcredits = NULL, updated_at_ms = ?2
             WHERE request_id = ?1",
            params![request_id, chrono::Utc::now().timestamp_millis()],
        )
        .map_err(|error| format!("bridge billing conflict update failed: {error}"))?;
    Ok(())
}

fn credit_amount_from_microcredits(value: i64) -> Result<CreditAmount, String> {
    if value < 0 {
        return Err("stored bridge billing amount is negative".into());
    }
    let decimal = format!("{}.{:06}", value / 1_000_000, value % 1_000_000);
    CreditAmount::parse(&decimal, "credits")
}

fn initialize_bridge_schema<F>(connection: &mut Connection, migration_hook: F) -> Result<(), String>
where
    F: Fn(&Transaction<'_>) -> Result<(), String>,
{
    let has_metadata_object: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1 AND name NOT GLOB 'sqlite_*')",
        [BRIDGE_SCHEMA_META_TABLE],
        |row| row.get(0),
    ).map_err(|error| format!("bridge schema metadata inventory unavailable: {error}"))?;
    if has_metadata_object {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Deferred)
            .map_err(|error| format!("bridge billing read-only validation transaction unavailable: {error}"))?;
        validate_versioned_bridge_schema(&transaction)?;
        let version = read_bridge_schema_metadata(&transaction)?.0;
        transaction
            .commit()
            .map_err(|error| format!("bridge billing schema validation commit failed: {error}"))?;
        if version < BRIDGE_SCHEMA_VERSION {
            upgrade_bridge_capacity_schema(connection, &migration_hook)?;
        }
        return Ok(());
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("bridge billing schema transaction unavailable: {error}"))?;
    let objects = bridge_schema_objects(&transaction)?;
    match objects.iter().find(|(_, name)| name == BRIDGE_SCHEMA_META_TABLE) {
        Some((kind, _)) => {
            if kind != "table" {
                return Err("bridge billing schema metadata object is not a table".into());
            }
            validate_versioned_bridge_schema(&transaction)?;
        }
        None if objects.is_empty() => {
            create_legacy_bridge_schema(&transaction)?;
            validate_bridge_schema(&transaction, false, true)?;
            validate_foreign_key_integrity(&transaction)?;
            create_bridge_schema_metadata(&transaction, &migration_hook)?;
            validate_versioned_bridge_schema(&transaction)?;
        }
        None => {
            validate_bridge_schema(&transaction, false, true)?;
            validate_foreign_key_integrity(&transaction)?;
            create_bridge_schema_metadata(&transaction, &migration_hook)?;
            validate_versioned_bridge_schema(&transaction)?;
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("bridge billing schema commit failed: {error}"))?;
    upgrade_bridge_capacity_schema(connection, &|_| Ok(()))?;
    Ok(())
}

fn upgrade_bridge_capacity_schema<F>(connection: &mut Connection, hook: &F) -> Result<(), String>
where F: Fn(&Transaction<'_>) -> Result<(), String>,
{
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("bridge capacity migration unavailable: {error}"))?;
    validate_versioned_bridge_schema(&transaction)?;
    let (version, _, _) = read_bridge_schema_metadata(&transaction)?;
    if version == 1 {
        super::bridge_budget::create_schema(&transaction)?;
        super::bridge_budget::validate_schema(&transaction)?;
        hook(&transaction)?;
        let changed = transaction.execute(
            "UPDATE bridge_schema_meta SET schema_version = 2 WHERE singleton = 1 AND schema_version = 1", [],
        ).map_err(|error| format!("bridge capacity migration version update failed: {error}"))?;
        if changed != 1 { return Err("bridge capacity migration lost version CAS".into()); }
        validate_versioned_bridge_schema(&transaction)?;
    }
    if read_bridge_schema_metadata(&transaction)?.0 == 2 {
        super::bridge_prepared::create_schema(&transaction)?;
        super::bridge_prepared::validate_schema(&transaction)?;
        hook(&transaction)?;
        let changed=transaction.execute("UPDATE bridge_schema_meta SET schema_version=3 WHERE singleton=1 AND schema_version=2",[])
            .map_err(|error|format!("bridge preparation migration failed: {error}"))?;
        if changed!=1 { return Err("bridge preparation migration lost version CAS".into()); }
        validate_versioned_bridge_schema(&transaction)?;
    }
    if read_bridge_schema_metadata(&transaction)?.0 == 3 {
        super::bridge_execution::create_schema(&transaction)?;
        super::bridge_execution::validate_schema(&transaction)?;
        hook(&transaction)?;
        let changed=transaction.execute("UPDATE bridge_schema_meta SET schema_version=4 WHERE singleton=1 AND schema_version=3",[])
            .map_err(|error|format!("bridge execution migration failed: {error}"))?;
        if changed!=1 {return Err("bridge execution migration lost version CAS".into());}
        validate_versioned_bridge_schema(&transaction)?;
    }
    if read_bridge_schema_metadata(&transaction)?.0 == 4 {
        super::bridge_receipts::create_schema(&transaction)?;
        super::bridge_receipts::validate_schema(&transaction)?;
        hook(&transaction)?;
        let changed=transaction.execute("UPDATE bridge_schema_meta SET schema_version=5 WHERE singleton=1 AND schema_version=4",[])
            .map_err(|error|format!("bridge receipt migration failed: {error}"))?;
        if changed!=1 {return Err("bridge receipt migration lost version CAS".into());}
        validate_versioned_bridge_schema(&transaction)?;
    }
    if read_bridge_schema_metadata(&transaction)?.0 == 5 {
        super::bridge_rebase::create_schema(&transaction)?;
        super::bridge_rebase::validate_schema(&transaction)?;
        hook(&transaction)?;
        let changed=transaction.execute("UPDATE bridge_schema_meta SET schema_version=6 WHERE singleton=1 AND schema_version=5",[])
            .map_err(|error|format!("bridge rebase migration failed: {error}"))?;
        if changed!=1 {return Err("bridge rebase migration lost version CAS".into());}
        validate_versioned_bridge_schema(&transaction)?;
    }
    transaction.commit().map_err(|error| format!("bridge capacity migration commit failed: {error}"))
}

fn validate_versioned_bridge_schema(transaction: &Transaction<'_>) -> Result<(), String> {
    let objects = bridge_schema_objects(transaction)?;
    match objects.iter().find(|(_, name)| name == BRIDGE_SCHEMA_META_TABLE) {
        Some((kind, _)) if kind == "table" => {}
        Some(_) => return Err("bridge billing schema metadata object is not a table".into()),
        None => return Err("versioned bridge billing schema metadata is missing".into()),
    }
    validate_bridge_metadata_table(transaction)?;
    let (version, instance_id, generation) = read_bridge_schema_metadata(transaction)?;
    if version > BRIDGE_SCHEMA_VERSION {
        return Err(format!("bridge billing schema version {version} is newer than supported"));
    }
    if !(1..=BRIDGE_SCHEMA_VERSION).contains(&version) {
        return Err(format!("bridge billing schema version {version} is unsupported"));
    }
    validate_bridge_schema(transaction, true, false)?;
    if version >= 2 { super::bridge_budget::validate_schema(transaction)?; }
    if version >= 3 { super::bridge_prepared::validate_schema(transaction)?; }
    if version >= 4 { super::bridge_execution::validate_schema(transaction)?; }
    if version >= 5 { super::bridge_receipts::validate_schema(transaction)?; }
    if version >= 6 { super::bridge_rebase::validate_schema(transaction)?; }
    validate_bridge_identity(&instance_id, &generation)?;
    validate_foreign_key_integrity(transaction)?;
    Ok(())
}

fn bridge_schema_objects(transaction: &Transaction<'_>) -> Result<Vec<(String, String)>, String> {
    let mut statement = transaction
        .prepare("SELECT type, name FROM sqlite_master WHERE name NOT GLOB 'sqlite_*' ORDER BY type, name")
        .map_err(|error| format!("bridge schema inventory unavailable: {error}"))?;
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|error| format!("bridge schema inventory failed: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge schema inventory failed: {error}"));
    rows
}

fn bridge_table_columns(
    transaction: &Transaction<'_>,
    table: &str,
) -> Result<Vec<(String, String, i64, Option<String>, i64)>, String> {
    let mut statement = transaction
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"))?;
    let columns = statement
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"));
    columns
}

fn validate_bridge_schema(
    transaction: &Transaction<'_>,
    versioned: bool,
    check_constraints_by_write_probe: bool,
) -> Result<(), String> {
    const LEGACY_OBJECTS: &[(&str, &str)] = &[
        ("index", "bridge_core_upstream_sessions_lookup_idx"),
        ("table", "bridge_billing_receipts"),
        ("table", "bridge_core_key_registry_state"),
        ("table", "bridge_core_api_keys"),
        ("table", "bridge_core_requests"),
        ("table", "bridge_core_one_shot_test_requests"),
        ("table", "bridge_core_request_modes"),
        ("table", "bridge_core_upstream_sessions"),
    ];
    let mut objects = bridge_schema_objects(transaction)?;
    if versioned && read_bridge_schema_metadata(transaction)?.0 >= 2 {
        objects.retain(|(_, name)| !super::bridge_budget::SCHEMA_OBJECTS.iter().any(|(_, expected, _)| name == expected));
    }
    if versioned && read_bridge_schema_metadata(transaction)?.0 >= 3 {
        objects.retain(|(_,name)| !super::bridge_prepared::SCHEMA_OBJECTS.iter().any(|(_,expected,_)|name==expected));
    }
    if versioned && read_bridge_schema_metadata(transaction)?.0 >= 4 {
        objects.retain(|(_,name)| !super::bridge_execution::SCHEMA_OBJECTS.iter().any(|(_,expected,_)|name==expected));
    }
    if versioned && read_bridge_schema_metadata(transaction)?.0 >= 5 {
        objects.retain(|(_,name)| !super::bridge_receipts::SCHEMA_OBJECTS.iter().any(|(_,expected,_)|name==expected));
    }
    if versioned && read_bridge_schema_metadata(transaction)?.0 >= 6 {
        objects.retain(|(_,name)| !super::bridge_rebase::SCHEMA_OBJECTS.iter().any(|(_,expected,_)|name==expected));
    }
    let required_count = LEGACY_OBJECTS.len() + usize::from(versioned);
    let complete = objects.len() == required_count
        && LEGACY_OBJECTS.iter().all(|(kind, name)| {
            objects.iter().any(|(found_kind, found_name)| found_kind == kind && found_name == name)
        })
        && (!versioned || objects.iter().any(|(kind, name)| {
            kind == "table" && name == BRIDGE_SCHEMA_META_TABLE
        }));
    if !complete {
        return Err("bridge billing schema layout is incomplete or contains unknown objects".into());
    }

    let required_columns: &[(&str, &[(&str, &str, bool)])] = &[
        ("bridge_billing_receipts", &[
            ("request_id", "TEXT", true), ("status", "TEXT", true),
            ("actual_microcredits", "INTEGER", false), ("unit", "TEXT", false),
            ("source_ref", "TEXT", false), ("task_ref", "TEXT", false),
            ("observed_at_ms", "INTEGER", true), ("updated_at_ms", "INTEGER", true),
        ]),
        ("bridge_core_key_registry_state", &[
            ("singleton", "INTEGER", false), ("version", "INTEGER", true),
            ("updated_at_ms", "INTEGER", true),
        ]),
        ("bridge_core_api_keys", &[
            ("key_id", "TEXT", true), ("display_name", "TEXT", true),
            ("active", "INTEGER", true), ("snapshot_version", "INTEGER", true),
        ]),
        ("bridge_core_requests", &[
            ("request_id", "TEXT", true), ("core_key_id", "TEXT", true),
            ("associated_at_ms", "INTEGER", true), ("conflict", "INTEGER", true),
        ]),
        ("bridge_core_one_shot_test_requests", &[
            ("request_id", "TEXT", true), ("authorized_at_ms", "INTEGER", true),
        ]),
        ("bridge_core_request_modes", &[
            ("request_id", "TEXT", true), ("billing_mode", "TEXT", true),
            ("operation_id", "TEXT", false),
        ]),
        ("bridge_core_upstream_sessions", &[
            ("request_id", "TEXT", true), ("account_ref", "TEXT", true),
            ("session_id", "TEXT", true), ("conflict", "INTEGER", true),
            ("associated_at_ms", "INTEGER", true),
        ]),
    ];
    for (table, columns) in required_columns {
        let actual = bridge_table_columns(transaction, table)?;
        if actual.len() != columns.len() || columns.iter().any(|(name, kind, not_null)| {
            !actual.iter().any(|(found_name, found_kind, found_not_null, _, _)| {
                found_name == name && found_kind.eq_ignore_ascii_case(kind)
                    && (*found_not_null != 0) == *not_null
            })
        }) {
            return Err(format!("bridge billing {table} columns or NOT NULL constraints do not match the legacy layout"));
        }
        if matches!(*table, "bridge_core_requests" | "bridge_core_upstream_sessions")
            && !actual.iter().any(|(name, _, _, default, _)| {
                name == "conflict" && default.as_deref() == Some("0")
            })
        {
            return Err(format!("bridge billing {table} conflict column default is not canonical 0"));
        }
    }
    let required_primary_keys: &[(&str, &[(&str, i64)])] = &[
        ("bridge_billing_receipts", &[("request_id", 1)]),
        ("bridge_core_key_registry_state", &[("singleton", 1)]),
        ("bridge_core_api_keys", &[("key_id", 1)]),
        ("bridge_core_requests", &[("request_id", 1)]),
        ("bridge_core_one_shot_test_requests", &[("request_id", 1)]),
        ("bridge_core_request_modes", &[("request_id", 1)]),
        ("bridge_core_upstream_sessions", &[
            ("request_id", 1), ("account_ref", 2), ("session_id", 3),
        ]),
    ];
    for (table, keys) in required_primary_keys {
        let actual = bridge_table_columns(transaction, table)?;
        if actual.iter().filter(|(_, _, _, _, primary)| *primary != 0).count() != keys.len()
            || keys.iter().any(|(name, rank)| {
                !actual.iter().any(|(found_name, _, _, _, found_rank)| {
                    found_name == name && found_rank == rank
                })
            })
        {
            return Err(format!("bridge billing {table} primary key does not match the legacy layout"));
        }
    }
    let mut foreign_key_statement = transaction
        .prepare("PRAGMA foreign_key_list(bridge_core_request_modes)")
        .map_err(|error| format!("bridge request-mode foreign key unavailable: {error}"))?;
    let foreign_keys = foreign_key_statement
        .query_map([], |row| Ok((
            row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?,
            row.get::<_, String>(3)?, row.get::<_, String>(4)?,
        )))
        .map_err(|error| format!("bridge request-mode foreign key unreadable: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge request-mode foreign key unreadable: {error}"))?;
    if foreign_keys.len() != 1 || !matches!(
        &foreign_keys[0],
        (0, 0, table, from, to)
            if table == "bridge_core_requests" && from == "request_id" && to == "request_id"
    ) {
        return Err("bridge request-mode foreign key does not match the legacy layout".into());
    }
    validate_bridge_lookup_index(transaction)?;
    validate_receipt_status_binary_collation(transaction)?;
    if check_constraints_by_write_probe {
        validate_legacy_check_constraints(transaction)?;
    } else {
        validate_bridge_check_constraint_structure(transaction, versioned)?;
    }
    if versioned {
        validate_bridge_metadata_table(transaction)?;
    }
    Ok(())
}

fn validate_receipt_status_binary_collation(transaction: &Transaction<'_>) -> Result<(), String> {
    let sql: String = transaction.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'bridge_billing_receipts'",
        [],
        |row| row.get(0),
    ).map_err(|error| format!("bridge receipt status definition unavailable: {error}"))?;
    let tokens = tokenize_sqlite_schema_sql(&sql)?;
    let opening = tokens.iter().position(|token| token == "P:(")
        .ok_or_else(|| "bridge receipt table definition has no column list".to_string())?;
    let mut column_start = opening + 1;
    let mut nested = 0usize;
    for cursor in opening + 1..tokens.len() {
        let end_of_column = match tokens[cursor].as_str() {
            "P:(" => { nested += 1; false }
            "P:)" if nested == 0 => true,
            "P:)" => { nested -= 1; false }
            "P:," if nested == 0 => true,
            _ => false,
        };
        if !end_of_column { continue; }
        if tokens.get(column_start).is_some_and(|token| token == "I:status") {
            let mut constraint_depth = 0usize;
            let column = &tokens[column_start + 1..cursor];
            for (index, token) in column.iter().enumerate() {
                if token == "I:collate" && constraint_depth == 0
                    && column.get(index + 1).map(String::as_str) != Some("I:binary")
                {
                    return Err("bridge receipt status column must use BINARY collation".into());
                }
                match token.as_str() {
                    "P:(" => constraint_depth += 1,
                    "P:)" if constraint_depth > 0 => constraint_depth -= 1,
                    _ => {}
                }
            }
            return Ok(());
        }
        if tokens[cursor] == "P:)" { break; }
        column_start = cursor + 1;
    }
    Err("bridge receipt status column definition is missing".into())
}

fn validate_bridge_check_constraint_structure(
    transaction: &Transaction<'_>,
    versioned: bool,
) -> Result<(), String> {
    let mut required = vec![
        ("bridge_billing_receipts", "status IN ('pending','final','unknown','unverified','conflict')"),
        ("bridge_billing_receipts", "actual_microcredits IS NULL OR actual_microcredits >= 0"),
        ("bridge_core_key_registry_state", "singleton = 1"),
        ("bridge_core_key_registry_state", "version >= 0"),
        ("bridge_core_api_keys", "active IN (0, 1)"),
        ("bridge_core_api_keys", "snapshot_version >= 0"),
        ("bridge_core_requests", "conflict IN (0, 1)"),
        ("bridge_core_request_modes", "billing_mode IN ('quoted', 'legacy_one_shot', 'controlled_unquoted')"),
        ("bridge_core_request_modes", "(billing_mode = 'controlled_unquoted' AND operation_id IS NOT NULL) OR (billing_mode != 'controlled_unquoted' AND operation_id IS NULL)"),
        ("bridge_core_upstream_sessions", "conflict IN (0, 1)"),
    ];
    if versioned {
        required.extend([
            (BRIDGE_SCHEMA_META_TABLE, "singleton = 1"),
            (BRIDGE_SCHEMA_META_TABLE, "schema_version > 0"),
            (BRIDGE_SCHEMA_META_TABLE, "length(bridge_instance_id) BETWEEN 1 AND 128"),
            (BRIDGE_SCHEMA_META_TABLE, "length(event_generation) BETWEEN 1 AND 128"),
        ]);
    }

    for (table, required_expression) in required {
        let sql: Option<String> = transaction.query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get(0),
        ).optional().map_err(|error| format!("bridge {table} CHECK definition unavailable: {error}"))?;
        let sql = sql.ok_or_else(|| format!("bridge {table} CHECK definition is missing"))?;
        let actual = extract_sqlite_check_expressions(&sql)?;
        let expected = tokenize_sqlite_schema_sql(required_expression)?;
        if !actual.iter().any(|expression| expression == &expected) {
            return Err(format!("bridge {table} CHECK constraint is missing or altered"));
        }
    }
    Ok(())
}

fn extract_sqlite_check_expressions(sql: &str) -> Result<Vec<Vec<String>>, String> {
    let tokens = tokenize_sqlite_schema_sql(sql)?;
    let mut checks = Vec::new();
    let mut index = 0;
    while index + 1 < tokens.len() {
        if tokens[index] != "I:check" || tokens[index + 1] != "P:(" {
            index += 1;
            continue;
        }
        let mut depth = 1usize;
        let mut expression = Vec::new();
        let mut cursor = index + 2;
        while cursor < tokens.len() && depth != 0 {
            match tokens[cursor].as_str() {
                "P:(" => {
                    depth += 1;
                    expression.push(tokens[cursor].clone());
                }
                "P:)" => {
                    depth -= 1;
                    if depth != 0 {
                        expression.push(tokens[cursor].clone());
                    }
                }
                _ => expression.push(tokens[cursor].clone()),
            }
            cursor += 1;
        }
        if depth != 0 {
            return Err("bridge table CHECK expression has unbalanced parentheses".into());
        }
        checks.push(expression);
        index = cursor;
    }
    Ok(checks)
}

pub(super) fn tokenize_sqlite_schema_sql(sql: &str) -> Result<Vec<String>, String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'-' && bytes.get(index + 1) == Some(&b'-') {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' { index += 1; }
            continue;
        }
        if byte == b'/' && bytes.get(index + 1) == Some(&b'*') {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            if index + 1 >= bytes.len() {
                return Err("bridge table SQL has an unterminated comment".into());
            }
            index += 2;
            continue;
        }
        if byte == b'\'' {
            let start = index;
            index += 1;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == b'\'' {
                    if bytes.get(index + 1) == Some(&b'\'') {
                        index += 2;
                    } else {
                        index += 1;
                        closed = true;
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            if !closed { return Err("bridge table SQL has an unterminated string".into()); }
            tokens.push(format!("S:{}", &sql[start..index]));
            continue;
        }
        if matches!(byte, b'"' | b'`' | b'[') {
            let start = index;
            let close = if byte == b'[' { b']' } else { byte };
            index += 1;
            let mut closed = false;
            while index < bytes.len() {
                if bytes[index] == close {
                    if bytes.get(index + 1) == Some(&close) {
                        index += 2;
                    } else {
                        index += 1;
                        closed = true;
                        break;
                    }
                } else {
                    index += 1;
                }
            }
            if !closed { return Err("bridge table SQL has an unterminated identifier".into()); }
            let quoted = &sql[start + 1..index - 1];
            let unescaped = if close == b']' {
                quoted.replace("]]", "]")
            } else {
                let delimiter = close as char;
                quoted.replace(&format!("{delimiter}{delimiter}"), &delimiter.to_string())
            };
            tokens.push(format!("I:{}", unescaped.to_ascii_lowercase()));
            continue;
        }
        if byte.is_ascii_alphabetic() || byte == b'_' {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
            {
                index += 1;
            }
            tokens.push(format!("I:{}", sql[start..index].to_ascii_lowercase()));
            continue;
        }
        if byte.is_ascii_digit() {
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_digit() { index += 1; }
            tokens.push(format!("N:{}", &sql[start..index]));
            continue;
        }
        if byte >= 0x80 {
            let character = sql[index..].chars().next().unwrap();
            tokens.push(format!("U:{character}"));
            index += character.len_utf8();
            continue;
        }
        if index + 1 < bytes.len()
            && matches!(&bytes[index..index + 2], b">=" | b"<=" | b"!=" | b"<>" | b"==")
        {
            tokens.push(format!("P:{}", &sql[index..index + 2]));
            index += 2;
        } else {
            tokens.push(format!("P:{}", byte as char));
            index += 1;
        }
    }
    Ok(tokens)
}

fn validate_bridge_lookup_index(transaction: &Transaction<'_>) -> Result<(), String> {
    let mut statement = transaction
        .prepare("PRAGMA index_list('bridge_core_upstream_sessions')")
        .map_err(|error| format!("bridge session lookup index unavailable: {error}"))?;
    let indexes = statement
        .query_map([], |row| Ok((
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
        )))
        .map_err(|error| format!("bridge session lookup index unreadable: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge session lookup index unreadable: {error}"))?;
    if !indexes.iter().any(|(name, unique, origin, partial)| {
        name == "bridge_core_upstream_sessions_lookup_idx"
            && *unique == 0 && origin == "c" && *partial == 0
    }) {
        return Err("bridge session lookup index properties do not match the legacy layout".into());
    }

    let mut statement = transaction
        .prepare("PRAGMA index_xinfo('bridge_core_upstream_sessions_lookup_idx')")
        .map_err(|error| format!("bridge session lookup index columns unavailable: {error}"))?;
    let index_columns = statement
        .query_map([], |row| Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
        )))
        .map_err(|error| format!("bridge session lookup index columns unreadable: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge session lookup index columns unreadable: {error}"))?;
    let key_columns: Vec<_> = index_columns
        .iter()
        .filter(|(_, _, _, _, _, key)| *key == 1)
        .collect();
    if key_columns.len() != 2 || !matches!(
        (key_columns[0], key_columns[1]),
        ((0, cid_a, Some(name_a), 0, coll_a, 1), (1, cid_b, Some(name_b), 0, coll_b, 1))
            if *cid_a >= 0 && name_a == "account_ref" && coll_a.eq_ignore_ascii_case("BINARY")
                && *cid_b >= 0 && name_b == "session_id" && coll_b.eq_ignore_ascii_case("BINARY")
    ) {
        return Err("bridge session lookup index columns do not match the legacy layout".into());
    }
    Ok(())
}

fn validate_legacy_check_constraints(transaction: &Transaction<'_>) -> Result<(), String> {
    let nonce = rand::random::<u128>();
    require_legacy_check_rejection(
        transaction,
        "receipt status",
        |transaction| transaction.execute(
            "INSERT INTO bridge_billing_receipts
             (request_id, status, actual_microcredits, unit, observed_at_ms, updated_at_ms)
             VALUES (?1, 'invalid_status', NULL, 'credits', 0, 0)",
            [format!("bridge-schema-probe-{nonce}-receipt-status")],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "receipt amount",
        |transaction| transaction.execute(
            "INSERT INTO bridge_billing_receipts
             (request_id, status, actual_microcredits, unit, observed_at_ms, updated_at_ms)
             VALUES (?1, 'pending', -1, 'credits', 0, 0)",
            [format!("bridge-schema-probe-{nonce}-receipt-amount")],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "key registry singleton",
        |transaction| transaction.execute(
            "INSERT INTO bridge_core_key_registry_state(singleton, version, updated_at_ms)
             VALUES (2, 0, 0)",
            [],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "key registry version",
        |transaction| transaction.execute(
            "INSERT OR REPLACE INTO bridge_core_key_registry_state(singleton, version, updated_at_ms)
             VALUES (1, -1, 0)",
            [],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "Core Key active flag",
        |transaction| transaction.execute(
            "INSERT INTO bridge_core_api_keys(key_id, display_name, active, snapshot_version)
             VALUES (?1, 'probe', 2, 0)",
            [format!("bridge-schema-probe-{nonce}-active")],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "Core Key snapshot version",
        |transaction| transaction.execute(
            "INSERT INTO bridge_core_api_keys(key_id, display_name, active, snapshot_version)
             VALUES (?1, 'probe', 0, -1)",
            [format!("bridge-schema-probe-{nonce}-snapshot")],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "request conflict flag",
        |transaction| transaction.execute(
            "INSERT INTO bridge_core_requests(request_id, core_key_id, associated_at_ms, conflict)
             VALUES (?1, 'probe-key', 0, 2)",
            [format!("bridge-schema-probe-{nonce}-request-conflict")],
        ),
    )?;
    require_legacy_check_rejection(
        transaction,
        "request billing mode",
        |transaction| {
            let request_id = format!("bridge-schema-probe-{nonce}-mode-value");
            transaction.execute(
                "INSERT INTO bridge_core_requests(request_id, core_key_id, associated_at_ms, conflict)
                 VALUES (?1, 'probe-key', 0, 0)",
                [&request_id],
            )?;
            transaction.execute(
                "INSERT INTO bridge_core_request_modes(request_id, billing_mode, operation_id)
                 VALUES (?1, 'invalid_mode', NULL)",
                [&request_id],
            )
        },
    )?;
    require_legacy_check_rejection(
        transaction,
        "request operation binding",
        |transaction| {
            let request_id = format!("bridge-schema-probe-{nonce}-operation");
            transaction.execute(
                "INSERT INTO bridge_core_requests(request_id, core_key_id, associated_at_ms, conflict)
                 VALUES (?1, 'probe-key', 0, 0)",
                [&request_id],
            )?;
            transaction.execute(
                "INSERT INTO bridge_core_request_modes(request_id, billing_mode, operation_id)
                 VALUES (?1, 'quoted', 'unexpected-operation')",
                [&request_id],
            )
        },
    )?;
    require_legacy_check_rejection(
        transaction,
        "upstream session conflict flag",
        |transaction| transaction.execute(
            "INSERT INTO bridge_core_upstream_sessions
             (request_id, account_ref, session_id, conflict, associated_at_ms)
             VALUES (?1, 'probe-account', 'probe-session', 2, 0)",
            [format!("bridge-schema-probe-{nonce}-session-conflict")],
        ),
    )?;
    Ok(())
}

fn require_legacy_check_rejection<F>(
    transaction: &Transaction<'_>,
    constraint: &str,
    probe: F,
) -> Result<(), String>
where
    F: FnOnce(&Transaction<'_>) -> rusqlite::Result<usize>,
{
    transaction.execute_batch("SAVEPOINT bridge_schema_check_probe")
        .map_err(|error| format!("bridge schema {constraint} probe unavailable: {error}"))?;
    let probe_result = probe(transaction);
    transaction.execute_batch(
        "ROLLBACK TO bridge_schema_check_probe; RELEASE bridge_schema_check_probe",
    ).map_err(|error| format!("bridge schema {constraint} probe rollback failed: {error}"))?;
    match probe_result {
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK => Ok(()),
        Err(error) => Err(format!("bridge schema {constraint} probe failed unexpectedly: {error}")),
        Ok(_) => Err(format!("bridge billing legacy {constraint} CHECK constraint is missing")),
    }
}

fn validate_bridge_metadata_table(transaction: &Transaction<'_>) -> Result<(), String> {
    let columns = bridge_table_columns(transaction, BRIDGE_SCHEMA_META_TABLE)?;
    let expected = [
        ("singleton", "INTEGER", false, 1),
        ("schema_version", "INTEGER", true, 0),
        ("bridge_instance_id", "TEXT", true, 0),
        ("event_generation", "TEXT", true, 0),
    ];
    if columns.len() != expected.len() || expected.iter().any(|(name, kind, not_null, primary)| {
        !columns.iter().any(|(found_name, found_kind, found_not_null, _, found_primary)| {
            found_name == name && found_kind.eq_ignore_ascii_case(kind)
                && (*found_not_null != 0) == *not_null && found_primary == primary
        })
    }) {
        return Err("bridge billing schema metadata columns are invalid".into());
    }
    Ok(())
}

fn read_bridge_schema_metadata(transaction: &Transaction<'_>) -> Result<(i64, String, String), String> {
    let row_count: i64 = transaction
        .query_row("SELECT COUNT(*) FROM bridge_schema_meta", [], |row| row.get(0))
        .map_err(|error| format!("bridge billing schema metadata unreadable: {error}"))?;
    if row_count != 1 {
        return Err("bridge billing schema metadata must contain exactly one row".into());
    }
    let (singleton, version, instance_id, generation): (i64, i64, String, String) = transaction
        .query_row(
            "SELECT singleton, schema_version, bridge_instance_id, event_generation FROM bridge_schema_meta",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(|error| format!("bridge billing schema metadata unreadable: {error}"))?;
    if singleton != 1 {
        return Err("bridge billing schema metadata singleton is invalid".into());
    }
    Ok((version, instance_id, generation))
}

fn validate_bridge_identity(instance_id: &str, generation: &str) -> Result<(), String> {
    let valid = |value: &str| {
        !value.is_empty() && value.len() <= 128
            && value.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    };
    if !valid(instance_id) || !valid(generation) || instance_id == generation {
        return Err("bridge billing instance or generation metadata is invalid".into());
    }
    Ok(())
}

pub(super) fn is_recovery_required_generation(generation: &str) -> bool {
    generation.starts_with(BRIDGE_RECOVERY_REQUIRED_PREFIX)
}

fn is_active_owner_generation(generation: &str) -> bool {
    generation.starts_with(BRIDGE_ACTIVE_OWNER_PREFIX)
}

pub(super) fn is_dirty_bridge_generation(generation: &str) -> bool {
    is_active_owner_generation(generation) || is_recovery_required_generation(generation)
}

fn new_active_owner_generation() -> String {
    format!(
        "{BRIDGE_ACTIVE_OWNER_PREFIX}{}-{:032x}",
        std::process::id(),
        rand::random::<u128>()
    )
}

fn validate_foreign_key_integrity(transaction: &Transaction<'_>) -> Result<(), String> {
    let mut statement = transaction
        .prepare("PRAGMA foreign_key_check")
        .map_err(|error| format!("bridge billing foreign key check unavailable: {error}"))?;
    if statement
        .query([])
        .map_err(|error| format!("bridge billing foreign key check failed: {error}"))?
        .next()
        .map_err(|error| format!("bridge billing foreign key check failed: {error}"))?
        .is_some()
    {
        return Err("bridge billing legacy foreign key references are invalid".into());
    }
    Ok(())
}

fn create_legacy_bridge_schema(transaction: &Transaction<'_>) -> Result<(), String> {
    transaction.execute_batch(
        "CREATE TABLE bridge_billing_receipts (
           request_id TEXT PRIMARY KEY NOT NULL,
           status TEXT NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict')),
           actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
           unit TEXT,
           source_ref TEXT,
           task_ref TEXT,
           observed_at_ms INTEGER NOT NULL,
           updated_at_ms INTEGER NOT NULL
         );
         CREATE TABLE bridge_core_key_registry_state (
           singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
           version INTEGER NOT NULL CHECK(version >= 0),
           updated_at_ms INTEGER NOT NULL
         );
         CREATE TABLE bridge_core_api_keys (
           key_id TEXT PRIMARY KEY NOT NULL,
           display_name TEXT NOT NULL,
           active INTEGER NOT NULL CHECK(active IN (0, 1)),
           snapshot_version INTEGER NOT NULL CHECK(snapshot_version >= 0)
         );
         CREATE TABLE bridge_core_requests (
           request_id TEXT PRIMARY KEY NOT NULL,
           core_key_id TEXT NOT NULL,
           associated_at_ms INTEGER NOT NULL,
           conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1))
         );
         CREATE TABLE bridge_core_one_shot_test_requests (
           request_id TEXT PRIMARY KEY NOT NULL,
           authorized_at_ms INTEGER NOT NULL
         );
         CREATE TABLE bridge_core_request_modes (
           request_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_core_requests(request_id),
           billing_mode TEXT NOT NULL CHECK(billing_mode IN ('quoted', 'legacy_one_shot', 'controlled_unquoted')),
           operation_id TEXT,
           CHECK((billing_mode = 'controlled_unquoted' AND operation_id IS NOT NULL)
              OR (billing_mode != 'controlled_unquoted' AND operation_id IS NULL))
         );
         CREATE TABLE bridge_core_upstream_sessions (
           request_id TEXT NOT NULL,
           account_ref TEXT NOT NULL,
           session_id TEXT NOT NULL,
           conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1)),
           associated_at_ms INTEGER NOT NULL,
           PRIMARY KEY(request_id, account_ref, session_id)
         );
         CREATE INDEX bridge_core_upstream_sessions_lookup_idx
           ON bridge_core_upstream_sessions(account_ref, session_id);",
    ).map_err(|error| format!("bridge billing legacy schema creation failed: {error}"))
}

fn create_bridge_schema_metadata<F>(transaction: &Transaction<'_>, migration_hook: &F) -> Result<(), String>
where
    F: Fn(&Transaction<'_>) -> Result<(), String>,
{
    transaction.execute_batch(
        "CREATE TABLE bridge_schema_meta (
           singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
           schema_version INTEGER NOT NULL CHECK(schema_version > 0),
           bridge_instance_id TEXT NOT NULL CHECK(length(bridge_instance_id) BETWEEN 1 AND 128),
           event_generation TEXT NOT NULL CHECK(length(event_generation) BETWEEN 1 AND 128)
         );",
    ).map_err(|error| format!("bridge billing schema metadata creation failed: {error}"))?;
    migration_hook(transaction)?;
    let instance_id = format!("bridge-instance-v1-{:032x}", rand::random::<u128>());
    let generation = format!("bridge-generation-v1-{:032x}", rand::random::<u128>());
    transaction.execute(
        "INSERT INTO bridge_schema_meta(singleton, schema_version, bridge_instance_id, event_generation)
         VALUES (1, ?1, ?2, ?3)",
        params![1, instance_id, generation],
    ).map_err(|error| format!("bridge billing schema metadata insertion failed: {error}"))?;
    Ok(())
}

#[cfg(test)]
fn test_dir(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-bridge-billing-{label}-{}",
        rand::random::<u64>()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_v2_migration_preserves_v1_rows_and_identity() {
        let dir = test_dir("budget-v2-migration");
        let mut old = create_legacy_v0_database(&dir);
        let tx = old.transaction().unwrap();
        create_bridge_schema_metadata(&tx, &|_| Ok(())).unwrap();
        tx.commit().unwrap();
        let before = snapshot_legacy_rows(&old);
        let identity = bridge_metadata(&old).unwrap().unwrap();
        assert_eq!(identity.0, 1);
        drop(old);
        let store = BridgeBillingStore::open(&dir).unwrap();
        let after = bridge_metadata(&store.connection).unwrap().unwrap();
        assert_eq!(snapshot_legacy_rows(&store.connection), before);
        assert_eq!((&after.1, &after.2), (&identity.1, &identity.2));
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(after.0, BRIDGE_SCHEMA_VERSION, "legacy bridge must traverse every budget schema migration");
    }

    #[test]
    fn budget_v2_migration_failure_rolls_back_and_can_retry() {
        let dir=test_dir("budget-v2-rollback");
        let mut db=create_legacy_v0_database(&dir);
        let tx=db.transaction().unwrap();
        create_bridge_schema_metadata(&tx,&|_|Ok(())).unwrap();
        tx.commit().unwrap();
        let before=(schema_objects(&db),snapshot_legacy_rows(&db),bridge_metadata(&db).unwrap());
        let result=upgrade_bridge_capacity_schema(&mut db,&|tx| {
            let tables:i64=tx.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name='bridge_capacity_slots'",[],|r|r.get(0)).unwrap();
            assert_eq!(tables,1,"failure injection must follow v2 DDL");
            Err("injected failure before version CAS".into())
        });
        assert!(result.unwrap_err().contains("injected failure"));
        assert_eq!((schema_objects(&db),snapshot_legacy_rows(&db),bridge_metadata(&db).unwrap()),before);
        drop(db);
        let upgraded=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(snapshot_legacy_rows(&upgraded.connection),before.1);
        assert_eq!(bridge_metadata(&upgraded.connection).unwrap().unwrap().0,BRIDGE_SCHEMA_VERSION);
        drop(upgraded);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn budget_v2_rejects_missing_constraints_foreign_keys_and_indexes() {
        use super::super::bridge_budget::SCHEMA_OBJECTS;
        for variant in 0..3 {
            let dir=test_dir("budget-v2-invalid-schema");
            drop(BridgeBillingStore::open(&dir).unwrap());
            let db=Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
            if variant==0 { db.execute_batch("DROP INDEX bridge_capacity_slots_by_account_stage").unwrap(); }
            else {
                db.execute_batch("DROP TABLE bridge_capacity_slots").unwrap();
                let sql=SCHEMA_OBJECTS[1].2;
                let malformed=if variant==1 { sql.replace("CHECK(typeof(hold_microcredits) = 'integer' AND hold_microcredits > 0)","") }
                    else { sql.replace("REFERENCES bridge_capacity_accounts(account_ref)","") };
                assert_ne!(malformed,sql);
                db.execute_batch(&malformed).unwrap();
                db.execute_batch(SCHEMA_OBJECTS[2].2).unwrap();
            }
            let before=schema_objects(&db);
            drop(db);
            assert!(BridgeBillingStore::open(&dir).is_err(),"variant {variant} accepted");
            let db=Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
            assert_eq!(schema_objects(&db),before,"invalid layouts must not be silently repaired");
            drop(db); std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn prepared_v3_upgrade_preserves_v2_capacity_and_rolls_back_failure() {
        let dir=test_dir("prepared-v3-upgrade");
        let mut db=create_legacy_v0_database(&dir);
        let tx=db.transaction().unwrap();
        create_bridge_schema_metadata(&tx,&|_|Ok(())).unwrap();
        super::super::bridge_budget::create_schema(&tx).unwrap();
        tx.execute("UPDATE bridge_schema_meta SET schema_version=2",[]).unwrap();
        tx.execute_batch("INSERT INTO bridge_capacity_accounts(account_ref,snapshot_ref,snapshot_epoch,general_microcredits,work_microcredits,observed_at_ms)
            VALUES ('account','snapshot',1,100,100,1);
            INSERT INTO bridge_capacity_slots(budget_id,core_key_id,account_ref,snapshot_epoch,bridge_instance_id,event_generation,eligibility,stage,hold_microcredits,amount_microcredits,actual_microcredits)
            SELECT 'old-budget','legacy-key-a','account',1,bridge_instance_id,event_generation,'general_or_work','D',50,60,60 FROM bridge_schema_meta;").unwrap();
        tx.commit().unwrap();
        let before=(schema_objects(&db),snapshot_legacy_rows(&db),bridge_metadata(&db).unwrap());
        assert!(upgrade_bridge_capacity_schema(&mut db,&|_|Err("injected-v3-failure".into())).is_err());
        assert_eq!((schema_objects(&db),snapshot_legacy_rows(&db),bridge_metadata(&db).unwrap()),before);
        drop(db);
        let store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,60);
        let identity=bridge_metadata(&store.connection).unwrap().unwrap();
        let old=before.2.unwrap();
        assert_eq!((identity.1,identity.2),(old.1,old.2));
        assert_eq!(identity.0,BRIDGE_SCHEMA_VERSION);
        drop(store);std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn execution_v4_migration_failure_preserves_existing_sent_budget() {
        use super::super::bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome};
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("first consume")};
        store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&ctx.consume_epoch).unwrap();
        store.connection.execute_batch("DROP TABLE bridge_capacity_covered; DROP TABLE bridge_capacity_rebases; DROP TABLE bridge_budget_receipt_events; DROP TABLE bridge_budget_receipts; DROP TABLE bridge_budget_executions; UPDATE bridge_schema_meta SET schema_version=3").unwrap();
        let before=(schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap());
        let result=upgrade_bridge_capacity_schema(&mut store.connection,&|tx| {
            let count:i64=tx.query_row("SELECT COUNT(*) FROM bridge_budget_executions WHERE execution_state='unknown'",[],|r|r.get(0)).unwrap();
            assert_eq!(count,1);Err("injected-v4-migration-failure".into())
        });
        assert!(result.unwrap_err().contains("injected-v4"));
        assert_eq!((schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap()),before);
        let pending:i64=store.connection.query_row("SELECT SUM(amount_microcredits) FROM bridge_capacity_slots WHERE account_ref='account' AND stage='P'",[],|r|r.get(0)).unwrap();
        assert_eq!(pending,40_000_000);
        drop(store);let store=BridgeBillingStore::open(&dir).unwrap();
        assert!(store.budget_execution(&prepared.authorization.budget_id).unwrap().is_some());
        cleanup(dir,store,lease);
    }

    #[cfg(windows)]
    #[test]
    fn receipt_v5_migration_failure_preserves_existing_capacity_and_identity() {
        use super::super::bridge_prepared::tests::{fixture,input,cleanup};
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        store.connection.execute_batch("DROP TABLE bridge_capacity_covered; DROP TABLE bridge_capacity_rebases; DROP TABLE bridge_budget_receipt_events; DROP TABLE bridge_budget_receipts; UPDATE bridge_schema_meta SET schema_version=4").unwrap();
        let before=(schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap());
        let result=upgrade_bridge_capacity_schema(&mut store.connection,&|tx| {
            let count:i64=tx.query_row("SELECT COUNT(*) FROM bridge_budget_receipts",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
            Err("injected-v5-migration-failure".into())
        });
        assert!(result.unwrap_err().contains("injected-v5"));assert_eq!((schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap()),before);
        let pending:i64=store.connection.query_row("SELECT SUM(amount_microcredits) FROM bridge_capacity_slots WHERE account_ref='account' AND stage='P'",[],|r|r.get(0)).unwrap();
        assert_eq!(pending,40_000_000);
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.prepare_budget(&lease,&input(),None,20).unwrap().dispatch_token,prepared.dispatch_token);
        cleanup(dir,store,lease);
    }

    #[cfg(windows)]
    #[test]
    fn rebase_v6_migration_is_atomic_and_preserves_settled_receipts() {
        use super::super::{bridge_prepared::tests::{fixture,cleanup},bridge_receipts::tests::{send,cache}};
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-schema6");
        cache(&dir,&store,&[(&id,"50")],2000);store.confirm_budget_usage(&lease,&id).unwrap();
        store.connection.execute_batch("DROP TABLE bridge_capacity_covered; DROP TABLE bridge_capacity_rebases; UPDATE bridge_schema_meta SET schema_version=5").unwrap();
        let before=(schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap());
        let result=upgrade_bridge_capacity_schema(&mut store.connection,&|tx| {
            let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='bridge_capacity_covered')",[],|r|r.get(0)).unwrap();
            assert!(exists);Err("injected-v6-migration-failure".into())
        });
        assert!(result.unwrap_err().contains("injected-v6"));
        assert_eq!((schema_objects(&store.connection),bridge_metadata(&store.connection).unwrap()),before);
        drop(store);let store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        assert_eq!(store.latest_budget_receipt_event(&id).unwrap().unwrap().receipt.unwrap().actual_credits.unwrap().as_microcredits(),50_000_000);
        store.connection.execute_batch("DROP INDEX bridge_capacity_rebases_pending").unwrap();
        assert!(BridgeBillingStore::open(&dir).is_err(),"opening latest schema must not silently repair a missing guard");
        cleanup(dir,store,lease);
    }

    #[test]
    fn legacy_conflict_default_one_is_refused_for_both_tables() {
        assert_default_one_variants_are_refused(false);
    }

    #[test]
    fn versioned_conflict_default_one_is_refused_for_both_tables() {
        assert_default_one_variants_are_refused(true);
    }

    fn assert_default_one_variants_are_refused(versioned: bool) {
        let mut accepted = Vec::new();
        let mut changed = Vec::new();
        for table in ["bridge_core_requests", "bridge_core_upstream_sessions"] {
            let dir = test_dir(&format!("default-one-{}-{table}", if versioned { "v1" } else { "v0" }));
            let fixture = create_schema_variant_fixture(&dir, versioned);
            rebuild_conflict_with_default_one(&fixture, table);
            let omitted_value = omitted_conflict_value(&fixture, table);
            let before_schema = schema_objects(&fixture);
            let before_rows = snapshot_legacy_rows(&fixture);
            let before_metadata = bridge_metadata(&fixture).unwrap();
            drop(fixture);

            let opened = BridgeBillingStore::open(&dir);
            let was_accepted = opened.is_ok();
            drop(opened);
            let after = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
            let after_schema = schema_objects(&after);
            let after_rows = snapshot_legacy_rows(&after);
            let after_metadata = bridge_metadata(&after).unwrap();
            drop(after);
            std::fs::remove_dir_all(&dir).unwrap();

            assert_eq!(omitted_value, 1, "the {table} fixture must persist omitted conflict as 1");
            assert_eq!(before_metadata.is_some(), versioned, "fixture version is wrong for {table}");
            if was_accepted { accepted.push(table); }
            if after_schema != before_schema || after_rows != before_rows || after_metadata != before_metadata {
                changed.push(table);
            }
        }
        assert!(accepted.is_empty(), "DEFAULT 1 schema variants passed open: {accepted:?}");
        assert!(changed.is_empty(), "rejected DEFAULT 1 variants changed schema, rows or metadata: {changed:?}");
    }

    fn create_schema_variant_fixture(dir: &Path, versioned: bool) -> Connection {
        if versioned {
            drop(BridgeBillingStore::open(dir).unwrap());
            Connection::open(dir.join(BILLING_DB_FILE)).unwrap()
        } else {
            create_legacy_v0_database(dir)
        }
    }

    fn rebuild_conflict_with_default_one(connection: &Connection, table: &str) {
        let sql = match table {
            "bridge_core_requests" =>
                "PRAGMA foreign_keys = OFF;
                 CREATE TABLE bridge_core_requests_rebuilt (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   core_key_id TEXT NOT NULL,
                   associated_at_ms INTEGER NOT NULL,
                   conflict INTEGER NOT NULL DEFAULT 1 CHECK(conflict IN (0, 1))
                 );
                 INSERT INTO bridge_core_requests_rebuilt SELECT * FROM bridge_core_requests;
                 DROP TABLE bridge_core_requests;
                 ALTER TABLE bridge_core_requests_rebuilt RENAME TO bridge_core_requests;
                 PRAGMA foreign_keys = ON;",
            "bridge_core_upstream_sessions" =>
                "CREATE TABLE bridge_core_upstream_sessions_rebuilt (
                   request_id TEXT NOT NULL,
                   account_ref TEXT NOT NULL,
                   session_id TEXT NOT NULL,
                   conflict INTEGER NOT NULL DEFAULT 1 CHECK(conflict IN (0, 1)),
                   associated_at_ms INTEGER NOT NULL,
                   PRIMARY KEY(request_id, account_ref, session_id)
                 );
                 INSERT INTO bridge_core_upstream_sessions_rebuilt SELECT * FROM bridge_core_upstream_sessions;
                 DROP TABLE bridge_core_upstream_sessions;
                 ALTER TABLE bridge_core_upstream_sessions_rebuilt RENAME TO bridge_core_upstream_sessions;
                 CREATE INDEX bridge_core_upstream_sessions_lookup_idx
                   ON bridge_core_upstream_sessions(account_ref, session_id);",
            _ => unreachable!("only the two conflict columns are fixture variants"),
        };
        connection.execute_batch(sql).unwrap();
    }

    fn omitted_conflict_value(connection: &Connection, table: &str) -> i64 {
        let (insert, lookup) = match table {
            "bridge_core_requests" => (
                "INSERT INTO bridge_core_requests(request_id, core_key_id, associated_at_ms)
                 VALUES ('default-probe-request', 'legacy-key-a', 0)",
                "SELECT conflict FROM bridge_core_requests WHERE request_id = 'default-probe-request'",
            ),
            "bridge_core_upstream_sessions" => (
                "INSERT INTO bridge_core_upstream_sessions(request_id, account_ref, session_id, associated_at_ms)
                 VALUES ('legacy-quoted', 'default-probe-account', 'default-probe-session', 0)",
                "SELECT conflict FROM bridge_core_upstream_sessions
                 WHERE account_ref = 'default-probe-account' AND session_id = 'default-probe-session'",
            ),
            _ => unreachable!("only the two conflict columns are fixture variants"),
        };
        connection.execute_batch("SAVEPOINT omitted_conflict_probe").unwrap();
        connection.execute(insert, []).unwrap();
        let value = connection.query_row(lookup, [], |row| row.get(0)).unwrap();
        connection.execute_batch("ROLLBACK TO omitted_conflict_probe; RELEASE omitted_conflict_probe").unwrap();
        value
    }

    #[test]
    fn legacy_receipt_nocase_is_refused_without_migration() {
        assert_receipt_nocase_variant_is_refused(false);
    }

    #[test]
    fn versioned_receipt_nocase_is_refused() {
        assert_receipt_nocase_variant_is_refused(true);
    }

    fn assert_receipt_nocase_variant_is_refused(versioned: bool) {
        let dir = test_dir(if versioned { "receipt-nocase-v1" } else { "receipt-nocase-v0" });
        let fixture = create_schema_variant_fixture(&dir, versioned);
        rebuild_receipts_with_status_column(
            &fixture,
            "TEXT COLLATE NOCASE NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict'))",
            "",
        );
        fixture.execute_batch("SAVEPOINT nocase_probe").unwrap();
        fixture.execute(
            "INSERT INTO bridge_billing_receipts(request_id, status, observed_at_ms, updated_at_ms)
             VALUES ('nocase-probe', 'FINAL', 0, 0)",
            [],
        ).unwrap();
        let sql_matches_lowercase: i64 = fixture.query_row(
            "SELECT COUNT(*) FROM bridge_billing_receipts
             WHERE request_id = 'nocase-probe' AND status = 'final'",
            [], |row| row.get(0),
        ).unwrap();
        fixture.execute_batch("ROLLBACK TO nocase_probe; RELEASE nocase_probe").unwrap();
        let before_schema = schema_objects(&fixture);
        let before_rows = snapshot_legacy_rows(&fixture);
        let before_metadata = bridge_metadata(&fixture).unwrap();
        drop(fixture);

        let opened = BridgeBillingStore::open(&dir);
        let was_accepted = opened.is_ok();
        drop(opened);
        let after = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after_schema = schema_objects(&after);
        let after_rows = snapshot_legacy_rows(&after);
        let after_metadata = bridge_metadata(&after).unwrap();
        drop(after);
        std::fs::remove_dir_all(&dir).unwrap();

        assert_eq!(sql_matches_lowercase, 1, "the NOCASE fixture must match FINAL as final in SQL");
        assert!(BillingReceiptStatus::parse("FINAL").is_err(), "Rust must reject the same uppercase status");
        assert!(!was_accepted, "NOCASE receipt status must be refused for v{}", if versioned { 1 } else { 0 });
        assert_eq!(after_schema, before_schema, "rejection must not change the schema");
        assert_eq!(after_rows, before_rows, "rejection must preserve old rows");
        assert_eq!(after_metadata, before_metadata, "rejection must not add or change metadata");
    }

    fn rebuild_receipts_with_status_column(connection: &Connection, status_column: &str, extra_check: &str) {
        connection.execute_batch(&format!(
            "CREATE TABLE bridge_billing_receipts_rebuilt (
               request_id TEXT PRIMARY KEY NOT NULL,
               status {status_column},
               actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
               unit TEXT, source_ref TEXT, task_ref TEXT,
               observed_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
               {extra_check}
             );
             INSERT INTO bridge_billing_receipts_rebuilt SELECT * FROM bridge_billing_receipts;
             DROP TABLE bridge_billing_receipts;
             ALTER TABLE bridge_billing_receipts_rebuilt RENAME TO bridge_billing_receipts;"
        )).unwrap();
    }

    #[test]
    fn collate_words_in_comments_and_string_checks_do_not_change_binary_status() {
        let dir = test_dir("collate-token-decoys");
        let fixture = create_legacy_v0_database(&dir);
        rebuild_receipts_with_status_column(
            &fixture,
            "TEXT /* COLLATE NOCASE */ NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict'))",
            ", CHECK('COLLATE NOCASE' = 'COLLATE NOCASE')",
        );
        let before_rows = snapshot_legacy_rows(&fixture);
        drop(fixture);

        let migrated = BridgeBillingStore::open(&dir).is_ok();
        let reopened = BridgeBillingStore::open(&dir).is_ok();
        let after = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after_rows = snapshot_legacy_rows(&after);
        let metadata = bridge_metadata(&after).unwrap();
        let uppercase_insert = after.execute(
            "INSERT INTO bridge_billing_receipts(request_id, status, observed_at_ms, updated_at_ms)
             VALUES ('binary-probe', 'FINAL', 0, 0)",
            [],
        );
        drop(after);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(migrated && reopened, "comments and string literals must not count as column COLLATE");
        assert!(metadata.is_some(), "canonical v0 with harmless COLLATE text must migrate");
        assert_eq!(after_rows, before_rows, "migration must preserve legacy rows");
        assert!(matches!(uppercase_insert,
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_CHECK),
            "BINARY status CHECK must reject uppercase FINAL");
    }

    #[test]
    fn versioned_open_does_not_wait_for_or_take_a_writer_reservation() {
        let dir = test_dir("versioned-open-read-only");
        drop(BridgeBillingStore::open(&dir).unwrap());
        let blocker = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        blocker.busy_timeout(Duration::ZERO).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();

        let started = std::time::Instant::now();
        let reopened = BridgeBillingStore::open(&dir);
        let elapsed = started.elapsed();
        let opened = reopened.is_ok();
        drop(reopened);
        blocker.execute_batch("ROLLBACK;").unwrap();
        drop(blocker);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(opened, "read-only versioned validation should coexist with a WAL writer reservation");
        assert!(elapsed < Duration::from_millis(750),
            "ordinary versioned open should not wait on a writer, took {elapsed:?}");
    }

    #[test]
    fn versioned_open_rejects_a_removed_check_constraint() {
        let dir = test_dir("versioned-check-removed");
        drop(BridgeBillingStore::open(&dir).unwrap());

        let tamper = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        tamper.execute_batch(
            "PRAGMA foreign_keys = OFF;
             ALTER TABLE bridge_billing_receipts RENAME TO bridge_billing_receipts_with_checks;
             CREATE TABLE bridge_billing_receipts (
               request_id TEXT PRIMARY KEY NOT NULL,
               status TEXT NOT NULL,
               actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
               unit TEXT, source_ref TEXT, task_ref TEXT,
               observed_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO bridge_billing_receipts SELECT * FROM bridge_billing_receipts_with_checks;
             DROP TABLE bridge_billing_receipts_with_checks;",
        ).unwrap();
        drop(tamper);

        let result = BridgeBillingStore::open(&dir);
        let validation_error = result.as_ref().err().cloned();
        drop(result);
        std::fs::remove_dir_all(dir).unwrap();

        assert!(
            validation_error.as_deref().is_some_and(|error| error.contains("CHECK constraint is missing or altered")),
            "versioned open must structurally reject a missing receipt-status CHECK, got {validation_error:?}"
        );
    }

    #[test]
    fn legacy_migration_does_not_accept_unique_failure_as_missing_check_evidence() {
        let dir = test_dir("legacy-check-masked-by-unique");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "ALTER TABLE bridge_billing_receipts RENAME TO old_bridge_billing_receipts;
             CREATE TABLE bridge_billing_receipts (
               request_id TEXT PRIMARY KEY NOT NULL,
               status TEXT NOT NULL UNIQUE,
               actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
               unit TEXT, source_ref TEXT, task_ref TEXT,
               observed_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO bridge_billing_receipts SELECT * FROM old_bridge_billing_receipts;
             INSERT INTO bridge_billing_receipts
               (request_id, status, actual_microcredits, unit, observed_at_ms, updated_at_ms)
               VALUES ('check-probe-blocker', 'invalid_status', NULL, 'credits', 0, 0);
             DROP TABLE old_bridge_billing_receipts;",
        ).unwrap();
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir);
        let accepted = opened.is_ok();
        drop(opened);
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!accepted,
            "v0 migration must require SQLITE_CONSTRAINT_CHECK for the receipt-status probe, not UNIQUE");
        assert_eq!(metadata, None, "a masked missing CHECK must never receive the v1 marker");
    }

    fn create_legacy_v0_database(dir: &Path) -> Connection {
        let connection = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        connection.execute_batch(
            "CREATE TABLE bridge_billing_receipts (
               request_id TEXT PRIMARY KEY NOT NULL,
               status TEXT NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict')),
               actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
               unit TEXT,
               source_ref TEXT,
               task_ref TEXT,
               observed_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE bridge_core_key_registry_state (
               singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
               version INTEGER NOT NULL CHECK(version >= 0),
               updated_at_ms INTEGER NOT NULL
             );
             CREATE TABLE bridge_core_api_keys (
               key_id TEXT PRIMARY KEY NOT NULL,
               display_name TEXT NOT NULL,
               active INTEGER NOT NULL CHECK(active IN (0, 1)),
               snapshot_version INTEGER NOT NULL CHECK(snapshot_version >= 0)
             );
             CREATE TABLE bridge_core_requests (
               request_id TEXT PRIMARY KEY NOT NULL,
               core_key_id TEXT NOT NULL,
               associated_at_ms INTEGER NOT NULL,
               conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1))
             );
             CREATE TABLE bridge_core_one_shot_test_requests (
               request_id TEXT PRIMARY KEY NOT NULL,
               authorized_at_ms INTEGER NOT NULL
             );
             CREATE TABLE bridge_core_request_modes (
               request_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_core_requests(request_id),
               billing_mode TEXT NOT NULL CHECK(billing_mode IN ('quoted', 'legacy_one_shot', 'controlled_unquoted')),
               operation_id TEXT,
               CHECK((billing_mode = 'controlled_unquoted' AND operation_id IS NOT NULL)
                  OR (billing_mode != 'controlled_unquoted' AND operation_id IS NULL))
             );
             CREATE TABLE bridge_core_upstream_sessions (
               request_id TEXT NOT NULL,
               account_ref TEXT NOT NULL,
               session_id TEXT NOT NULL,
               conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1)),
               associated_at_ms INTEGER NOT NULL,
               PRIMARY KEY(request_id, account_ref, session_id)
             );
             CREATE INDEX bridge_core_upstream_sessions_lookup_idx
               ON bridge_core_upstream_sessions(account_ref, session_id);
             INSERT INTO bridge_core_key_registry_state VALUES (1, 7, 1700000000000);
             INSERT INTO bridge_core_api_keys VALUES ('legacy-key-a', 'Legacy A', 1, 7);
             INSERT INTO bridge_core_api_keys VALUES ('legacy-key-b', 'Legacy B', 0, 7);
             INSERT INTO bridge_core_requests VALUES ('legacy-quoted', 'legacy-key-a', 1700000000001, 0);
             INSERT INTO bridge_core_requests VALUES ('legacy-one-shot', 'legacy-key-b', 1700000000002, 0);
             INSERT INTO bridge_core_requests VALUES ('legacy-controlled', 'legacy-key-a', 1700000000003, 1);
             INSERT INTO bridge_core_one_shot_test_requests VALUES ('legacy-one-shot', 1700000000004);
             INSERT INTO bridge_core_request_modes VALUES ('legacy-quoted', 'quoted', NULL);
             INSERT INTO bridge_core_request_modes VALUES ('legacy-one-shot', 'legacy_one_shot', NULL);
             INSERT INTO bridge_core_request_modes VALUES ('legacy-controlled', 'controlled_unquoted', 'legacy-op');
             INSERT INTO bridge_core_upstream_sessions VALUES ('legacy-quoted', 'legacy-account-a', 'legacy-session-a', 0, 1700000000005);
             INSERT INTO bridge_core_upstream_sessions VALUES ('legacy-controlled', 'legacy-account-b', 'legacy-session-b', 1, 1700000000006);
             INSERT INTO bridge_billing_receipts VALUES ('legacy-quoted', 'final', 1250000, 'credits', 'legacy-source-final', 'legacy-task-final', 1700000000007, 1700000000008);
             INSERT INTO bridge_billing_receipts VALUES ('legacy-one-shot', 'conflict', NULL, 'credits', 'legacy-source-conflict', NULL, 1700000000009, 1700000000010);
             INSERT INTO bridge_billing_receipts VALUES ('legacy-controlled', 'unknown', NULL, 'credits', NULL, NULL, 1700000000011, 1700000000012);"
        ).unwrap();
        connection
    }

    fn snapshot_legacy_rows(connection: &Connection) -> Vec<(String, Vec<Vec<rusqlite::types::Value>>)> {
        [
            "bridge_billing_receipts",
            "bridge_core_key_registry_state",
            "bridge_core_api_keys",
            "bridge_core_requests",
            "bridge_core_one_shot_test_requests",
            "bridge_core_request_modes",
            "bridge_core_upstream_sessions",
        ]
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
                .unwrap();
            let column_count = statement.column_count();
            let rows = statement
                .query_map([], |row| {
                    (0..column_count)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            (table.to_string(), rows)
        })
        .collect()
    }

    fn bridge_metadata(connection: &Connection) -> rusqlite::Result<Option<(i64, String, String)>> {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bridge_schema_meta')",
            [],
            |row| row.get(0),
        )?;
        if !exists {
            return Ok(None);
        }
        connection
            .query_row(
                "SELECT schema_version, bridge_instance_id, event_generation FROM bridge_schema_meta WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map(Some)
    }

    fn create_bridge_metadata(connection: &Connection, version: i64) {
        connection.execute_batch(
            "CREATE TABLE bridge_schema_meta (
               singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
               schema_version INTEGER NOT NULL,
               bridge_instance_id TEXT NOT NULL,
               event_generation TEXT NOT NULL
             );",
        ).unwrap();
        connection.execute(
            "INSERT INTO bridge_schema_meta VALUES (1, ?1, 'fixture-instance', 'fixture-generation')",
            [version],
        ).unwrap();
    }

    fn schema_objects(connection: &Connection) -> Vec<(String, String)> {
        connection
            .prepare("SELECT type, name FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type, name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }

    #[test]
    fn legacy_v0_migration_preserves_all_existing_rows_and_creates_v1_metadata() {
        let dir = test_dir("legacy-v0-migration");
        let legacy = create_legacy_v0_database(&dir);
        let before = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let (opened, metadata, after) = match BridgeBillingStore::open(&dir) {
            Ok(store) => {
                let metadata = bridge_metadata(&store.connection).unwrap();
                let after = snapshot_legacy_rows(&store.connection);
                drop(store);
                (true, metadata, Some(after))
            }
            Err(_) => (false, None, None),
        };
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(opened, "a valid legacy v0 database should open");
        assert_eq!(
            metadata.map(|(version, instance_id, generation)| {
                version == BRIDGE_SCHEMA_VERSION && !instance_id.trim().is_empty() && !generation.trim().is_empty()
            }),
            Some(true),
            "migration should persist v1 and non-empty instance/generation identities"
        );
        assert_eq!(after, Some(before), "migration must retain every legacy row and field");
    }

    #[cfg(windows)]
    #[test]
    fn pre_migration_v0_wal_snapshot_clones_allow_only_one_host_activity_lease() {
        use super::super::bridge_budget_lease::BridgeBudgetLease;

        let root = test_dir("legacy-v0-wal-clones");
        let primary_dir = root.join("primary");
        let replica_dir = root.join("replica");
        std::fs::create_dir_all(&primary_dir).unwrap();
        std::fs::create_dir_all(&replica_dir).unwrap();

        let legacy = create_legacy_v0_database(&primary_dir);
        legacy.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA wal_autocheckpoint = 0;
             UPDATE bridge_core_key_registry_state
             SET updated_at_ms = updated_at_ms + 1 WHERE singleton = 1;",
        ).unwrap();
        let wal_path = std::path::PathBuf::from(format!(
            "{}-wal",
            primary_dir.join(BILLING_DB_FILE).display()
        ));
        let had_uncheckpointed_wal = std::fs::metadata(&wal_path)
            .is_ok_and(|metadata| metadata.len() > 32);
        let replica_path = replica_dir.join(BILLING_DB_FILE);
        legacy.execute(
            "VACUUM INTO ?1",
            [replica_path.to_string_lossy().as_ref()],
        ).unwrap();
        drop(legacy);

        let first = BridgeBillingStore::open(&primary_dir).unwrap();
        let second = BridgeBillingStore::open(&replica_dir).unwrap();
        let first_instance = first.bridge_identity().unwrap().0;
        let second_instance = second.bridge_identity().unwrap().0;
        let first_lease = BridgeBudgetLease::try_acquire(&first).unwrap().unwrap();
        let second_attempt = BridgeBudgetLease::try_acquire(&second).unwrap();
        let second_was_denied = second_attempt.is_none();
        drop(second_attempt);
        drop(first_lease);
        let second_acquired_after_release = BridgeBudgetLease::try_acquire(&second)
            .unwrap()
            .is_some();
        drop(second);
        drop(first);
        std::fs::remove_dir_all(&root).unwrap();

        assert!(had_uncheckpointed_wal, "the v0 clone fixture must include committed WAL state");
        assert_ne!(first_instance, second_instance,
            "each first migration currently assigns an independent v1 identity");
        assert!(second_was_denied,
            "two pre-migration v0 snapshot clones must not both hold host activity leases");
        assert!(second_acquired_after_release,
            "dropping the first two-level activity guard must release the host lock");
    }

    #[test]
    fn fresh_database_persists_instance_and_generation_across_reopen() {
        let dir = test_dir("fresh-v1-stability");
        let (first_open, first_metadata, second_open, second_metadata) =
            match BridgeBillingStore::open(&dir) {
                Ok(first) => {
                    let first_metadata = bridge_metadata(&first.connection).unwrap();
                    drop(first);
                    match BridgeBillingStore::open(&dir) {
                        Ok(second) => {
                            let second_metadata = bridge_metadata(&second.connection).unwrap();
                            drop(second);
                            (true, first_metadata, true, second_metadata)
                        }
                        Err(_) => (true, first_metadata, false, None),
                    }
                }
                Err(_) => (false, None, false, None),
            };
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(first_open && second_open, "fresh v1 DB should open and reopen");
        assert!(first_metadata.as_ref().is_some_and(|(version, instance, generation)| {
            *version == BRIDGE_SCHEMA_VERSION && !instance.trim().is_empty() && !generation.trim().is_empty()
        }), "fresh DB should persist v1 instance and generation metadata");
        assert_eq!(second_metadata, first_metadata, "ordinary reopen must not rotate identity or generation");
    }

    #[test]
    fn future_schema_version_is_rejected_without_schema_mutation() {
        let dir = test_dir("future-schema-version");
        let fixture = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        create_bridge_metadata(&fixture, 99);
        let before = schema_objects(&fixture);
        drop(fixture);

        let opened = BridgeBillingStore::open(&dir).is_ok();
        let after = Connection::open(dir.join(BILLING_DB_FILE))
            .map(|connection| schema_objects(&connection));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!opened, "a schema version newer than v1 must be refused");
        assert_eq!(after.unwrap(), before, "refusing a future DB must not create or rewrite objects");
    }

    #[test]
    fn partial_v1_schema_is_rejected_without_filling_missing_tables() {
        let dir = test_dir("partial-v1-schema");
        let fixture = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        create_bridge_metadata(&fixture, 1);
        let before = schema_objects(&fixture);
        drop(fixture);

        let opened = BridgeBillingStore::open(&dir).is_ok();
        let after = Connection::open(dir.join(BILLING_DB_FILE))
            .map(|connection| schema_objects(&connection));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!opened, "a v1 marker without its required legacy tables must be refused");
        assert_eq!(after.unwrap(), before, "refusing a partial DB must not synthesize missing tables");
    }

    #[test]
    fn conflicting_instance_and_generation_metadata_is_refused() {
        let dir = test_dir("conflicting-v1-identities");
        let store = BridgeBillingStore::open(&dir).unwrap();
        let instance_id = store.bridge_identity().unwrap().0;
        drop(store);
        let connection = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        connection.execute(
            "UPDATE bridge_schema_meta SET event_generation = ?1 WHERE singleton = 1",
            [&instance_id],
        ).unwrap();
        drop(connection);

        let reopened = BridgeBillingStore::open(&dir);
        let rejected = reopened.is_err();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(rejected, "conflicting instance/generation metadata must fail closed");
    }

    #[test]
    fn malformed_legacy_layout_is_rejected_without_creating_other_tables() {
        let dir = test_dir("malformed-legacy-layout");
        let fixture = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        fixture.execute_batch(
            "CREATE TABLE bridge_core_api_keys (key_id TEXT PRIMARY KEY NOT NULL, active INTEGER NOT NULL);
             INSERT INTO bridge_core_api_keys VALUES ('partial-key', 1);",
        ).unwrap();
        let before = schema_objects(&fixture);
        drop(fixture);

        let opened = BridgeBillingStore::open(&dir).is_ok();
        let after = Connection::open(dir.join(BILLING_DB_FILE))
            .map(|connection| schema_objects(&connection));
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!opened, "an incomplete unversioned legacy layout must be refused");
        assert_eq!(after.unwrap(), before, "refusing malformed v0 must not fill the rest of the layout");
    }

    #[test]
    fn legacy_layout_without_receipt_primary_key_is_refused() {
        let dir = test_dir("legacy-missing-receipt-primary-key");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "ALTER TABLE bridge_billing_receipts RENAME TO old_bridge_billing_receipts;
             CREATE TABLE bridge_billing_receipts (
               request_id TEXT NOT NULL,
               status TEXT NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict')),
               actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
               unit TEXT, source_ref TEXT, task_ref TEXT,
               observed_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO bridge_billing_receipts SELECT * FROM old_bridge_billing_receipts;
             DROP TABLE old_bridge_billing_receipts;",
        ).unwrap();
        let before = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir).is_ok();
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after = snapshot_legacy_rows(&reopened);
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!opened, "v0 without the immutable receipt request key must be refused");
        assert_eq!(metadata, None, "rejected v0 must not gain a version marker");
        assert_eq!(after, before, "rejection must not alter legacy receipt or other rows");
    }

    #[test]
    fn legacy_layout_without_request_mode_foreign_key_is_refused() {
        let dir = test_dir("legacy-missing-mode-foreign-key");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "ALTER TABLE bridge_core_request_modes RENAME TO old_bridge_core_request_modes;
             CREATE TABLE bridge_core_request_modes (
               request_id TEXT PRIMARY KEY NOT NULL,
               billing_mode TEXT NOT NULL CHECK(billing_mode IN ('quoted', 'legacy_one_shot', 'controlled_unquoted')),
               operation_id TEXT,
               CHECK((billing_mode = 'controlled_unquoted' AND operation_id IS NOT NULL)
                  OR (billing_mode != 'controlled_unquoted' AND operation_id IS NULL))
             );
             INSERT INTO bridge_core_request_modes SELECT * FROM old_bridge_core_request_modes;
             DROP TABLE old_bridge_core_request_modes;",
        ).unwrap();
        let before = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir).is_ok();
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after = snapshot_legacy_rows(&reopened);
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!opened, "v0 without the declared request-mode foreign key must be refused");
        assert_eq!(metadata, None, "rejected v0 must not gain a version marker");
        assert_eq!(after, before, "rejection must not alter legacy rows");
    }

    #[test]
    fn legacy_layout_without_request_mode_checks_is_refused_without_mutation() {
        let dir = test_dir("legacy-missing-mode-checks");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE bridge_core_request_modes_rebuilt (
               request_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_core_requests(request_id),
               billing_mode TEXT NOT NULL,
               operation_id TEXT
             );
             INSERT INTO bridge_core_request_modes_rebuilt SELECT * FROM bridge_core_request_modes;
             DROP TABLE bridge_core_request_modes;
             ALTER TABLE bridge_core_request_modes_rebuilt RENAME TO bridge_core_request_modes;
             PRAGMA foreign_keys = ON;",
        ).unwrap();
        let before_schema = schema_objects(&legacy);
        let before_rows = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir);
        let accepted = opened.is_ok();
        drop(opened);
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after_schema = schema_objects(&reopened);
        let after_rows = snapshot_legacy_rows(&reopened);
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!accepted, "v0 request-mode CHECK constraints must be required");
        assert_eq!(metadata, None, "rejected v0 must not gain a version marker");
        assert_eq!(after_schema, before_schema, "rejection must retain every legacy object");
        assert_eq!(after_rows, before_rows, "rejection must preserve every legacy row");
    }

    #[test]
    fn legacy_layout_without_non_primary_key_not_null_is_refused_without_mutation() {
        let dir = test_dir("legacy-missing-request-key-not-null");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE bridge_core_requests_rebuilt (
               request_id TEXT PRIMARY KEY NOT NULL,
               core_key_id TEXT,
               associated_at_ms INTEGER NOT NULL,
               conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1))
             );
             INSERT INTO bridge_core_requests_rebuilt SELECT * FROM bridge_core_requests;
             DROP TABLE bridge_core_requests;
             ALTER TABLE bridge_core_requests_rebuilt RENAME TO bridge_core_requests;
             PRAGMA foreign_keys = ON;",
        ).unwrap();
        let before_schema = schema_objects(&legacy);
        let before_rows = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir);
        let accepted = opened.is_ok();
        drop(opened);
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after_schema = schema_objects(&reopened);
        let after_rows = snapshot_legacy_rows(&reopened);
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!accepted, "v0 request attribution must retain its non-null Core key");
        assert_eq!(metadata, None, "rejected v0 must not gain a version marker");
        assert_eq!(after_schema, before_schema, "rejection must retain every legacy object");
        assert_eq!(after_rows, before_rows, "rejection must preserve every legacy row");
    }

    #[test]
    fn legacy_layout_with_same_named_but_wrong_lookup_index_is_refused() {
        let dir = test_dir("legacy-wrong-session-index");
        let legacy = create_legacy_v0_database(&dir);
        legacy.execute_batch(
            "DROP INDEX bridge_core_upstream_sessions_lookup_idx;
             CREATE INDEX bridge_core_upstream_sessions_lookup_idx
               ON bridge_core_upstream_sessions(request_id, session_id);",
        ).unwrap();
        let before_schema = schema_objects(&legacy);
        let before_rows = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let opened = BridgeBillingStore::open(&dir);
        let accepted = opened.is_ok();
        drop(opened);
        let reopened = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        let after_schema = schema_objects(&reopened);
        let after_rows = snapshot_legacy_rows(&reopened);
        let metadata = bridge_metadata(&reopened).unwrap();
        drop(reopened);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(!accepted, "a matching index name cannot replace the legacy lookup semantics");
        assert_eq!(metadata, None, "rejected v0 must not gain a version marker");
        assert_eq!(after_schema, before_schema, "rejection must retain every legacy object");
        assert_eq!(after_rows, before_rows, "rejection must preserve every legacy row");
    }

    #[test]
    fn failed_legacy_migration_rolls_back_schema_and_keeps_legacy_rows() {
        let dir = test_dir("migration-write-failure");
        let legacy = create_legacy_v0_database(&dir);
        let before = snapshot_legacy_rows(&legacy);
        drop(legacy);

        let mut connection = Connection::open(dir.join(BILLING_DB_FILE)).unwrap();
        connection.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL; PRAGMA foreign_keys = ON;",
        ).unwrap();
        let saw_uncommitted_metadata_table = std::cell::Cell::new(false);
        let migration_error = initialize_bridge_schema(&mut connection, |transaction| {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'bridge_schema_meta')",
                [], |row| row.get(0),
            ).unwrap();
            saw_uncommitted_metadata_table.set(exists);
            Err("injected failure after metadata DDL".into())
        }).err();
        let after_rows = snapshot_legacy_rows(&connection);
        let metadata = bridge_metadata(&connection).unwrap();
        drop(connection);
        std::fs::remove_dir_all(&dir).unwrap();

        assert!(saw_uncommitted_metadata_table.get(), "fault must occur after metadata DDL in the migration transaction");
        assert_eq!(migration_error.as_deref(), Some("injected failure after metadata DDL"));
        assert_eq!(metadata, None, "failed migration must not leave a v1 marker");
        assert_eq!(after_rows, before, "failed migration must preserve all legacy rows and fields");
    }

    #[cfg(windows)]
    #[test]
    fn explicit_recovery_rejects_a_foreign_lease_before_worker_confirmation() {
        use super::super::bridge_budget_lease::BridgeBudgetLease;

        let first_dir = test_dir("recovery-foreign-lease-a");
        let second_dir = test_dir("recovery-foreign-lease-b");
        let first = BridgeBillingStore::open(&first_dir).unwrap();
        let mut second = BridgeBillingStore::open(&second_dir).unwrap();
        let mut foreign_lease = BridgeBudgetLease::try_acquire(&first).unwrap().unwrap();
        let before = second.bridge_identity().unwrap().1;
        let confirmed = std::cell::Cell::new(false);
        let result = second.rotate_event_generation_for_recovery(&mut foreign_lease, || {
            confirmed.set(true);
            Ok(())
        });
        let after = second.bridge_identity().unwrap().1;
        drop(foreign_lease);
        drop(second);
        drop(first);
        std::fs::remove_dir_all(first_dir).unwrap();
        std::fs::remove_dir_all(second_dir).unwrap();

        assert!(result.is_err(), "recovery must reject another bridge instance's lease");
        assert!(!confirmed.get(), "foreign lease must be rejected before worker confirmation");
        assert_eq!(after, before, "foreign lease must not rotate the stored generation");
    }

    #[cfg(windows)]
    #[test]
    fn active_lease_is_not_recovery_confirmation_and_drop_stays_dirty() {
        use super::super::bridge_budget_lease::BridgeBudgetLease;

        let dir = test_dir("explicit-generation-recovery");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let before = store.bridge_identity().unwrap().1;
        let mut lease = BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        let after_acquire = store.bridge_identity().unwrap().1;
        let confirmed = std::cell::Cell::new(false);
        let recovery = store.rotate_event_generation_for_recovery(&mut lease, || {
            confirmed.set(true);
            Ok(())
        });
        let after_recovery = store.bridge_identity().unwrap().1;
        drop(lease);
        drop(store);
        let reopened = BridgeBillingStore::open(&dir).unwrap();
        let after_reopen = reopened.bridge_identity().unwrap().1;
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();

        assert!(after_acquire.starts_with("bridge-active-v1-"),
            "acquisition must persist the active owner before making the lease charge-ready");
        assert!(recovery.is_err(), "an ordinary successful mutex acquisition is not recovery evidence");
        assert!(!confirmed.get(), "recovery callback must not run without a persisted pending marker");
        assert_eq!(after_recovery, after_acquire, "rejected recovery must retain the active owner marker");
        assert_eq!(after_reopen, after_acquire,
            "dropping a lease without explicit clean close must leave the database dirty");
        assert!(before.starts_with("bridge-generation-v1-"), "fixture starts from a clean generation");
    }

    #[test]
    fn core_controlled_attribution_rejects_operation_or_mode_rebinding() {
        let dir = test_dir("core-controlled-attribution");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(
            store.record_core_request_with_billing_mode(
                "req-controlled", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-a")
            ).unwrap(),
            CoreRequestRecord::Created
        );
        assert_eq!(
            store.record_core_request_with_billing_mode(
                "req-controlled", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-a")
            ).unwrap(),
            CoreRequestRecord::Duplicate
        );
        assert_eq!(
            store.record_core_request_with_billing_mode(
                "req-controlled", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-b")
            ).unwrap(),
            CoreRequestRecord::Conflict
        );
        assert_eq!(
            store.record_core_request_with_billing_mode(
                "req-controlled", "key_a", CoreBillingMode::Quoted, None
            ).unwrap(),
            CoreRequestRecord::Conflict
        );
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn core_key_registry_is_atomic_idempotent_and_retains_only_safe_metadata() {
        let dir = test_dir("core-key-registry");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let snapshot = CoreKeyRegistrySnapshot {
            version: 10,
            keys: vec![CoreKeyMetadata {
                id: "key_opaque_1".into(),
                display_name: "周的桌面 Key".into(),
                active: true,
            }],
        };
        assert_eq!(store.replace_core_key_registry(&snapshot).unwrap(), RegistryUpdate::Applied);
        assert_eq!(store.replace_core_key_registry(&snapshot).unwrap(), RegistryUpdate::Unchanged);

        let serialized = serde_json::to_string(&snapshot).unwrap();
        assert!(!serialized.contains("aw_live_"));
        assert!(!serialized.contains("prefix"));

        let stale = CoreKeyRegistrySnapshot { version: 9, keys: vec![] };
        assert!(store.replace_core_key_registry(&stale).is_err());
        let rows = store.core_key_usage().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key_id, "key_opaque_1");
        assert_eq!(rows[0].display_name, "周的桌面 Key");
        assert!(rows[0].active);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn core_request_attribution_is_immutable_per_request_and_isolated_per_key() {
        let dir = test_dir("core-key-attribution");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let snapshot = CoreKeyRegistrySnapshot {
            version: 1,
            keys: vec![
                CoreKeyMetadata { id: "key_a".into(), display_name: "A".into(), active: true },
                CoreKeyMetadata { id: "key_b".into(), display_name: "B".into(), active: true },
            ],
        };
        store.replace_core_key_registry(&snapshot).unwrap();
        assert_eq!(store.record_core_request("req-a", "key_a").unwrap(), CoreRequestRecord::Created);
        assert_eq!(store.record_core_request("req-a", "key_a").unwrap(), CoreRequestRecord::Duplicate);
        assert_eq!(store.record_core_request("req-b", "key_b").unwrap(), CoreRequestRecord::Created);
        assert_eq!(store.record_core_request("req-pending", "key_a").unwrap(), CoreRequestRecord::Created);
        assert_eq!(store.record_core_request("req-conflict", "key_a").unwrap(), CoreRequestRecord::Created);
        assert_eq!(store.record_receipt(&final_receipt("req-a", "2.375", "source-a"), EvidenceTrust::VerifiedSourceContract).unwrap(), PersistReceiptResult::Created);
        assert_eq!(store.record_receipt(&final_receipt("req-pending", "9", "candidate"), EvidenceTrust::CandidateOnly).unwrap(), PersistReceiptResult::Created);
        assert_eq!(store.record_core_request("req-conflict", "key_b").unwrap(), CoreRequestRecord::Conflict);

        let rows = store.core_key_usage().unwrap();
        let key_a = rows.iter().find(|row| row.key_id == "key_a").unwrap();
        let key_b = rows.iter().find(|row| row.key_id == "key_b").unwrap();
        assert_eq!(key_a.verified_credits, "2.375000");
        assert_eq!(key_a.pending_requests, 1);
        assert_eq!(key_a.conflict_requests, 1);
        assert_eq!(key_b.verified_credits, "0.000000");
        assert_eq!(key_b.pending_requests, 1);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upstream_session_is_usable_only_when_unique_to_one_request_and_account() {
        let dir = test_dir("core-session-attribution");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-a", "key_a").unwrap();
        store.record_core_request("req-b", "key_b").unwrap();

        assert_eq!(
            store.record_core_session_attempt("req-a", "uid-1", "session-shared").unwrap(),
            CoreSessionLinkResult::Created
        );
        assert_eq!(
            store.record_core_session_attempt("req-a", "uid-1", "session-shared").unwrap(),
            CoreSessionLinkResult::Duplicate
        );
        assert_eq!(
            store.record_core_session_attempt("req-b", "uid-1", "session-shared").unwrap(),
            CoreSessionLinkResult::Conflict
        );
        assert_eq!(
            store.lookup_core_session_attribution("uid-1", "session-shared").unwrap(),
            Some(CoreSessionLookup::Ambiguous)
        );

        store.record_core_session_attempt("req-b", "uid-2", "session-private").unwrap();
        assert_eq!(
            store.lookup_core_session_attribution("uid-2", "session-private").unwrap(),
            Some(CoreSessionLookup::Unique { request_id: "req-b".into(), core_key_id: "key_b".into() })
        );
        assert_eq!(store.lookup_core_session_attribution("uid-1", "session-private").unwrap(), None);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn one_shot_request_uses_the_same_unique_session_evidence_as_regular_requests() {
        let dir = test_dir("one-shot-session-receipt");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request_with_mode("req-test", "key_a", true).unwrap();
        store.record_core_session_attempt("req-test", "uid-a", "session-a").unwrap();
        assert!(matches!(
            store.core_session_for_request("req-test").unwrap(),
            CoreBillingSessionLookup::Unique { ref core_key_id, ref account_ref, ref session_id, .. }
                if core_key_id == "key_a" && account_ref == "uid-a" && session_id == "session-a"
        ));

        let receipt = BillingReceipt {
            request_id: "req-test".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("245.85", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-a".into()),
            task_ref: Some("video-task-a".into()),
            observed_at_ms: 1_790_000_000_000,
        };
        assert_eq!(
            store.record_core_session_receipt(&receipt, "uid-a", "session-a", "key_a").unwrap(),
            PersistReceiptResult::Created,
        );
        let stored = store.get_receipt("req-test").unwrap().unwrap();
        assert_eq!(stored.actual_credits.unwrap().to_string(), "245.850000");
        assert_eq!(stored.task_ref.as_deref(), Some("video-task-a"));

        assert!(store.record_core_session_attempt("req-test", "uid-a", "session-retry").is_err());
        assert!(matches!(store.core_session_for_request("req-test").unwrap(), CoreBillingSessionLookup::Unique { .. }));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn controlled_video_recovery_only_exposes_registered_unconflicted_request() {
        let dir = test_dir("controlled-video-recovery-identity");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request_with_billing_mode("req-video-a", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-a")).unwrap();
        assert_eq!(store.controlled_request_key("req-video-a").unwrap().as_deref(), Some("key_a"));
        assert_eq!(store.controlled_request_key("missing").unwrap(), None);
        store.record_core_request_with_billing_mode("req-video-a", "key_b", CoreBillingMode::ControlledUnquoted, Some("op-a")).unwrap();
        assert_eq!(store.controlled_request_key("req-video-a").unwrap(), None);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pre_dispatch_rejection_records_zero_only_before_any_upstream_attempt() {
        let dir = test_dir("controlled-pre-dispatch-no-charge");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request_with_billing_mode("req-no-charge", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-no-charge")).unwrap();
        assert_eq!(store.record_controlled_pre_dispatch_no_charge("req-no-charge").unwrap(), PersistReceiptResult::Created);
        let receipt = store.get_receipt("req-no-charge").unwrap().unwrap();
        assert_eq!(receipt.status, BillingReceiptStatus::Final);
        assert_eq!(receipt.actual_credits.unwrap().as_microcredits(), 0);
        assert!(receipt.source_ref.unwrap().starts_with("aiwork-pre-dispatch-no-charge:"));
        store.record_core_request_with_billing_mode("req-attempted", "key_a", CoreBillingMode::ControlledUnquoted, Some("op-attempted")).unwrap();
        store.record_core_session_attempt("req-attempted", "uid-a", "session-a").unwrap();
        assert!(store.record_controlled_pre_dispatch_no_charge("req-attempted").is_err());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_core_request_with_one_exact_session_can_promote_actual_video_credits() {
        let dir = test_dir("regular-session-receipt");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-regular", "key_b").unwrap();
        store.record_core_session_attempt("req-regular", "uid-b", "session-b").unwrap();
        let receipt = BillingReceipt {
            request_id: "req-regular".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("3.250000", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-b".into()),
            task_ref: Some("video-task-b".into()),
            observed_at_ms: 1_790_000_000_000,
        };
        assert_eq!(
            store.record_core_session_receipt(&receipt, "uid-b", "session-b", "key_b").unwrap(),
            PersistReceiptResult::Created,
        );
        assert_eq!(store.get_receipt("req-regular").unwrap().unwrap().actual_credits.unwrap().to_string(), "3.250000");
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_core_receipt_rejects_wrong_key_account_and_session() {
        let dir = test_dir("regular-receipt-mismatch");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-regular", "key_b").unwrap();
        store.record_core_session_attempt("req-regular", "uid-b", "session-b").unwrap();
        let receipt = BillingReceipt {
            request_id: "req-regular".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("3.250000", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-b".into()),
            task_ref: Some("video-task-b".into()),
            observed_at_ms: 1_790_000_000_000,
        };
        assert!(store.record_core_session_receipt(&receipt, "uid-b", "session-b", "key_a").is_err());
        assert!(store.record_core_session_receipt(&receipt, "uid-a", "session-b", "key_b").is_err());
        assert!(store.record_core_session_receipt(&receipt, "uid-b", "session-a", "key_b").is_err());
        let missing = BillingReceipt { request_id: "req-missing".into(), ..receipt.clone() };
        assert!(store.record_core_session_receipt(&missing, "uid-b", "session-b", "key_b").is_err());
        let wrong_source = BillingReceipt { source_ref: Some("trae-usage-session:session-other".into()), ..receipt };
        assert!(store.record_core_session_receipt(&wrong_source, "uid-b", "session-b", "key_b").is_err());
        assert!(store.get_receipt("req-regular").unwrap().is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_core_receipt_rejects_shared_or_multiple_upstream_sessions() {
        let dir = test_dir("regular-receipt-ambiguous");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-a", "key_a").unwrap();
        store.record_core_request("req-b", "key_b").unwrap();
        store.record_core_session_attempt("req-a", "uid-a", "session-shared").unwrap();
        store.record_core_session_attempt("req-b", "uid-a", "session-shared").unwrap();
        let receipt = BillingReceipt {
            request_id: "req-a".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("3.250000", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-shared".into()),
            task_ref: Some("video-task-a".into()),
            observed_at_ms: 1_790_000_000_000,
        };
        assert!(store.record_core_session_receipt(&receipt, "uid-a", "session-shared", "key_a").is_err());
        store.record_core_request("req-c", "key_c").unwrap();
        store.record_core_session_attempt("req-c", "uid-c", "session-c1").unwrap();
        store.record_core_session_attempt("req-c", "uid-c", "session-c2").unwrap();
        let second = BillingReceipt { request_id: "req-c".into(), source_ref: Some("trae-usage-session:session-c1".into()), ..receipt };
        assert!(store.record_core_session_receipt(&second, "uid-c", "session-c1", "key_c").is_err());
        assert!(store.get_receipt("req-a").unwrap().is_none());
        assert!(store.get_receipt("req-c").unwrap().is_none());
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ordinary_chat_receipt_is_idempotent_and_conflicting_cost_stays_unsettled() {
        let dir = test_dir("regular-chat-receipt");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-chat", "key_chat").unwrap();
        store.record_core_session_attempt("req-chat", "uid-chat", "session-chat").unwrap();
        let receipt = BillingReceipt {
            request_id: "req-chat".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("0.050400", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-chat".into()),
            task_ref: None,
            observed_at_ms: 1_790_000_000_000,
        };
        assert_eq!(store.record_core_session_receipt(&receipt, "uid-chat", "session-chat", "key_chat").unwrap(), PersistReceiptResult::Created);
        assert_eq!(store.record_core_session_receipt(&receipt, "uid-chat", "session-chat", "key_chat").unwrap(), PersistReceiptResult::Duplicate);
        let conflicting = BillingReceipt { actual_credits: Some(CreditAmount::parse("0.050401", "credits").unwrap()), ..receipt };
        assert_eq!(store.record_core_session_receipt(&conflicting, "uid-chat", "session-chat", "key_chat").unwrap(), PersistReceiptResult::Conflict);
        assert_eq!(store.get_receipt("req-chat").unwrap().unwrap().status, BillingReceiptStatus::Conflict);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn settled_core_request_cannot_start_another_upstream_attempt() {
        let dir = test_dir("settled-request-no-redispatch");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-final", "key_a").unwrap();
        store.record_core_session_attempt("req-final", "uid-a", "session-a").unwrap();
        let receipt = BillingReceipt {
            request_id: "req-final".into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse("1.250000", "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some("trae-usage-session:session-a".into()),
            task_ref: None,
            observed_at_ms: 1_790_000_000_000,
        };
        store.record_core_session_receipt(&receipt, "uid-a", "session-a", "key_a").unwrap();

        assert!(store.record_core_session_attempt("req-final", "uid-a", "session-a").is_err());
        assert!(store.record_core_session_attempt("req-final", "uid-b", "session-b").is_err());
        assert!(matches!(store.core_session_for_request("req-final").unwrap(), CoreBillingSessionLookup::Unique { .. }));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upstream_session_attribution_rejects_untrusted_or_malformed_identifiers() {
        let dir = test_dir("core-session-validation");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-a", "key_a").unwrap();

        assert!(store.record_core_session_attempt("missing", "uid-1", "session-1").is_err());
        assert!(store.record_core_session_attempt("req-a", "uid/1", "session-1").is_err());
        assert!(store.record_core_session_attempt("req-a", "uid-1", "session 1").is_err());
        assert!(store.lookup_core_session_attribution("uid-1", "\n").is_err());

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upstream_attempt_payload_requires_session_only_for_authenticated_core_request() {
        let dir = test_dir("core-session-payload");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request("req-a", "key_a").unwrap();
        let attribution = CoreRequestAttribution {
            request_id: "req-a".into(),
            core_key_id: "key_a".into(),
            billing_mode: CoreBillingMode::Quoted,
            operation_id: None,
        };
        assert!(BridgeBillingStore::record_core_upstream_attempt_from_payload(
            &dir, Some(&attribution), "uid-1", &serde_json::json!({"model":"seedance"})
        ).is_err());
        assert_eq!(BridgeBillingStore::record_core_upstream_attempt_from_payload(
            &dir, None, "uid-1", &serde_json::json!({})
        ).unwrap(), None);
        assert_eq!(BridgeBillingStore::record_core_upstream_attempt_from_payload(
            &dir, Some(&attribution), "uid-1", &serde_json::json!({"session_id":"session-1"})
        ).unwrap(), Some(CoreSessionLinkResult::Created));
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn core_usage_session_id_is_stable_per_request_and_account() {
        let first = core_usage_session_id("core-req-a", "uid-a");
        assert_eq!(first, core_usage_session_id("core-req-a", "uid-a"));
        assert_ne!(first, core_usage_session_id("core-req-b", "uid-a"));
        assert_ne!(first, core_usage_session_id("core-req-a", "uid-b"));
        assert!(valid_upstream_session_id(&first));
    }

    #[test]
    fn pending_poll_accounts_include_only_unique_unsettled_core_attempts() {
        let dir = test_dir("core-pending-session-poll");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        for (request, key) in [("req-final", "key_a"), ("req-pending", "key_b"),
            ("req-conflict-a", "key_c"), ("req-conflict-b", "key_d")] {
            store.record_core_request(request, key).unwrap();
        }
        store.record_core_session_attempt("req-final", "uid-final", "session-final").unwrap();
        store.record_core_session_attempt("req-pending", "uid-pending", "session-pending").unwrap();
        store.record_core_session_attempt("req-conflict-a", "uid-conflict", "session-shared").unwrap();
        store.record_core_session_attempt("req-conflict-b", "uid-conflict", "session-shared").unwrap();
        store.record_receipt(&final_receipt("req-final", "1", "verified-source"), EvidenceTrust::VerifiedSourceContract).unwrap();

        let accounts = store.pending_core_session_accounts().unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0].0, "uid-pending");
        assert!(accounts[0].1 > 0);
        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn key_mirror_and_request_attribution_survive_restart_and_removed_keys_keep_history() {
        let dir = test_dir("core-key-restart");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.replace_core_key_registry(&CoreKeyRegistrySnapshot {
            version: 1,
            keys: vec![CoreKeyMetadata { id: "key_gone".into(), display_name: "Old name".into(), active: true }],
        }).unwrap();
        store.record_core_request("req-old", "key_gone").unwrap();
        store.replace_core_key_registry(&CoreKeyRegistrySnapshot { version: 2, keys: vec![] }).unwrap();
        drop(store);

        let reopened = BridgeBillingStore::open(&dir).unwrap();
        let rows = reopened.core_key_usage().unwrap();
        let old_key = rows.iter().find(|row| row.key_id == "key_gone").unwrap();
        assert!(!old_key.active);
        assert_eq!(old_key.display_name, "Old name");
        assert_eq!(old_key.pending_requests, 1);
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn final_receipt(request_id: &str, amount: &str, source_ref: &str) -> BillingReceipt {
        BillingReceipt {
            request_id: request_id.into(),
            status: BillingReceiptStatus::Final,
            actual_credits: Some(CreditAmount::parse(amount, "credits").unwrap()),
            unit: Some("credits".into()),
            source_ref: Some(source_ref.into()),
            task_ref: Some("video-task-1".into()),
            observed_at_ms: 1_790_000_000_000,
        }
    }

    #[test]
    fn client_cannot_supply_a_quote_amount_and_missing_upstream_quote_fails_closed() {
        let forged = serde_json::json!({
            "request_id": "request-1",
            "endpoint": "/v1/chat/completions",
            "model": "model-a",
            "request_fingerprint": "fingerprint-a",
            "max_credits": "0.000001",
        });
        assert!(serde_json::from_value::<BridgeQuoteRequest>(forged).is_err());

        let response = quote_unavailable("request-1");
        assert_eq!(response.status, "unavailable");
        assert_eq!(response.error_code, "quote_unavailable");
        assert!(response.max_credits.is_none());
    }

    #[test]
    fn unverified_candidate_is_durable_but_never_exposes_an_actual_amount() {
        let dir = test_dir("candidate");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let receipt = final_receipt("request-2", "1.25", "upstream-receipt-2");
        assert_eq!(
            store
                .record_receipt(&receipt, EvidenceTrust::CandidateOnly)
                .unwrap(),
            PersistReceiptResult::Created
        );
        drop(store);

        let reopened = BridgeBillingStore::open(&dir).unwrap();
        let stored = reopened.get_receipt("request-2").unwrap().unwrap();
        assert_eq!(stored.status, BillingReceiptStatus::Unverified);
        assert_eq!(stored.actual_credits, None);
        assert_eq!(stored.unit.as_deref(), Some("credits"));

        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn trusted_receipt_is_idempotent_and_conflicting_amounts_never_choose_a_winner() {
        let dir = test_dir("conflict");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let first = final_receipt("request-3", "2.125", "upstream-receipt-3");
        assert_eq!(
            store
                .record_receipt(&first, EvidenceTrust::VerifiedSourceContract)
                .unwrap(),
            PersistReceiptResult::Created
        );
        assert_eq!(
            store
                .record_receipt(&first, EvidenceTrust::VerifiedSourceContract)
                .unwrap(),
            PersistReceiptResult::Duplicate
        );

        let conflicting = final_receipt("request-3", "2.5", "upstream-receipt-3");
        assert_eq!(
            store
                .record_receipt(&conflicting, EvidenceTrust::VerifiedSourceContract)
                .unwrap(),
            PersistReceiptResult::Conflict
        );
        let stored = store.get_receipt("request-3").unwrap().unwrap();
        assert_eq!(stored.status, BillingReceiptStatus::Conflict);
        assert_eq!(stored.actual_credits, None);

        drop(store);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
