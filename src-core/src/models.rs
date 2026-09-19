use std::{collections::{BTreeMap, BTreeSet}, fmt, sync::Arc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{CoreStore, Principal};

pub type SharedCoreStore = Arc<CoreStore>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamAccountState {
    Available,
    Cooling,
    Forbidden,
    Disabled,
}

impl UpstreamAccountState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Cooling => "cooling",
            Self::Forbidden => "forbidden",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationStatus {
    Fresh,
    Stale,
    Failed,
}

impl ObservationStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "fresh" => Some(Self::Fresh),
            "stale" => Some(Self::Stale),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseState {
    Held,
    Active,
    Succeeded,
    Failed,
    Unknown,
    Released,
}

impl LeaseState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Active => "active",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Released => "released",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "active" => Some(Self::Active),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            "released" => Some(Self::Released),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterUpstreamAccount {
    pub id: String,
    pub provider: String,
    /// Opaque vault/keychain locator only; never a raw credential.
    pub credentials_ref: String,
    pub region: Option<String>,
    pub capabilities: BTreeSet<String>,
    pub enabled: bool,
    pub max_concurrency: i64,
    pub state: UpstreamAccountState,
    pub cooldown_until_ms: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub consecutive_errors: i64,
}

impl RegisterUpstreamAccount {
    pub fn new(id: String, provider: String, credentials_ref: String) -> Self {
        Self {
            id,
            provider,
            credentials_ref,
            region: None,
            capabilities: BTreeSet::new(),
            enabled: true,
            max_concurrency: 1,
            state: UpstreamAccountState::Available,
            cooldown_until_ms: None,
            cooldown_reason: None,
            consecutive_errors: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamAccount {
    pub id: String,
    pub provider: String,
    /// Opaque vault/keychain locator only; never a raw credential.
    pub credentials_ref: String,
    pub region: Option<String>,
    pub capabilities: BTreeSet<String>,
    pub enabled: bool,
    pub max_concurrency: i64,
    pub state: UpstreamAccountState,
    pub cooldown_until_ms: Option<i64>,
    pub cooldown_reason: Option<String>,
    pub consecutive_errors: i64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamObservation {
    pub id: String,
    pub account_ref: String,
    pub resource_kind: String,
    pub observed_value: Option<i64>,
    pub value_scale: i64,
    pub source: String,
    pub status: ObservationStatus,
    pub observed_at_ms: i64,
    pub stale_at_ms: i64,
    pub summary: Value,
}

impl UpstreamObservation {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        account_ref: String,
        resource_kind: String,
        observed_value: Option<i64>,
        value_scale: i64,
        source: String,
        status: ObservationStatus,
        observed_at_ms: i64,
        stale_at_ms: i64,
        summary: Value,
    ) -> Self {
        Self {
            id,
            account_ref,
            resource_kind,
            observed_value,
            value_scale,
            source,
            status,
            observed_at_ms,
            stale_at_ms,
            summary,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamLease {
    pub id: String,
    pub request_id: String,
    pub account_ref: String,
    pub resource_kind: String,
    pub predicted_units: i64,
    pub observation_id: Option<String>,
    pub state: LeaseState,
    pub lease_expires_at_ms: i64,
    pub reconcile_until_ms: Option<i64>,
    pub upstream_request_ref: Option<String>,
    pub error_kind: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub settled_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRole {
    Admin,
    Operator,
    User,
}

impl UserRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    pub id: String,
    pub name: String,
    pub role: UserRole,
}

/// Sanitized legacy records accepted by the atomic migration boundary.
/// `legacy_key` is transient input only and is never persisted by CoreStore.
#[derive(Clone, PartialEq, Eq)]
pub struct LegacyMigrationKey {
    pub legacy_key_id: String,
    pub legacy_key: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMigrationAsset {
    pub id: String,
    pub owner_key_id: String,
    pub user_id: String,
    pub filename: String,
    pub mime_type: String,
    pub extension: String,
    pub size: i64,
    pub content_sha256: String,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub storage_ref: String,
    pub migration_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMigrationJob {
    pub id: String,
    pub owner_key_id: String,
    pub user_id: String,
    pub status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMigrationObservation {
    pub id: String,
    pub account_ref: String,
    pub resource_kind: String,
    pub observed_value: Option<i64>,
    pub summary_json: String,
    pub observed_at_ms: i64,
}

#[derive(Clone, PartialEq, Eq)]
pub struct LegacyMigrationBatch {
    pub migration_id: String,
    pub actor: Principal,
    pub reason: String,
    pub scopes: BTreeSet<String>,
    pub source_hashes: BTreeMap<String, String>,
    pub keys: Vec<LegacyMigrationKey>,
    pub assets: Vec<LegacyMigrationAsset>,
    pub jobs: Vec<LegacyMigrationJob>,
    pub observations: Vec<LegacyMigrationObservation>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyMigrationResult {
    pub migration_id: String,
    pub issued_keys: Vec<IssuedApiKey>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreUserAdminView {
    pub id: String,
    pub name: String,
    pub role: String,
    pub status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreApiKeyAdminView {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prefix: String,
    pub scopes: BTreeSet<String>,
    pub status: String,
    pub created_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetState {
    Active,
    Expired,
}

impl AssetState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Expired => "expired",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "active" => Some(Self::Active),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateAssetInput {
    pub id: String,
    pub filename: String,
    pub mime_type: String,
    pub extension: String,
    pub size: i64,
    pub sha256: String,
    pub storage_ref: String,
    pub content_token_digest: Vec<u8>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreAsset {
    pub id: String,
    pub user_id: String,
    pub filename: String,
    pub mime_type: String,
    pub extension: String,
    pub size: i64,
    pub sha256: String,
    pub storage_ref: String,
    pub content_token_digest: Vec<u8>,
    pub created_at_ms: i64,
    pub expires_at_ms: i64,
    pub state: AssetState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Created,
    Queued,
    Running,
    CancelRequested,
    Canceled,
    Succeeded,
    Failed,
    Unknown,
}

impl JobState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Canceled => "canceled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "created" => Some(Self::Created),
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "cancel_requested" => Some(Self::CancelRequested),
            "canceled" => Some(Self::Canceled),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Created, Self::Queued | Self::Failed | Self::Unknown)
                | (Self::Queued, Self::Running | Self::CancelRequested | Self::Failed | Self::Unknown)
                | (Self::Running, Self::Succeeded | Self::Failed | Self::CancelRequested | Self::Unknown)
                | (Self::CancelRequested, Self::Canceled | Self::Unknown)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobAttemptState {
    Queued,
    Running,
    CancelRequested,
    Canceled,
    Succeeded,
    Failed,
    Unknown,
}

impl JobAttemptState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::CancelRequested => "cancel_requested",
            Self::Canceled => "canceled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "queued" => Some(Self::Queued),
            "running" => Some(Self::Running),
            "cancel_requested" => Some(Self::CancelRequested),
            "canceled" => Some(Self::Canceled),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateVideoJobInput {
    pub id: String,
    pub input_hash: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreJob {
    pub id: String,
    pub request_id: String,
    pub user_id: String,
    pub kind: String,
    pub model: String,
    pub input_hash: Vec<u8>,
    pub state: JobState,
    pub output_ref: Option<String>,
    pub artifact_ref: Option<String>,
    pub error_code: Option<String>,
    pub reconcile_required: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_heartbeat_ms: Option<i64>,
    pub cancel_requested_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreJobAttempt {
    pub id: String,
    pub job_id: String,
    pub attempt_no: i64,
    pub account_ref: String,
    pub lease_id: String,
    pub upstream_request_ref: Option<String>,
    pub state: JobAttemptState,
    pub error_code: Option<String>,
    pub retryable: bool,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub last_heartbeat_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoJobQueueClaim {
    pub job: CoreJob,
    pub attempt: CoreJobAttempt,
    pub lease: UpstreamLease,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BeginRequestInput {
    pub user_id: String,
    pub api_key_id: String,
    pub protocol: String,
    pub endpoint: String,
    pub model: String,
    pub idempotency_key: String,
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightReserveInput {
    pub request: BeginRequestInput,
    pub resource_kind: String,
    pub amount: i64,
    pub ttl_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHandle {
    pub id: String,
    pub user_id: String,
    pub api_key_id: String,
    pub protocol: String,
    pub endpoint: String,
    pub model: String,
    pub state: RequestState,
    pub result: Option<RequestResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginRequest {
    Created(RequestHandle),
    Existing(RequestHandle),
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreflightReserveResult {
    Created {
        request: RequestHandle,
        reservation: Reservation,
    },
    Existing {
        request: RequestHandle,
        reservation: Option<Reservation>,
    },
    Conflict,
    Insufficient { available: i64, required: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestResult {
    pub status: Option<i64>,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Received,
    Validating,
    Reserved,
    Queued,
    Dispatched,
    Completing,
    CancelRequested,
    Canceled,
    Succeeded,
    Failed,
    Unknown,
    Settled,
}

impl RequestState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Received => "received",
            Self::Validating => "validating",
            Self::Reserved => "reserved",
            Self::Queued => "queued",
            Self::Dispatched => "dispatched",
            Self::Completing => "completing",
            Self::CancelRequested => "cancel_requested",
            Self::Canceled => "canceled",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Unknown => "unknown",
            Self::Settled => "settled",
        }
    }

    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "received" => Some(Self::Received),
            "validating" => Some(Self::Validating),
            "reserved" => Some(Self::Reserved),
            "queued" => Some(Self::Queued),
            "dispatched" => Some(Self::Dispatched),
            "completing" => Some(Self::Completing),
            "cancel_requested" => Some(Self::CancelRequested),
            "canceled" => Some(Self::Canceled),
            "succeeded" => Some(Self::Succeeded),
            "failed" => Some(Self::Failed),
            "unknown" => Some(Self::Unknown),
            "settled" => Some(Self::Settled),
            _ => None,
        }
    }

    pub(crate) const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Received, Self::Validating)
                | (Self::Validating, Self::Reserved | Self::Failed | Self::Unknown)
                | (Self::Reserved, Self::Queued | Self::Failed | Self::Unknown)
                | (Self::Reserved, Self::CancelRequested)
                | (Self::Queued, Self::Dispatched | Self::Failed | Self::Unknown)
                | (Self::Queued, Self::CancelRequested)
                | (Self::Dispatched, Self::Completing | Self::Failed | Self::Unknown)
                | (Self::Dispatched, Self::CancelRequested)
                | (Self::Completing, Self::Succeeded | Self::Failed | Self::Unknown)
                | (Self::Completing, Self::CancelRequested)
                | (Self::CancelRequested, Self::Canceled | Self::Unknown)
                | (Self::Canceled, Self::Settled)
                | (Self::Succeeded | Self::Failed | Self::Unknown, Self::Settled)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaGrant {
    pub user_id: String,
    pub resource_kind: String,
    pub amount: i64,
    pub actor_user_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReserve {
    pub user_id: String,
    pub request_id: String,
    pub resource_kind: String,
    pub amount: i64,
    pub ttl_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaBalance {
    pub user_id: String,
    pub resource_kind: String,
    pub available: i64,
    pub held: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReservationState {
    Held,
    Committed,
    Released,
    Unknown,
}

impl ReservationState {
    pub(crate) fn from_db(value: &str) -> Option<Self> {
        match value {
            "held" => Some(Self::Held),
            "committed" => Some(Self::Committed),
            "released" => Some(Self::Released),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }

    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Committed => "committed",
            Self::Released => "released",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reservation {
    pub id: String,
    pub user_id: String,
    pub request_id: String,
    pub resource_kind: String,
    pub amount: i64,
    pub state: ReservationState,
    pub expires_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveResult {
    Created(Reservation),
    Existing(Reservation),
    Insufficient { available: i64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    Commit { actual_amount: Option<i64> },
    Release,
    Unknown,
}

#[derive(Clone, PartialEq, Eq)]
pub struct IssuedApiKey {
    pub id: String,
    pub plaintext: String,
    pub prefix: String,
    pub user_id: String,
    pub scopes: BTreeSet<String>,
}

impl fmt::Debug for IssuedApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedApiKey")
            .field("id", &self.id)
            .field("prefix", &self.prefix)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .finish()
    }
}
