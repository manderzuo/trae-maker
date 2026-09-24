#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("core storage I/O error: {source}")]
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("core SQLite error: {source}")]
    Sqlite {
        #[from]
        source: rusqlite::Error,
    },
    #[error("core schema migration error: {source}")]
    Migration {
        #[source]
        source: rusqlite::Error,
    },
    #[error("core schema version is invalid: {value}")]
    InvalidSchemaVersion { value: String },
    #[error("core schema version {version} is newer than this binary supports")]
    UnsupportedSchemaVersion { version: u32 },
    #[error("core serialization error: {source}")]
    Serialization {
        #[from]
        source: serde_json::Error,
    },
    #[error("invalid {field}: {reason}")]
    Validation { field: String, reason: String },
    #[error("quota amount must be positive")]
    InvalidQuotaAmount,
    #[error("actual quota amount cannot exceed the reserved amount")]
    ActualAmountExceedsReservation,
    #[error("quota reservation {reservation_id} was not found")]
    ReservationNotFound { reservation_id: String },
    #[error("quota reservation request id conflict: {request_id}")]
    ReservationRequestConflict { request_id: String },
    #[error("quota adjustment would overdraw the available balance")]
    QuotaOverdrawn,
    #[error("no enabled cost policy matches endpoint {endpoint} and model {model}")]
    BudgetPolicyMissing { endpoint: String, model: String },
    #[error("api key {api_key_id} is not active for user {user_id}")]
    InvalidRequestIdentity { user_id: String, api_key_id: String },
    #[error("request {request_id} cannot transition from {expected:?} to {next:?}")]
    InvalidTransition {
        request_id: String,
        expected: crate::RequestState,
        next: crate::RequestState,
    },
    #[error("invalid configuration for {key}: {value}")]
    InvalidConfiguration { key: String, value: String },
    #[error("missing required scope: {scope}")]
    MissingScope { scope: String },
    #[error("quota is insufficient: available {available}, required {required}")]
    QuotaInsufficient { available: i64, required: i64 },
    #[error("quota pool is not configured for user {user_id} and resource {resource_kind}")]
    QuotaPoolNotConfigured { user_id: String, resource_kind: String },
    #[error("quota pool is insufficient: available {available}, required {required}")]
    QuotaPoolInsufficient { available: i64, required: i64 },
    #[error("AI Work upstream credits are unavailable: {reason}")]
    UpstreamCreditsUnavailable { reason: String },
    #[error("AI Work upstream credits {upstream_total} are below existing Key commitments {committed}")]
    UpstreamCommitmentsExceedBalance { upstream_total: i64, committed: i64 },
    #[error("AI Work upstream allocatable credits are insufficient: available {available}, required {required}")]
    UpstreamCreditLimitExceeded { available: i64, required: i64 },
    #[error("api key {api_key_id} concurrency limit reached: {active_concurrency}/{max_concurrency}")]
    KeyConcurrencyExceeded {
        api_key_id: String,
        active_concurrency: i64,
        max_concurrency: i64,
    },
    #[error("key quota is not configured for api key {api_key_id} and resource {resource_kind}")]
    KeyQuotaNotConfigured { api_key_id: String, resource_kind: String },
    #[error("quota budget account {account_id} requires migration reconciliation")]
    QuotaMigrationPending { account_id: String },
    #[error("api key {api_key_id} does not belong to user {user_id}")]
    ApiKeyOwnershipMismatch { api_key_id: String, user_id: String },
    #[error("quota migration {migration_id} conflicts with an existing allocation")]
    QuotaMigrationConflict { migration_id: String },
    #[error("reservation {reservation_id} has already been settled with a different decision")]
    ReservationSettlementConflict { reservation_id: String },
    #[error("idempotency key conflicts with an existing request")]
    IdempotencyConflict,
    #[error("reservation {reservation_id} failed after reservation: {source}")]
    ReservationContext {
        reservation_id: String,
        #[source]
        source: Box<CoreError>,
    },
    #[error("core mode {mode} does not permit enforcing chat operations")]
    CoreModeNotEnforcing { mode: String },
    #[error("active admin authorization is required")]
    AdminRequired,
    #[error("target user is not active")]
    UserNotActive,
    #[error("user {user_id} was not found")]
    UserNotFound { user_id: String },
    #[error("api key {api_key_id} was not found")]
    ApiKeyNotFound { api_key_id: String },
    #[error("API key encryption is unavailable")]
    ApiKeyEncryptionUnavailable,
    #[error("encrypted key material is unavailable for API key {api_key_id}")]
    ApiKeySecretUnavailable { api_key_id: String },
    #[error("legacy migration validation failed: {reason}")]
    MigrationValidation { reason: String },
    #[error("reservation {reservation_id} is owned by another principal")]
    ReservationOwnerMismatch { reservation_id: String },
    #[error("request {request_id} was not found")]
    RequestNotFound { request_id: String },
    #[error("billing quote does not match request {request_id}")]
    BillingQuoteMismatch { request_id: String },
    #[error("billing quote for request {request_id} has expired")]
    BillingQuoteExpired { request_id: String },
    #[error("billing quote conflicts with the existing quote for request {request_id}")]
    BillingQuoteConflict { request_id: String },
    #[error("billing receipt is invalid: {reason}")]
    BillingReceiptInvalid { reason: String },
    #[error("billing settlement is blocked for API key {api_key_id}")]
    ApiKeyBillingBlocked { api_key_id: String },
    #[error("request {request_id} operation failed: {source}")]
    RequestContext {
        request_id: String,
        #[source]
        source: Box<CoreError>,
    },
}
