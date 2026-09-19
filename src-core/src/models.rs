use std::{collections::BTreeSet, fmt, sync::Arc};

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
