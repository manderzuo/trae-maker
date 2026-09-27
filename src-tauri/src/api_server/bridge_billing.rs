use std::{fs, path::Path, time::Duration};

use aiwork_core::CreditAmount;
use rusqlite::Transaction;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const BILLING_DB_FILE: &str = "bridge-billing.sqlite3";
const BRIDGE_SCHEMA_META_TABLE: &str = "bridge_schema_meta";
const BRIDGE_SCHEMA_VERSION: i64 = 1;

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
    connection: Connection,
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
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 PRAGMA foreign_keys = ON;",
            )
            .map_err(|error| format!("bridge billing database pragmas unavailable: {error}"))?;
        initialize_bridge_schema(&mut connection, |_| Ok(()))?;
        Ok(Self { connection })
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
            || lease.instance_id() != instance_id
            || lease.generation() != generation
        {
            return Err("recovery requires an active lease for this instance and generation".into());
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
        let next_generation = loop {
            let candidate = format!("bridge-generation-v1-{:032x}", rand::random::<u128>());
            if candidate != generation {
                break candidate;
            }
        };
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
        lease.record_generation(next_generation.clone());
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
            "SELECT s.account_ref, MIN(s.associated_at_ms)
             FROM bridge_core_upstream_sessions s
             JOIN bridge_core_requests r ON r.request_id = s.request_id
             LEFT JOIN bridge_billing_receipts b ON b.request_id = r.request_id
             WHERE s.conflict = 0 AND r.conflict = 0
               AND (b.request_id IS NULL OR b.status IN ('pending','unknown','unverified'))
             GROUP BY s.account_ref
             ORDER BY s.account_ref",
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
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| format!("bridge billing schema transaction unavailable: {error}"))?;
    let objects = bridge_schema_objects(&transaction)?;
    match objects.iter().find(|(_, name)| name == BRIDGE_SCHEMA_META_TABLE) {
        Some((kind, _)) => {
            if kind != "table" {
                return Err("bridge billing schema metadata object is not a table".into());
            }
            validate_bridge_metadata_table(&transaction)?;
            let (version, instance_id, generation) = read_bridge_schema_metadata(&transaction)?;
            if version > BRIDGE_SCHEMA_VERSION {
                return Err(format!("bridge billing schema version {version} is newer than supported"));
            }
            if version != BRIDGE_SCHEMA_VERSION {
                return Err(format!("bridge billing schema version {version} is unsupported"));
            }
            validate_bridge_schema(&transaction, true)?;
            validate_bridge_identity(&instance_id, &generation)?;
            validate_foreign_key_integrity(&transaction)?;
        }
        None if objects.is_empty() => {
            create_legacy_bridge_schema(&transaction)?;
            validate_bridge_schema(&transaction, false)?;
            validate_foreign_key_integrity(&transaction)?;
            create_bridge_schema_metadata(&transaction, &migration_hook)?;
        }
        None => {
            validate_bridge_schema(&transaction, false)?;
            validate_foreign_key_integrity(&transaction)?;
            create_bridge_schema_metadata(&transaction, &migration_hook)?;
        }
    }
    transaction
        .commit()
        .map_err(|error| format!("bridge billing schema commit failed: {error}"))?;
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
) -> Result<Vec<(String, String, i64, i64)>, String> {
    let mut statement = transaction
        .prepare(&format!("PRAGMA table_info(\"{table}\")"))
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"))?;
    let columns = statement
        .query_map([], |row| Ok((row.get(1)?, row.get(2)?, row.get(3)?, row.get(5)?)))
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| format!("bridge {table} columns unavailable: {error}"));
    columns
}

fn validate_bridge_schema(transaction: &Transaction<'_>, versioned: bool) -> Result<(), String> {
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
    let objects = bridge_schema_objects(transaction)?;
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

    let required_columns: &[(&str, &[(&str, &str)])] = &[
        ("bridge_billing_receipts", &[
            ("request_id", "TEXT"), ("status", "TEXT"), ("actual_microcredits", "INTEGER"),
            ("unit", "TEXT"), ("source_ref", "TEXT"), ("task_ref", "TEXT"),
            ("observed_at_ms", "INTEGER"), ("updated_at_ms", "INTEGER"),
        ]),
        ("bridge_core_key_registry_state", &[
            ("singleton", "INTEGER"), ("version", "INTEGER"), ("updated_at_ms", "INTEGER"),
        ]),
        ("bridge_core_api_keys", &[
            ("key_id", "TEXT"), ("display_name", "TEXT"), ("active", "INTEGER"),
            ("snapshot_version", "INTEGER"),
        ]),
        ("bridge_core_requests", &[
            ("request_id", "TEXT"), ("core_key_id", "TEXT"),
            ("associated_at_ms", "INTEGER"), ("conflict", "INTEGER"),
        ]),
        ("bridge_core_one_shot_test_requests", &[
            ("request_id", "TEXT"), ("authorized_at_ms", "INTEGER"),
        ]),
        ("bridge_core_request_modes", &[
            ("request_id", "TEXT"), ("billing_mode", "TEXT"), ("operation_id", "TEXT"),
        ]),
        ("bridge_core_upstream_sessions", &[
            ("request_id", "TEXT"), ("account_ref", "TEXT"), ("session_id", "TEXT"),
            ("conflict", "INTEGER"), ("associated_at_ms", "INTEGER"),
        ]),
    ];
    for (table, columns) in required_columns {
        let actual = bridge_table_columns(transaction, table)?;
        if actual.len() != columns.len() || columns.iter().any(|(name, kind)| {
            !actual.iter().any(|(found_name, found_kind, _, _)| {
                found_name == name && found_kind.eq_ignore_ascii_case(kind)
            })
        }) {
            return Err(format!("bridge billing {table} columns do not match the legacy layout"));
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
        if actual.iter().filter(|(_, _, _, primary)| *primary != 0).count() != keys.len()
            || keys.iter().any(|(name, rank)| {
                !actual.iter().any(|(found_name, _, _, found_rank)| {
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
    if versioned {
        validate_bridge_metadata_table(transaction)?;
    }
    Ok(())
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
        !columns.iter().any(|(found_name, found_kind, found_not_null, found_primary)| {
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
        params![BRIDGE_SCHEMA_VERSION, instance_id, generation],
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
                version == 1 && !instance_id.trim().is_empty() && !generation.trim().is_empty()
            }),
            Some(true),
            "migration should persist v1 and non-empty instance/generation identities"
        );
        assert_eq!(after, Some(before), "migration must retain every legacy row and field");
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
            *version == 1 && !instance.trim().is_empty() && !generation.trim().is_empty()
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
    fn explicit_recovery_rotates_only_after_worker_stop_confirmation() {
        use super::super::bridge_budget_lease::BridgeBudgetLease;

        let dir = test_dir("explicit-generation-recovery");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        let before = store.bridge_identity().unwrap().1;
        let mut lease = BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        let after_acquire = store.bridge_identity().unwrap().1;
        let denied = store.rotate_event_generation_for_recovery(&mut lease, || {
            Err("old charging workers are still active".into())
        });
        let after_denied = store.bridge_identity().unwrap().1;
        let rotated = store.rotate_event_generation_for_recovery(&mut lease, || Ok(()));
        let after_rotation = store.bridge_identity().unwrap().1;
        drop(lease);
        drop(store);
        let reopened = BridgeBillingStore::open(&dir).unwrap();
        let after_reopen = reopened.bridge_identity().unwrap().1;
        drop(reopened);
        std::fs::remove_dir_all(dir).unwrap();

        assert_eq!(after_acquire, before, "acquiring a lease must not rotate generation");
        assert!(denied.is_err(), "workers still active must block recovery");
        assert_eq!(after_denied, before, "failed recovery must retain the old generation");
        assert!(rotated.is_ok(), "confirmed worker shutdown should permit explicit recovery");
        assert_eq!(rotated.unwrap(), after_rotation);
        assert_ne!(after_rotation, before, "explicit recovery must mint a new generation");
        assert_eq!(after_reopen, after_rotation, "reopening must retain the recovered generation");
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
