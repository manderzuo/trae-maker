mod cost;
mod error;
mod identity;
mod models;
mod ports;
mod quota;
mod requests;
mod schema;
mod store;
mod upstream;

pub use cost::{CostError, CostEstimate, CostPolicy};
pub use error::CoreError;
pub use identity::{require_scope, AuthError, Principal};
pub use models::{
    BeginRequest, BeginRequestInput, IssuedApiKey, NewUser, QuotaBalance, QuotaGrant,
    LegacyMigrationAsset, LegacyMigrationBatch, LegacyMigrationJob, LegacyMigrationKey,
    LegacyMigrationObservation, LegacyMigrationResult,
    PreflightReserveInput, PreflightReserveResult, QuotaReserve, RequestHandle, RequestResult,
    LeaseState, ObservationStatus, RegisterUpstreamAccount, RequestState, Reservation,
    ReservationState, ReserveResult, Settlement, SharedCoreStore, UpstreamAccount,
    UpstreamAccountState, UpstreamLease, UpstreamObservation, User, UserRole,
};
pub use ports::{ChatExecutionRequest, ChatExecutionResult, ChatExecutor, MockChatExecutor, UpstreamError};
pub use requests::canonical_json_hash;
pub use store::{CoreStore, CORE_DB_FILE, CURRENT_SCHEMA_VERSION};
