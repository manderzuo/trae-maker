//! Startup and diagnostic orchestration only. Core owns selection, leases and quota.
use std::{collections::{BTreeMap, BTreeSet}, path::{Path, PathBuf}, sync::{Arc, Mutex}};

use aiwork_core::{ChatExecutor, CoreStore, LeaseState, ObservationReader, ObservationRequest,
    ObservationSnapshot, ObservationStatus, Principal, RegisterUpstreamAccount, UpstreamAccountState,
    UpstreamObservation};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tauri::Manager;

use super::{core_bridge::CoreMode, pool::WbSyncAccount, upstream_observation::TauriObservationReader};
use crate::models::{AccountCooldownsFile, ApiPoolFile, GroupsFile, RawAccount, RemainingCreditsFile, SchedulerStatus};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchedulerMode {
    #[default]
    Off,
    Shadow,
    Enforce,
}

impl TryFrom<&str> for SchedulerMode {
    type Error = SchedulerError;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off), "shadow" => Ok(Self::Shadow), "enforce" => Ok(Self::Enforce),
            _ => Err(SchedulerError::InvalidMode),
        }
    }
}

/// Only fixed codes cross the runtime boundary; never include adapter errors or input values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedulerError {
    InvalidMode, IncompatibleModes, NotReady, AdminRequired, StorageUnavailable,
    ReaderUnavailable, SyncFailed, RecoveryFailed, EndpointNotEnabled,
}

impl SchedulerError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidMode => "scheduler_mode_invalid",
            Self::IncompatibleModes => "scheduler_mode_incompatible",
            Self::NotReady => "scheduler_not_ready",
            Self::AdminRequired => "scheduler_admin_required",
            Self::StorageUnavailable => "scheduler_storage_unavailable",
            Self::ReaderUnavailable => "scheduler_reader_unavailable",
            Self::SyncFailed => "scheduler_sync_failed",
            Self::RecoveryFailed => "scheduler_recovery_failed",
            Self::EndpointNotEnabled => "scheduler_endpoint_not_enabled",
        }
    }
}
impl std::fmt::Display for SchedulerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str(self.code()) }
}
impl std::error::Error for SchedulerError {}

/// Core mode is the upper bound: a shadow Core can never enforce scheduling.
pub fn validate_modes(core: CoreMode, scheduler: SchedulerMode) -> Result<SchedulerMode, SchedulerError> {
    match (core, scheduler) {
        (CoreMode::Off, _) | (CoreMode::Shadow, SchedulerMode::Off) => Ok(SchedulerMode::Off),
        (CoreMode::Shadow, _) => Ok(SchedulerMode::Shadow),
        (CoreMode::Enforce, SchedulerMode::Off) => Err(SchedulerError::NotReady),
        (CoreMode::Enforce, SchedulerMode::Shadow) => Err(SchedulerError::IncompatibleModes),
        (CoreMode::Enforce, SchedulerMode::Enforce) => Ok(SchedulerMode::Enforce),
    }
}

pub fn authenticate_sync_admin(store: &CoreStore, key: Option<&str>) -> Result<Principal, SchedulerError> {
    let principal = store.authenticate_api_key(key.ok_or(SchedulerError::AdminRequired)?)
        .map_err(|_| SchedulerError::AdminRequired)?;
    store.authorize_admin_principal(&principal).map_err(|_| SchedulerError::AdminRequired)?;
    Ok(principal)
}

pub type ObservationRegistry = BTreeMap<String, Arc<dyn ObservationReader>>;
// Task 5 supplies lease-aware dispatch. Merely registering a Phase 1 port does not authorize execution.
pub type ExecutorRegistry = BTreeMap<String, Arc<dyn ChatExecutor>>;

#[derive(Debug, Serialize)]
pub struct ShadowCandidate {
    pub account_hash: String,
    pub provider: &'static str,
    pub enabled: bool,
    pub state: &'static str,
    pub chat_confirmed: bool,
    pub observation: &'static str,
}

pub struct SchedulerRuntime {
    pub store: Arc<CoreStore>,
    pub mode: SchedulerMode,
    pub readers: ObservationRegistry,
    pub executors: ExecutorRegistry,
    data_dir: PathBuf,
    status: Mutex<SchedulerStatus>,
}

impl SchedulerRuntime {
    pub fn new(store: Arc<CoreStore>, data_dir: PathBuf, mode: SchedulerMode,
        readers: ObservationRegistry, executors: ExecutorRegistry, now_ms: i64) -> Result<Self, SchedulerError> {
        let recovered = if mode == SchedulerMode::Enforce {
            // Core returns the entire recoverable set, including pre-existing unknown leases.
            let already_unknown: BTreeSet<_> = store.list_recoverable_leases()
                .map_err(|_| SchedulerError::RecoveryFailed)?.into_iter()
                .filter(|lease| lease.state == LeaseState::Unknown).map(|lease| lease.id).collect();
            store.recover_expired_upstream_leases(now_ms).map_err(|_| SchedulerError::RecoveryFailed)?
                .iter().filter(|lease| lease.state == LeaseState::Unknown && !already_unknown.contains(&lease.id)).count() as u64
        } else { 0 };
        Ok(Self {
            store, mode, readers, executors, data_dir,
            status: Mutex::new(SchedulerStatus { mode, ready: mode != SchedulerMode::Off,
                recovered_leases: recovered, ..Default::default() }),
        })
    }

    pub fn note_error(&self, error: SchedulerError) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.ready = false;
        status.last_error = Some(error.code().into());
    }

    /// Deliberately no endpoint is enabled in Task 4. Task 5 must add atomic lease dispatch.
    pub fn require_endpoint(&self, _endpoint: &str) -> Result<(), SchedulerError> {
        if self.mode != SchedulerMode::Enforce { return Err(SchedulerError::NotReady); }
        Err(SchedulerError::EndpointNotEnabled)
    }

    pub fn scheduler_status(&self) -> SchedulerStatus {
        self.scheduler_status_at(chrono::Utc::now().timestamp_millis())
    }

    fn scheduler_status_at(&self, now_ms: i64) -> SchedulerStatus {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if self.mode != SchedulerMode::Off {
            match self.store.scheduler_status_counts(now_ms) {
                Ok(counts) => merge_core_status_counts(&mut status, &counts),
                Err(_) => {
                    status.ready = false;
                    status.last_error = Some(SchedulerError::StorageUnavailable.code().into());
                }
            }
        }
        status
    }

    /// Administrator-only diagnostic projection. It contains aggregate Core
    /// counts and runtime flags, never account credentials, user grants or
    /// another user's request/quota rows.
    pub fn scheduler_status_for_admin(
        &self,
        principal: &Principal,
        now_ms: i64,
    ) -> Result<Value, SchedulerError> {
        let mut payload = self
            .store
            .scheduler_status_for_admin(principal, now_ms)
            .map_err(|error| match error {
                aiwork_core::CoreError::AdminRequired => SchedulerError::AdminRequired,
                _ => SchedulerError::StorageUnavailable,
            })?;
        let runtime = serde_json::to_value(self.scheduler_status())
            .map_err(|_| SchedulerError::StorageUnavailable)?;
        if let (Some(target), Some(source)) = (payload.as_object_mut(), runtime.as_object()) {
            for (key, value) in source {
                target.insert(key.clone(), value.clone());
            }
        }
        Ok(payload)
    }

    /// Administrator-facing inventory for a resource, with no ranking or eligibility decision.
    /// Hashes and fixed categories also protect older Core rows containing non-opaque identifiers.
    pub fn shadow_candidates(&self, resource_kind: &str, now_ms: i64) -> Vec<ShadowCandidate> {
        if self.mode != SchedulerMode::Shadow { return Vec::new(); }
        let accounts = match read_directory(&self.data_dir) {
            Ok(accounts) => accounts,
            Err(error) => { self.note_error(error); return Vec::new(); }
        };
        let mut candidates = Vec::new();
        for account in accounts.values() {
            let observation = match self.store.get_latest_observation(&account.id, resource_kind) {
                Ok(Some(o)) if o.status == ObservationStatus::Fresh && o.source == "reader"
                    && o.observed_at_ms <= now_ms && o.stale_at_ms > now_ms => "fresh",
                Ok(Some(o)) if o.status == ObservationStatus::Failed => "failed",
                Ok(Some(_)) => "stale",
                Ok(None) => "missing",
                Err(_) => { self.note_error(SchedulerError::StorageUnavailable); "unavailable" }
            };
            candidates.push(ShadowCandidate {
                account_hash: Sha256::digest(account.id.as_bytes()).iter().map(|b| format!("{b:02x}")).collect(),
                provider: match account.provider.as_str() { "trae" => "trae", "workbuddy" => "workbuddy", "mock" => "mock", _ => "other" },
                enabled: account.enabled, state: account.state.as_str(),
                chat_confirmed: account.capabilities.contains("chat"), observation,
            });
        }
        if let Ok(json) = serde_json::to_string(&candidates) {
            crate::fs_utils::app_log(&self.data_dir, &format!("scheduler.shadow_candidates {json}"));
        }
        candidates
    }

    /// Inventory/freshness diagnostics, NOT a duplicate selector or authorization to dispatch.
    /// Errors become diagnostics; shadow callers never need to block a legacy request.
    pub fn shadow_dry_run(&self, now_ms: i64) -> SchedulerStatus {
        if self.mode != SchedulerMode::Shadow { return self.scheduler_status(); }
        let result = diagnostic_counts(&self.data_dir, now_ms);
        {
            let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
            status.dry_runs += 1;
            match result {
                Ok((accounts, enabled, fresh, stale)) => {
                    status.accounts = accounts; status.enabled_accounts = enabled;
                    status.fresh_observations = fresh; status.stale_observations = stale;
                }
                Err(error) => { status.ready = false; status.last_error = Some(error.code().into()); }
            }
        }
        // Use the same injected clock as the diagnostic query. Calling the
        // public wall-clock status here would immediately relabel fixed-time
        // fixture observations as stale and hide the shadow result.
        let status = self.scheduler_status_at(now_ms);
        if let Ok(json) = serde_json::to_string(&status) {
            crate::fs_utils::app_log(&self.data_dir, &format!("scheduler.shadow_dry_run {json}"));
        }
        status
    }

    /// Explicit refresh only; startup never invokes network readers or any executor.
    pub fn refresh_observation(&self, request: ObservationRequest, now_ms: i64) -> Result<(), SchedulerError> {
        if self.mode == SchedulerMode::Off { return Err(SchedulerError::NotReady); }
        let account_ref = request.account_ref.clone();
        let resource_kind = request.resource_kind.clone();
        let accounts = read_directory(&self.data_dir)?;
        let account = accounts.get(&request.account_ref).filter(|a| a.provider == request.provider)
            .ok_or(SchedulerError::ReaderUnavailable)?;
        let result = self.readers.get(&account.provider).ok_or(SchedulerError::ReaderUnavailable)
            .and_then(|reader| reader.read(request.clone()).map_err(|_| SchedulerError::ReaderUnavailable))
            .and_then(|snapshot| {
                if snapshot.account_ref != request.account_ref || snapshot.resource_kind != request.resource_kind {
                    return Err(SchedulerError::ReaderUnavailable);
                }
                self.store.record_observation_snapshot(snapshot).map(|_| ()).map_err(|_| SchedulerError::ReaderUnavailable)
            });
        if result.is_ok() {
            let observation_id = self
                .store
                .get_latest_observation(&account_ref, &resource_kind)
                .map_err(|_| SchedulerError::StorageUnavailable)?
                .map(|observation| observation.id);
            self.store
                .record_upstream_health_transition(
                    &account_ref,
                    None,
                    None,
                    &resource_kind,
                    observation_id.as_deref(),
                    "success",
                    now_ms,
                )
                .map_err(|_| SchedulerError::StorageUnavailable)?;
        } else {
            self.status.lock().unwrap_or_else(|e| e.into_inner()).reader_failures += 1;
            let previous = self.store.get_latest_observation(&account_ref, &resource_kind)
                .map_err(|_| SchedulerError::StorageUnavailable)?;
            let scale = previous.as_ref().map(|o| o.value_scale).unwrap_or(1);
            // Core sorts equal timestamps by ID. A failed refresh must supersede the
            // previous sample even with an injected/frozen clock or clock rollback.
            let failed_at = match previous {
                Some(previous) => now_ms.max(previous.observed_at_ms.checked_add(1).ok_or(SchedulerError::StorageUnavailable)?),
                None => now_ms,
            };
            // Core preserves the last successful value on a Failed row; a failure is never zero credit.
            let failed_id = format!("observation_{:032x}", rand::random::<u128>());
            self.store.append_upstream_observation(UpstreamObservation::new(
                failed_id.clone(), account_ref.clone(), resource_kind.clone(),
                None, scale, "reader".into(), ObservationStatus::Failed, failed_at, failed_at,
                serde_json::json!({"status":"failed", "reason":"reader_unavailable"}),
            )).map_err(|_| SchedulerError::StorageUnavailable)?;
            self.store
                .record_upstream_health_transition(
                    &account_ref,
                    None,
                    None,
                    &resource_kind,
                    Some(&failed_id),
                    "reader_failure",
                    now_ms,
                )
                .map_err(|_| SchedulerError::StorageUnavailable)?;
        }
        result
    }
}

fn merge_core_status_counts(status: &mut SchedulerStatus, counts: &Value) {
    let count = |key: &str| counts.get(key).and_then(Value::as_u64);
    if let Some(value) = count("accounts") { status.accounts = value; }
    if let Some(value) = count("enabled_accounts") { status.enabled_accounts = value; }
    if let Some(value) = count("fresh_observations") { status.fresh_observations = value; }
    if let Some(value) = count("stale_observations") { status.stale_observations = value; }
    if let Some(value) = count("active_leases") { status.active_leases = value; }
    if let Some(value) = count("unknown_leases") { status.unknown_leases = value; }
    if let Some(value) = count("reader_failures") {
        // Keep failures observed in the current runtime even when the
        // diagnostic row could not be persisted (for example, an unavailable
        // reader before an observation can be appended). The durable count is
        // still included once it is available.
        status.reader_failures = status.reader_failures.max(value);
    }
}

/// Secret-free projection of the legacy directory. Legacy IDs stay in the adapter map only.
pub struct AccountDirectory {
    accounts: Vec<RegisterUpstreamAccount>,
    cached_observations: Vec<ObservationSnapshot>,
    adapter_ids: BTreeMap<String, String>,
}

impl AccountDirectory {
    #[allow(clippy::too_many_arguments)]
    pub fn from_legacy(trae: &[RawAccount], wb: &[WbSyncAccount], pool: &ApiPoolFile,
        groups: &GroupsFile, cooldowns: &AccountCooldownsFile, credits: &RemainingCreditsFile,
        now_ms: i64) -> Result<Self, SchedulerError> {
        let mut directory = Self { accounts: Vec::new(), cached_observations: Vec::new(), adapter_ids: BTreeMap::new() };
        for account in trae {
            let Some(uid) = account.user_id.as_deref().filter(|uid| !uid.is_empty()) else { continue; };
            let in_group = pool.group_ids.is_empty() || groups.membership.get(uid).is_some_and(|g| pool.group_ids.contains(g));
            let enabled = pool.enabled_uids.iter().any(|id| id == uid) && in_group && !account.jwt.trim().is_empty();
            let input = legacy_account("trae", uid, enabled, Some("cn"), cooldowns.cooldowns.get(uid), now_ms);
            for (resource, value) in [
                ("chat.general", credits.general.get(uid).or_else(|| credits.credits.get(uid))),
                ("chat.work", credits.work.get(uid)),
            ] {
                if let Some(value) = value {
                    let mut snapshot = TauriObservationReader::snapshot_from_trae_values(&input.id, resource, *value, 0, 0)
                        .map_err(|_| SchedulerError::SyncFailed)?;
                    snapshot.source = "json_cache".into();
                    snapshot.summary["source"] = serde_json::json!("json_cache");
                    snapshot.summary["status"] = serde_json::json!("stale");
                    directory.cached_observations.push(snapshot);
                }
            }
            directory.adapter_ids.insert(input.id.clone(), uid.into());
            directory.accounts.push(input);
        }
        for account in wb {
            if account.uid.is_empty() { continue; }
            let enabled = pool.wb_enabled && pool.enabled_uids.contains(&account.uid) && !account.token.trim().is_empty();
            // The legacy adapter confirms global routing; an unknown/local domain proves no region.
            let region = if account.global_region { Some("global") } else { None };
            let mut input = legacy_account("workbuddy", &account.uid, enabled, region,
                cooldowns.cooldowns.get(&account.uid), now_ms);
            if account.needs_relogin {
                input.enabled = false; input.state = UpstreamAccountState::Disabled;
                input.cooldown_reason = Some("session_dead".into());
            }
            if account.credits.is_some() {
                directory.cached_observations.push(TauriObservationReader::snapshot_from_workbuddy_cache(
                    &input.id, "chat.work", account.credits, 0, region,
                ).map_err(|_| SchedulerError::SyncFailed)?);
            }
            directory.adapter_ids.insert(input.id.clone(), account.uid.clone());
            directory.accounts.push(input);
        }
        Ok(directory)
    }

    pub fn readers(&self, app: &tauri::AppHandle) -> ObservationRegistry {
        let reader: Arc<dyn ObservationReader> = Arc::new(RegisteredTauriReader { app: app.clone(), ids: self.adapter_ids.clone() });
        BTreeMap::from([("trae".into(), reader.clone()), ("workbuddy".into(), reader)])
    }
}

struct RegisteredTauriReader { app: tauri::AppHandle, ids: BTreeMap<String, String> }
impl ObservationReader for RegisteredTauriReader {
    fn read(&self, mut request: ObservationRequest) -> Result<ObservationSnapshot, aiwork_core::ObservationError> {
        let opaque = request.account_ref.clone();
        request.account_ref = self.ids.get(&opaque).cloned().ok_or(aiwork_core::ObservationError::Unavailable)?;
        let state = self.app.state::<crate::state::AppState>();
        let mut snapshot = TauriObservationReader::new(&state, chrono::Utc::now().timestamp_millis()).read(request)?;
        snapshot.account_ref = opaque;
        Ok(snapshot)
    }
}

fn legacy_account(provider: &str, uid: &str, enabled: bool, region: Option<&str>,
    cooldown: Option<&crate::models::CooldownEntry>, now_ms: i64) -> RegisterUpstreamAccount {
    let digest = Sha256::digest(format!("aiwork-scheduler-v1\0{provider}\0{uid}").as_bytes());
    let digest = digest.iter().map(|b| format!("{b:02x}")).collect::<String>();
    let mut input = RegisterUpstreamAccount::new(format!("acct_{digest}"), provider.into(), format!("vault://scheduler/{provider}/{digest}"));
    input.region = region.map(str::to_owned);
    input.capabilities = BTreeSet::from(["chat".into()]);
    input.enabled = enabled;
    if !enabled { input.state = UpstreamAccountState::Disabled; input.cooldown_reason = Some("directory_disabled".into()); }
    if let Some(cd) = cooldown {
        input.consecutive_errors = i64::from(cd.error_count.max(0));
        match cd.error_type.as_str() {
            "SessionDead" | "Forbidden" => {
                input.enabled = false;
                input.state = if cd.error_type == "Forbidden" { UpstreamAccountState::Forbidden } else { UpstreamAccountState::Disabled };
                input.cooldown_reason = Some(if cd.error_type == "Forbidden" { "forbidden" } else { "session_dead" }.into());
            }
            "HardCredit" => { input.state = UpstreamAccountState::Cooling; input.cooldown_reason = Some("hard_credit".into()); }
            _ if cd.until.saturating_mul(1000) > now_ms => {
                input.state = UpstreamAccountState::Cooling;
                input.cooldown_until_ms = Some(cd.until.saturating_mul(1000));
                input.cooldown_reason = Some("legacy_cooldown".into());
            }
            _ => {}
        }
    }
    input
}

/// Only startup-owned directory metadata is reconciled. All writes use Core's validated APIs.
pub fn sync_upstream_accounts(store: &CoreStore, data_dir: &Path, principal: &Principal,
    directory: &AccountDirectory) -> Result<usize, SchedulerError> {
    store.authorize_admin_principal(principal).map_err(|_| SchedulerError::AdminRequired)?;
    let mut previous = read_directory(data_dir)?;
    let desired: BTreeSet<_> = directory.accounts.iter().map(|a| a.id.clone()).collect();
    let mut updates = directory.accounts.clone();
    for old in previous.values().filter(|a| a.credentials_ref.starts_with("vault://scheduler/") && !desired.contains(&a.id)) {
        let mut removed = old.clone(); removed.enabled = false;
        updates.push(removed);
    }
    let mut changed = 0;
    for mut input in updates {
        if let Some(old) = previous.get(&input.id) {
            input.max_concurrency = old.max_concurrency;
            // JSON startup must never clear an authoritative Core health decision.
            if old.state != UpstreamAccountState::Available && old.cooldown_reason.as_deref() != Some("directory_disabled") {
                if !matches!(input.state, UpstreamAccountState::Forbidden | UpstreamAccountState::Disabled) || old.state == UpstreamAccountState::Forbidden {
                    input.state = old.state;
                    input.cooldown_reason = old.cooldown_reason.clone();
                }
            }
            // Available is a routing state, not evidence that persisted health
            // counters or cooldown metadata have been reset by Core.
            input.consecutive_errors = input.consecutive_errors.max(old.consecutive_errors);
            if old.cooldown_until_ms > input.cooldown_until_ms {
                input.cooldown_until_ms = old.cooldown_until_ms;
                input.cooldown_reason = old.cooldown_reason.clone();
            } else if input.cooldown_reason.is_none()
                && old.cooldown_reason.as_deref() != Some("directory_disabled") {
                input.cooldown_reason = old.cooldown_reason.clone();
            }
            if *old == input { continue; }
        }
        store.upsert_upstream_account(input.clone(), principal).map_err(|_| SchedulerError::SyncFailed)?;
        previous.insert(input.id.clone(), input);
        changed += 1;
    }
    for snapshot in &directory.cached_observations {
        // One-time legacy import, never overwrite or supersede an existing reader observation.
        if store.get_latest_observation(&snapshot.account_ref, &snapshot.resource_kind).map_err(|_| SchedulerError::SyncFailed)?.is_none() {
            store.record_observation_snapshot(snapshot.clone()).map_err(|_| SchedulerError::SyncFailed)?;
        }
    }
    Ok(changed)
}

fn read_connection(data_dir: &Path) -> Result<rusqlite::Connection, SchedulerError> {
    let connection = rusqlite::Connection::open_with_flags(data_dir.join("data").join(aiwork_core::CORE_DB_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_| SchedulerError::StorageUnavailable)?;
    connection.busy_timeout(std::time::Duration::from_millis(100)).map_err(|_| SchedulerError::StorageUnavailable)?;
    Ok(connection)
}

// Core currently has no directory listing API. Read-only projection; no scheduler SQL or writes.
fn read_directory(data_dir: &Path) -> Result<BTreeMap<String, RegisterUpstreamAccount>, SchedulerError> {
    let connection = read_connection(data_dir)?;
    let mut statement = connection.prepare("SELECT id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency, state, cooldown_until_ms, cooldown_reason, consecutive_errors FROM upstream_accounts")
        .map_err(|_| SchedulerError::StorageUnavailable)?;
    let rows = statement.query_map([], |row| {
        let state: String = row.get(7)?;
        let state = match state.as_str() {
            "available" => UpstreamAccountState::Available, "cooling" => UpstreamAccountState::Cooling,
            "forbidden" => UpstreamAccountState::Forbidden, _ => UpstreamAccountState::Disabled,
        };
        let capabilities: String = row.get(4)?;
        let capabilities = serde_json::from_str(&capabilities).map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
        Ok(RegisterUpstreamAccount { id: row.get(0)?, provider: row.get(1)?, credentials_ref: row.get(2)?, region: row.get(3)?, capabilities,
            enabled: row.get(5)?, max_concurrency: row.get(6)?, state, cooldown_until_ms: row.get(8)?, cooldown_reason: row.get(9)?, consecutive_errors: row.get(10)? })
    }).map_err(|_| SchedulerError::StorageUnavailable)?;
    let mut accounts = BTreeMap::new();
    for row in rows { let account = row.map_err(|_| SchedulerError::StorageUnavailable)?; accounts.insert(account.id.clone(), account); }
    Ok(accounts)
}

fn diagnostic_counts(data_dir: &Path, now_ms: i64) -> Result<(u64, u64, u64, u64), SchedulerError> {
    let connection = read_connection(data_dir)?;
    let (accounts, enabled) = connection.query_row("SELECT count(*), coalesce(sum(enabled),0) FROM upstream_accounts", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|_| SchedulerError::StorageUnavailable)?;
    let mut statement = connection.prepare("SELECT source, status, observed_at_ms, stale_at_ms FROM upstream_observations o WHERE NOT EXISTS (SELECT 1 FROM upstream_observations n WHERE n.account_ref=o.account_ref AND n.resource_kind=o.resource_kind AND (n.observed_at_ms>o.observed_at_ms OR (n.observed_at_ms=o.observed_at_ms AND n.id>o.id)))")
        .map_err(|_| SchedulerError::StorageUnavailable)?;
    let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)))
        .map_err(|_| SchedulerError::StorageUnavailable)?;
    let (mut fresh, mut stale) = (0, 0);
    for row in rows {
        let (source, status, observed, expires) = row.map_err(|_| SchedulerError::StorageUnavailable)?;
        if source == "reader" && status == "fresh" && observed <= now_ms && expires > now_ms { fresh += 1; } else { stale += 1; }
    }
    Ok((accounts, enabled, fresh, stale))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiwork_core::{NewUser, UserRole, MockObservationReader, MockChatExecutor, ObservationStatus};
    use serde_json::json;

    const NOW: i64 = 1_725_000_000_000;

    struct Fixture {
        store: Arc<CoreStore>,
        admin: Principal,
        admin_key: String,
        dir: std::path::PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = std::path::PathBuf::from(r"D:\gpt")
                .join(format!("aiwork-scheduler-task6-{}", rand::random::<u64>()));
            let _ = std::fs::remove_dir_all(&dir);
            let store = Arc::new(CoreStore::open(&dir).unwrap());
            store.migrate().unwrap();
            store.create_bootstrap_admin(NewUser { id: "admin".into(), name: "Fixture".into(), role: UserRole::Admin }, "bootstrap").unwrap();
            let key = store.issue_api_key("admin", "fixture", BTreeSet::from(["chat:invoke".into()]), "admin").unwrap();
            let admin = store.authenticate_api_key(&key.plaintext).unwrap();
            Self { store, admin, admin_key: key.plaintext, dir }
        }
        fn runtime(&self, mode: SchedulerMode) -> SchedulerRuntime {
            SchedulerRuntime::new(self.store.clone(), self.dir.clone(), mode, BTreeMap::new(), BTreeMap::new(), NOW).unwrap()
        }
    }

    fn directory() -> AccountDirectory {
        let trae: crate::models::RawAccount = serde_json::from_value(json!({
            "UserID":"fixture-user-sensitive", "name":"private name", "jwt":"fixture-jwt-secret", "refresh_token":"fixture-refresh-secret"
        })).unwrap();
        let wb = super::super::pool::WbSyncAccount {
            uid: "fixture-wb-sensitive".into(), name: "private wb name".into(), token: "fixture-wb-secret".into(),
            domain: "fixture.invalid".into(), enterprise_id: "private-enterprise".into(), global_region: false,
            credits: Some(6.25), needs_relogin: false,
        };
        let pool = crate::models::ApiPoolFile {
            enabled_uids: vec!["fixture-user-sensitive".into(), "fixture-wb-sensitive".into()],
            wb_enabled: true, ..Default::default()
        };
        let credits: crate::models::RemainingCreditsFile = serde_json::from_value(json!({"general":{"fixture-user-sensitive":12.5}, "work":{"fixture-user-sensitive":3.0}})).unwrap();
        AccountDirectory::from_legacy(&[trae], &[wb], &pool, &Default::default(), &Default::default(), &credits, NOW).unwrap()
    }

    #[test]
    fn sync_writes_opaque_account_refs_without_credentials_or_user_grants() {
        let f = Fixture::new();
        let directory = directory();
        let first = sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        assert_eq!(first, 2);
        let audit_before = f.store.count_rows("audit_events").unwrap();
        assert_eq!(sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap(), 0);
        assert_eq!(f.store.count_rows("audit_events").unwrap(), audit_before);
        assert_eq!(f.store.count_rows("upstream_accounts").unwrap(), 2);
        assert_eq!(f.store.count_rows("upstream_observations").unwrap(), 3);
        assert_eq!(f.store.balance("admin", "chat.general").unwrap().available, 0);
        assert_eq!(f.store.count_rows("quota_ledger").unwrap(), 0);
        let rows = read_directory(&f.dir).unwrap();
        assert!(rows.values().all(|a| a.enabled && a.max_concurrency == 1 && a.capabilities == BTreeSet::from(["chat".into()])));
        let text = format!("{rows:?}");
        for secret in ["fixture-user-sensitive", "fixture-wb-sensitive", "fixture-jwt-secret", "fixture-refresh-secret", "fixture-wb-secret", "private"] {
            assert!(!text.contains(secret));
        }
        for observation in &directory.cached_observations {
            let stored = f.store.get_latest_observation(&observation.account_ref, &observation.resource_kind).unwrap().unwrap();
            assert_eq!(stored.source, "json_cache");
            assert_eq!(stored.status, ObservationStatus::Stale);
            assert_eq!(stored.stale_at_ms, 0);
        }
        let connection = rusqlite::Connection::open(f.dir.join("data").join(aiwork_core::CORE_DB_FILE)).unwrap();
        let audit: String = connection.query_row("SELECT group_concat(metadata_json) FROM audit_events", [], |r| r.get(0)).unwrap();
        assert!(!audit.contains("vault://"));
        assert!(!audit.contains("fixture-user-sensitive"));
    }

    #[test]
    fn scheduler_sync_preserves_core_health_and_disables_removed_accounts() {
        let f = Fixture::new();
        let mut directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let mut existing = directory.accounts[0].clone();
        existing.state = UpstreamAccountState::Forbidden;
        existing.consecutive_errors = 4;
        f.store.upsert_upstream_account(existing.clone(), &f.admin).unwrap();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        assert_eq!(read_directory(&f.dir).unwrap()[&existing.id].state, UpstreamAccountState::Forbidden);
        directory.accounts.clear();
        directory.cached_observations.clear();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        assert!(read_directory(&f.dir).unwrap().values().all(|a| !a.enabled));
    }

    #[test]
    fn scheduler_sync_rejects_unvalidated_admin_without_writes() {
        let f = Fixture::new();
        let mut forged = f.admin.clone();
        forged.key_id = "absent".into();
        assert!(sync_upstream_accounts(&f.store, &f.dir, &forged, &directory()).is_err());
        assert_eq!(f.store.count_rows("upstream_accounts").unwrap(), 0);
    }

    #[test]
    fn shadow_records_dry_run_but_enforce_rejects_missing_scheduler() {
        let f = Fixture::new();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory()).unwrap();
        let runtime = f.runtime(SchedulerMode::Shadow);
        let diagnostics = runtime.shadow_dry_run(NOW);
        assert_eq!(diagnostics.stale_observations, 3);
        assert_eq!(runtime.scheduler_status().dry_runs, 1);
        assert_eq!(f.store.count_rows("requests").unwrap(), 0);
        assert_eq!(f.store.count_rows("upstream_leases").unwrap(), 0);
        assert_eq!(f.store.count_rows("quota_ledger").unwrap(), 0);
        let bridge = crate::api_server::CoreBridge::new(f.store.clone(), CoreMode::Enforce);
        assert_eq!(bridge.scheduler().err().unwrap().code(), "scheduler_not_ready");
    }

    #[test]
    fn scheduler_mode_compatibility_matrix_and_startup_authorization() {
        for core in [CoreMode::Off, CoreMode::Shadow, CoreMode::Enforce] {
            for scheduler in [SchedulerMode::Off, SchedulerMode::Shadow, SchedulerMode::Enforce] {
                let result = validate_modes(core, scheduler);
                assert_eq!(result.is_err(), core == CoreMode::Enforce && scheduler != SchedulerMode::Enforce);
                if core == CoreMode::Off { assert_eq!(result.unwrap(), SchedulerMode::Off); }
                if core == CoreMode::Shadow && scheduler != SchedulerMode::Off { assert_eq!(result.unwrap(), SchedulerMode::Shadow); }
            }
        }
        let f = Fixture::new();
        assert_eq!(authenticate_sync_admin(&f.store, None).unwrap_err().code(), "scheduler_admin_required");
        assert_eq!(authenticate_sync_admin(&f.store, Some("untrusted-secret")).unwrap_err().code(), "scheduler_admin_required");
        assert_eq!(authenticate_sync_admin(&f.store, Some(&f.admin_key)).unwrap().user_id, "admin");
    }

    #[test]
    fn scheduler_mock_registry_records_snapshot_and_failure_without_execution() {
        let f = Fixture::new();
        let directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let account = &directory.accounts[0];
        let request = ObservationRequest { account_ref: account.id.clone(), provider: "trae".into(), resource_kind: "chat.general".into() };
        let snapshot = ObservationSnapshot {
            account_ref: account.id.clone(), resource_kind: "chat.general".into(), available_units: Some(1250), value_scale: 100,
            source: "reader".into(), observed_at_ms: NOW, stale_at_ms: NOW + 60000,
            capabilities: vec!["chat".into()], region: Some("cn".into()), summary: json!({"available":1250}),
        };
        let reader = MockObservationReader::new(std::collections::HashMap::from([((account.id.clone(), "chat.general".into()), snapshot)]));
        let executor = Arc::new(MockChatExecutor::ok());
        let mut readers: ObservationRegistry = BTreeMap::new();
        readers.insert("trae".into(), Arc::new(reader));
        let mut executors: ExecutorRegistry = BTreeMap::new();
        executors.insert("trae".into(), executor.clone());
        let runtime = SchedulerRuntime::new(f.store.clone(), f.dir.clone(), SchedulerMode::Shadow, readers, executors, NOW).unwrap();
        runtime.refresh_observation(request.clone(), NOW).unwrap();
        assert_eq!(runtime.shadow_dry_run(NOW).fresh_observations, 1);
        let mut missing = request;
        missing.resource_kind = "chat.work".into();
        assert!(runtime.refresh_observation(missing, NOW + 1).is_err());
        assert_eq!(runtime.scheduler_status().reader_failures, 1);
        assert!(executor.calls().is_empty());
        assert_eq!(f.store.count_rows("quota_ledger").unwrap(), 0);
    }

    #[test]
    fn scheduler_startup_recovers_expired_lease_without_releasing_quota() {
        use aiwork_core::{BeginRequestInput, CostPolicy, PreflightReserveInput, QuotaGrant, SchedulerLeaseRequest, SchedulerLeaseResult, SelectionStrategy, LeaseState};
        let f = Fixture::new();
        let directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        f.store.upsert_cost_policy(CostPolicy { id: "fixture-policy".into(), endpoint: "chat".into(), model_pattern: "mock-*".into(), resource_kind: "chat.general".into(), reserve_amount: 1, max_actual_amount: Some(1), version: 1, enabled: true }).unwrap();
        f.store.grant(QuotaGrant { user_id: "admin".into(), resource_kind: "chat.general".into(), amount: 5, actor_user_id: "admin".into(), reason: "local fixture grant".into() }).unwrap();
        f.store.record_observation_snapshot(ObservationSnapshot {
            account_ref: directory.accounts[0].id.clone(), resource_kind: "chat.general".into(), available_units: Some(1000), value_scale: 100,
            source: "reader".into(), observed_at_ms: NOW, stale_at_ms: NOW + 60000, capabilities: vec!["chat".into()], region: Some("cn".into()), summary: json!({"available":1000}),
        }).unwrap();
        let acquired = f.store.preflight_reserve_with_lease(&f.admin, SchedulerLeaseRequest {
            preflight: PreflightReserveInput { request: BeginRequestInput { user_id: "admin".into(), api_key_id: f.admin.key_id.clone(), protocol: "openai".into(), endpoint: "chat".into(), model: "mock-1".into(), idempotency_key: "fixture-recovery".into(), body: json!({"model":"mock-1"}) }, resource_kind: "chat.general".into(), amount: 1, ttl_ms: 30000 },
            provider_hint: Some("trae".into()), required_capabilities: vec!["chat".into()], region: Some("cn".into()), predicted_units: 1, safety_margin_units: 0, observation_max_age_ms: 60000, allowed_accounts: None, dedicated_account: None, selection_strategy: SelectionStrategy::LeastActiveSlots, now_ms: NOW, lease_ttl_ms: 10, reconcile_ttl_ms: 60000,
        }).unwrap();
        assert!(matches!(acquired, SchedulerLeaseResult::Acquired(_)));
        let reopened = Arc::new(CoreStore::open(&f.dir).unwrap());
        let shadow = SchedulerRuntime::new(reopened.clone(), f.dir.clone(), SchedulerMode::Shadow, BTreeMap::new(), BTreeMap::new(), NOW + 11).unwrap();
        assert_eq!(shadow.scheduler_status().recovered_leases, 0);
        assert_eq!(f.store.list_recoverable_leases().unwrap()[0].state, LeaseState::Held);
        let runtime = SchedulerRuntime::new(reopened.clone(), f.dir.clone(), SchedulerMode::Enforce, BTreeMap::new(), BTreeMap::new(), NOW + 11).unwrap();
        assert_eq!(runtime.scheduler_status().recovered_leases, 1);
        assert_eq!(runtime.scheduler_status().unknown_leases, 1);
        assert_eq!(f.store.balance("admin", "chat.general").unwrap().held, 1);
        let again = SchedulerRuntime::new(reopened, f.dir.clone(), SchedulerMode::Enforce, BTreeMap::new(), BTreeMap::new(), NOW + 60001).unwrap();
        assert_eq!(again.scheduler_status().recovered_leases, 0);
        assert_eq!(f.store.balance("admin", "chat.general").unwrap().held, 1);
        assert_eq!(runtime.require_endpoint("chat").unwrap_err().code(), "scheduler_endpoint_not_enabled");
    }

    #[test]
    fn scheduler_legacy_filters_disable_unconfirmed_or_excluded_accounts() {
        let mut pool = crate::models::ApiPoolFile { enabled_uids: vec!["fixture-a".into()], group_ids: vec!["allowed".into()], ..Default::default() };
        let accounts = vec![serde_json::from_value(json!({"UserID":"fixture-a", "name":"fixture", "jwt":"fixture-secret"})).unwrap()];
        let mut groups = crate::models::GroupsFile::default();
        let mut cooldowns = crate::models::AccountCooldownsFile::default();
        let credits = crate::models::RemainingCreditsFile::default();
        let excluded = AccountDirectory::from_legacy(&accounts, &[], &pool, &groups, &cooldowns, &credits, NOW).unwrap();
        assert!(!excluded.accounts[0].enabled);
        groups.membership.insert("fixture-a".into(), "allowed".into());
        cooldowns.cooldowns.insert("fixture-a".into(), crate::models::CooldownEntry { error_type: "HardCredit".into(), until: 0, reason: "private-raw-response".into(), error_count: 2 });
        let cooling = AccountDirectory::from_legacy(&accounts, &[], &pool, &groups, &cooldowns, &credits, NOW).unwrap();
        assert_eq!(cooling.accounts[0].state, UpstreamAccountState::Cooling);
        assert!(!format!("{:?}", cooling.accounts).contains("private-raw-response"));
        pool.enabled_uids.clear();
        let disabled = AccountDirectory::from_legacy(&accounts, &[], &pool, &groups, &cooldowns, &credits, NOW).unwrap();
        assert!(!disabled.accounts[0].enabled);
    }

    #[test]
    fn scheduler_shadow_storage_failure_is_diagnostic_only() {
        let f = Fixture::new();
        let runtime = SchedulerRuntime::new(f.store.clone(), f.dir.join("missing-database"), SchedulerMode::Shadow, BTreeMap::new(), BTreeMap::new(), NOW).unwrap();
        let report = runtime.shadow_dry_run(NOW);
        assert_eq!(report.last_error.as_deref(), Some("scheduler_storage_unavailable"));
        assert_eq!(f.store.count_rows("requests").unwrap(), 0);
    }

    #[test]
    fn scheduler_reader_identity_mismatch_never_reaches_storage() {
        let f = Fixture::new();
        let directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let id = directory.accounts[0].id.clone();
        let snapshot = ObservationSnapshot {
            account_ref: "different-account".into(), resource_kind: "chat.general".into(), available_units: Some(10), value_scale: 1,
            source: "reader".into(), observed_at_ms: NOW, stale_at_ms: NOW + 1000, capabilities: vec![], region: None, summary: json!({}),
        };
        let mut readers: ObservationRegistry = BTreeMap::new();
        readers.insert("trae".into(), Arc::new(MockObservationReader::new(std::collections::HashMap::from([((id.clone(), "chat.general".into()), snapshot)]))));
        let runtime = SchedulerRuntime::new(f.store.clone(), f.dir.clone(), SchedulerMode::Shadow, readers, BTreeMap::new(), NOW).unwrap();
        let error = runtime.refresh_observation(ObservationRequest { account_ref: id.clone(), provider: "trae".into(), resource_kind: "chat.general".into() }, NOW).unwrap_err();
        assert_eq!(error.code(), "scheduler_reader_unavailable");
        assert_ne!(f.store.get_latest_observation(&id, "chat.general").unwrap().unwrap().status, ObservationStatus::Fresh);
        assert_eq!(f.store.count_rows("quota_ledger").unwrap(), 0);
    }

    #[test]
    fn scheduler_shadow_candidate_diagnostics_are_redacted_and_never_authorize_execution() {
        let f = Fixture::new();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory()).unwrap();
        let runtime = f.runtime(SchedulerMode::Shadow);
        let candidates = runtime.shadow_candidates("chat.general", NOW);
        assert_eq!(candidates.len(), 2);
        assert!(candidates.iter().all(|c| c.observation != "fresh"));
        let text = serde_json::to_string(&candidates).unwrap();
        assert!(!text.contains("fixture-user-sensitive") && !text.contains("vault://"));
        assert!(runtime.require_endpoint("chat").is_err());
        assert_eq!(f.store.count_rows("quota_reservations").unwrap(), 0);
    }

    #[test]
    fn scheduler_reader_failure_at_same_timestamp_supersedes_fresh_snapshot() {
        let f = Fixture::new();
        let directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let id = directory.accounts[0].id.clone();
        f.store.append_upstream_observation(UpstreamObservation::new(
            "zzzz-fresh".into(), id.clone(), "chat.general".into(), Some(1200), 100,
            "reader".into(), ObservationStatus::Fresh, NOW, NOW + 60000, json!({"available":1200}),
        )).unwrap();
        let runtime = f.runtime(SchedulerMode::Shadow);
        assert!(runtime.refresh_observation(ObservationRequest { account_ref: id.clone(), provider: "trae".into(), resource_kind: "chat.general".into() }, NOW).is_err());
        let latest = f.store.get_latest_observation(&id, "chat.general").unwrap().unwrap();
        assert_eq!(latest.status, ObservationStatus::Failed);
        assert_eq!(latest.observed_value, Some(1200));
        assert_eq!(latest.value_scale, 100);
    }

    #[test]
    fn scheduler_failed_refresh_preserves_value_and_scale_from_same_fresh_row() {
        for null_status in [ObservationStatus::Fresh, ObservationStatus::Stale] {
            let f = Fixture::new();
            let directory = directory();
            sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
            let id = directory.accounts[0].id.clone();
            f.store.append_upstream_observation(UpstreamObservation::new(
                "fresh-scaled".into(), id.clone(), "chat.general".into(), Some(1000), 100,
                "reader".into(), ObservationStatus::Fresh, NOW, NOW + 60000,
                json!({"available":1000, "value_scale":100}),
            )).unwrap();
            f.store.append_upstream_observation(UpstreamObservation::new(
                "later-null".into(), id.clone(), "chat.general".into(), None, 1,
                "reader".into(), null_status, NOW + 1, NOW + 60000, json!({}),
            )).unwrap();
            let runtime = f.runtime(SchedulerMode::Shadow);
            for now_ms in [NOW + 2, NOW + 3] {
                assert!(runtime.refresh_observation(ObservationRequest {
                    account_ref: id.clone(), provider: "trae".into(), resource_kind: "chat.general".into(),
                }, now_ms).is_err());
                let latest = f.store.get_latest_observation(&id, "chat.general").unwrap().unwrap();
                assert_eq!(latest.status, ObservationStatus::Failed);
                assert_eq!((latest.observed_value, latest.value_scale), (Some(1000), 100));
            }
            assert_eq!(f.store.count_rows("quota_ledger").unwrap(), 0);
        }
    }

    #[test]
    fn scheduler_sync_preserves_available_core_health_metadata() {
        let f = Fixture::new();
        let mut directory = directory();
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let mut account = directory.accounts[0].clone();
        account.state = UpstreamAccountState::Available;
        account.consecutive_errors = 4;
        account.cooldown_until_ms = Some(NOW + 60000);
        account.cooldown_reason = Some("transport_timeout".into());
        f.store.upsert_upstream_account(account.clone(), &f.admin).unwrap();
        let audit_count = f.store.count_rows("audit_events").unwrap();
        assert_eq!(sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap(), 0);
        assert_eq!(read_directory(&f.dir).unwrap()[&account.id], account);
        assert_eq!(f.store.count_rows("audit_events").unwrap(), audit_count);

        // A shorter legacy cooldown must not replace the stronger Core cooldown or its reason.
        directory.accounts[0].state = UpstreamAccountState::Cooling;
        directory.accounts[0].consecutive_errors = 1;
        directory.accounts[0].cooldown_until_ms = Some(NOW + 1000);
        directory.accounts[0].cooldown_reason = Some("legacy_cooldown".into());
        sync_upstream_accounts(&f.store, &f.dir, &f.admin, &directory).unwrap();
        let actual = read_directory(&f.dir).unwrap().remove(&account.id).unwrap();
        assert_eq!(actual.consecutive_errors, 4);
        assert_eq!(actual.cooldown_until_ms, Some(NOW + 60000));
        assert_eq!(actual.cooldown_reason.as_deref(), Some("transport_timeout"));
    }
}
