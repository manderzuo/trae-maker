use std::{fs, path::Path, time::Duration};

use aiwork_core::CreditAmount;
use rusqlite::Transaction;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

const BILLING_DB_FILE: &str = "bridge-billing.sqlite3";

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
pub(super) enum OneShotSessionLookup {
    Unauthorized,
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
    pub one_shot_test: bool,
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
    AuthorizedOneShotSession,
    AuthorizedOneShotChatSession,
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
    pub(super) fn open(data_dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(data_dir)
            .map_err(|error| format!("bridge billing data directory unavailable: {error}"))?;
        let path = data_dir.join(BILLING_DB_FILE);
        let connection = Connection::open(path)
            .map_err(|error| format!("bridge billing database unavailable: {error}"))?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(|error| format!("bridge billing database busy: {error}"))?;
        connection
            .execute_batch(
                "PRAGMA journal_mode = WAL;
                 PRAGMA synchronous = FULL;
                 CREATE TABLE IF NOT EXISTS bridge_billing_receipts (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   status TEXT NOT NULL CHECK(status IN ('pending','final','unknown','unverified','conflict')),
                   actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR actual_microcredits >= 0),
                   unit TEXT,
                   source_ref TEXT,
                   task_ref TEXT,
                   observed_at_ms INTEGER NOT NULL,
                   updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bridge_core_key_registry_state (
                   singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                   version INTEGER NOT NULL CHECK(version >= 0),
                   updated_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bridge_core_api_keys (
                   key_id TEXT PRIMARY KEY NOT NULL,
                   display_name TEXT NOT NULL,
                   active INTEGER NOT NULL CHECK(active IN (0, 1)),
                   snapshot_version INTEGER NOT NULL CHECK(snapshot_version >= 0)
                 );
                 CREATE TABLE IF NOT EXISTS bridge_core_requests (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   core_key_id TEXT NOT NULL,
                   associated_at_ms INTEGER NOT NULL,
                   conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1))
                 );
                 CREATE TABLE IF NOT EXISTS bridge_core_one_shot_test_requests (
                   request_id TEXT PRIMARY KEY NOT NULL,
                   authorized_at_ms INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS bridge_core_upstream_sessions (
                   request_id TEXT NOT NULL,
                   account_ref TEXT NOT NULL,
                   session_id TEXT NOT NULL,
                   conflict INTEGER NOT NULL DEFAULT 0 CHECK(conflict IN (0, 1)),
                   associated_at_ms INTEGER NOT NULL,
                   PRIMARY KEY(request_id, account_ref, session_id)
                 );
                 CREATE INDEX IF NOT EXISTS bridge_core_upstream_sessions_lookup_idx
                   ON bridge_core_upstream_sessions(account_ref, session_id);",
            )
            .map_err(|error| format!("bridge billing schema unavailable: {error}"))?;
        Ok(Self { connection })
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
        if !valid_request_id(request_id) || !valid_core_key_id(core_key_id) {
            return Err("Core request attribution identifiers are invalid".into());
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
        let existing_one_shot = if inserted == 1 {
            false
        } else {
            transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM bridge_core_one_shot_test_requests WHERE request_id = ?1)",
                params![request_id],
                |row| row.get::<_, bool>(0),
            ).map_err(|error| format!("Core one-shot request authorization lookup failed: {error}"))?
        };
        let result = if inserted == 1 {
            if one_shot_test {
                transaction.execute(
                    "INSERT INTO bridge_core_one_shot_test_requests(request_id, authorized_at_ms)
                     VALUES (?1, ?2)",
                    params![request_id, chrono::Utc::now().timestamp_millis()],
                ).map_err(|error| format!("Core one-shot request authorization insert failed: {error}"))?;
            }
            CoreRequestRecord::Created
        } else {
            let existing = transaction.query_row(
                "SELECT core_key_id FROM bridge_core_requests WHERE request_id = ?1",
                params![request_id],
                |row| row.get::<_, String>(0),
            ).map_err(|error| format!("Core request attribution lookup failed: {error}"))?;
            if existing == core_key_id && existing_one_shot == one_shot_test {
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

    pub(super) fn one_shot_session_for_request(
        &self,
        request_id: &str,
    ) -> Result<OneShotSessionLookup, String> {
        if !valid_request_id(request_id) {
            return Err("Core request attribution identifier is invalid".into());
        }
        let authorized: bool = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_core_one_shot_test_requests WHERE request_id = ?1)",
            params![request_id],
            |row| row.get(0),
        ).map_err(|error| format!("Core one-shot authorization lookup failed: {error}"))?;
        if !authorized {
            return Ok(OneShotSessionLookup::Unauthorized);
        }
        let request = self.connection.query_row(
            "SELECT core_key_id, conflict FROM bridge_core_requests WHERE request_id = ?1",
            params![request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, bool>(1)?)),
        ).optional().map_err(|error| format!("Core request lookup failed: {error}"))?;
        let Some((core_key_id, false)) = request else {
            return Ok(OneShotSessionLookup::Ambiguous);
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
            return Ok(OneShotSessionLookup::Missing);
        }
        if sessions.len() != 1 || sessions[0].2 {
            return Ok(OneShotSessionLookup::Ambiguous);
        }
        let (account_ref, session_id, _, associated_at_ms) = sessions.pop().unwrap();
        Ok(OneShotSessionLookup::Unique {
            core_key_id, account_ref, session_id, associated_at_ms,
        })
    }

    pub(super) fn record_one_shot_session_receipt(
        &mut self,
        receipt: &BillingReceipt,
        account_ref: &str,
        session_id: &str,
        core_key_id: &str,
    ) -> Result<PersistReceiptResult, String> {
        match self.one_shot_session_for_request(&receipt.request_id)? {
            OneShotSessionLookup::Unique {
                core_key_id: stored_key,
                account_ref: stored_account,
                session_id: stored_session,
                ..
            } if stored_key == core_key_id
                && stored_account == account_ref
                && stored_session == session_id => {}
            _ => return Err("one-shot billing receipt is not tied to one authorized Core session".into()),
        }
        self.record_receipt(receipt, EvidenceTrust::AuthorizedOneShotSession)
    }

    pub(super) fn record_one_shot_chat_receipt(
        &mut self,
        receipt: &BillingReceipt,
        account_ref: &str,
        session_id: &str,
        core_key_id: &str,
    ) -> Result<PersistReceiptResult, String> {
        if receipt.task_ref.is_some() {
            return Err("chat billing receipt cannot carry a video task reference".into());
        }
        match self.one_shot_session_for_request(&receipt.request_id)? {
            OneShotSessionLookup::Unique {
                core_key_id: stored_key,
                account_ref: stored_account,
                session_id: stored_session,
                ..
            } if stored_key == core_key_id
                && stored_account == account_ref
                && stored_session == session_id => {}
            _ => return Err("chat billing receipt is not tied to one authorized Core session".into()),
        }
        self.record_receipt(receipt, EvidenceTrust::AuthorizedOneShotChatSession)
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
            EvidenceTrust::AuthorizedOneShotSession => {
                if receipt.status != BillingReceiptStatus::Final
                    || receipt.unit.as_deref() != Some("credits")
                    || receipt.actual_credits.is_none()
                    || receipt.source_ref.as_deref().map_or(true, |source| !source.starts_with("trae-usage-session:"))
                    || receipt.task_ref.as_deref().map_or(true, str::is_empty)
                    || receipt.observed_at_ms <= 0
                {
                    return Err("authorized one-shot session receipt is incomplete".into());
                }
                (
                    BillingReceiptStatus::Final,
                    receipt.actual_credits.map(CreditAmount::as_microcredits),
                )
            }
            EvidenceTrust::AuthorizedOneShotChatSession => {
                if receipt.status != BillingReceiptStatus::Final
                    || receipt.unit.as_deref() != Some("credits")
                    || receipt.actual_credits.is_none()
                    || receipt.source_ref.as_deref().map_or(true, |source| !source.starts_with("trae-usage-session:"))
                    || receipt.task_ref.is_some()
                    || receipt.observed_at_ms <= 0
                {
                    return Err("authorized one-shot chat receipt is incomplete".into());
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
    fn only_authorized_one_shot_request_with_one_exact_session_can_promote_usage_to_final() {
        let dir = test_dir("one-shot-session-receipt");
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.record_core_request_with_mode("req-test", "key_a", true).unwrap();
        store.record_core_session_attempt("req-test", "uid-a", "session-a").unwrap();
        assert!(matches!(
            store.one_shot_session_for_request("req-test").unwrap(),
            OneShotSessionLookup::Unique { ref core_key_id, ref account_ref, ref session_id, .. }
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
            store.record_one_shot_session_receipt(&receipt, "uid-a", "session-a", "key_a").unwrap(),
            PersistReceiptResult::Created,
        );
        let stored = store.get_receipt("req-test").unwrap().unwrap();
        assert_eq!(stored.actual_credits.unwrap().to_string(), "245.850000");
        assert_eq!(stored.task_ref.as_deref(), Some("video-task-a"));

        store.record_core_request("req-regular", "key_b").unwrap();
        store.record_core_session_attempt("req-regular", "uid-b", "session-b").unwrap();
        assert!(store.record_one_shot_session_receipt(
            &BillingReceipt { request_id: "req-regular".into(), ..receipt.clone() },
            "uid-b", "session-b", "key_b",
        ).is_err());

        store.record_core_session_attempt("req-test", "uid-a", "session-retry").unwrap();
        assert_eq!(store.one_shot_session_for_request("req-test").unwrap(), OneShotSessionLookup::Ambiguous);
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
            one_shot_test: false,
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
