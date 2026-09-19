use std::{collections::BTreeSet, fmt, sync::Arc};

use serde_json::Value;

use crate::CoreStore;

pub type SharedCoreStore = Arc<CoreStore>;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub role: UserRole,
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
                | (Self::Queued, Self::Dispatched | Self::Failed | Self::Unknown)
                | (Self::Dispatched, Self::Completing | Self::Failed | Self::Unknown)
                | (Self::Completing, Self::Succeeded | Self::Failed | Self::Unknown)
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
