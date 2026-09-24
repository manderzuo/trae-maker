mod cost;
mod credits;
mod admin_summary;
mod admin_credentials;
mod error;
mod identity;
mod jobs;
mod models;
mod ports;
mod quota;
mod requests;
mod schema;
mod store;
mod upstream;
mod usage_trend;
mod video_billing;

pub use cost::{CostError, CostEstimate, CostPolicy};
pub use credits::CreditAmount;
pub use error::CoreError;
pub use identity::{require_scope, AuthError, Principal};
pub use models::{
    AssetState, BeginRequest, BeginRequestInput, BillingQuote, BillingReceipt, BillingReceiptResult,
    BillingReceiptStatus, BillingReservationResult, CoreAsset, CreateAssetInput, CreateVideoJobInput, CoreJob,
    CoreJobAttempt, CoreApiKeyAdminView, CoreQuotaBalanceView, CoreQuotaLedgerView, CoreQuotaUsageView,
    CoreUserAdminView, CoreVideoJobAdminView, IssuedApiKey, ApiKeySecretRecord, JobAttemptState, JobState, NewUser,
    QuotaBalance, QuotaBudgetAccount, QuotaBudgetBalance, QuotaBudgetScope, QuotaGrant,
    QuotaMigrationState, KeyQuotaGrant, LegacyQuotaAllocation, UpstreamCreditSnapshot,
    UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS,
    LegacyMigrationAsset, LegacyMigrationBatch, LegacyMigrationJob, LegacyMigrationKey,
    LegacyMigrationObservation, LegacyMigrationResult,
    PreflightReserveInput, PreflightReserveResult, QuotaReserve, RecoverableBillingRequest, RequestHandle, RequestResult,
    LeaseState, ObservationStatus, RegisterUpstreamAccount, RequestState, Reservation,
    ReservationState, ReserveResult, Settlement, SharedCoreStore, UpstreamAccount,
    UpstreamAccountState, UpstreamLease, UpstreamObservation, User, UserRole, VideoJobEnqueueResult,
    VideoJobQueueClaim,
};
pub use ports::{
    ChatExecutionRequest, ChatExecutionResult, ChatExecutor, MockChatExecutor,
    MockObservationReader, ObservationError, ObservationReader, ObservationRequest,
    ObservationSnapshot, UpstreamError,
};
pub use requests::canonical_json_hash;
pub use store::{CoreStore, CORE_DB_FILE, CURRENT_SCHEMA_VERSION};
pub use usage_trend::UsageTrendPoint;
pub use video_billing::{VideoBillingControl, VideoBillingControlInput, VideoBillingMode, VideoDiagnosticClaim};
pub use admin_summary::CoreAdminSummary;
pub use admin_credentials::{AdminCredentialRecord, NewAdminCredential};
pub use upstream::{
    LeaseOutcome, LeaseSettlement, ScheduleError, SchedulerLeaseRequest, SchedulerLeaseResult,
    SelectionStrategy, UpstreamLeaseGrant, VideoJobLeaseResult,
};

impl CoreStore {
    /// Records a successful reader snapshot as a new immutable observation row.
    /// Reader failures use `append_upstream_observation` with `Failed` so the
    /// prior fresh value is preserved by the existing storage boundary.
    pub fn record_observation_snapshot(
        &self,
        snapshot: ObservationSnapshot,
    ) -> Result<UpstreamObservation, CoreError> {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
        use rand::{rngs::OsRng, RngCore};

        let mut material = [0_u8; 16];
        OsRng.fill_bytes(&mut material);
        let status = if snapshot.source == "json_cache" {
            ObservationStatus::Stale
        } else {
            ObservationStatus::Fresh
        };
        self.append_upstream_observation(UpstreamObservation::new(
            format!("observation_{}", URL_SAFE_NO_PAD.encode(material)),
            snapshot.account_ref,
            snapshot.resource_kind,
            snapshot.available_units,
            snapshot.value_scale,
            snapshot.source,
            status,
            snapshot.observed_at_ms,
            snapshot.stale_at_ms,
            snapshot.summary,
        ))
    }
}
