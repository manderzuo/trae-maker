use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoBillingMode {
    Paused,
    DiagnosticOnce,
    Active,
}

impl VideoBillingMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::DiagnosticOnce => "diagnostic_once",
            Self::Active => "active",
        }
    }

    pub fn from_db(value: &str) -> Option<Self> {
        match value {
            "paused" => Some(Self::Paused),
            "diagnostic_once" => Some(Self::DiagnosticOnce),
            "active" => Some(Self::Active),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoBillingControl {
    pub mode: VideoBillingMode,
    pub reason: String,
    pub diagnostic_key_id: Option<String>,
    pub diagnostic_request_hash: Option<String>,
    pub diagnostic_claimed_at_ms: Option<i64>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoBillingControlInput {
    pub mode: VideoBillingMode,
    pub reason: String,
    pub diagnostic_key_id: Option<String>,
    pub diagnostic_request_hash: Option<String>,
}

impl VideoBillingControlInput {
    pub fn diagnostic(key_id: &str, request_hash: &str, reason: &str) -> Self {
        Self {
            mode: VideoBillingMode::DiagnosticOnce,
            reason: reason.to_owned(),
            diagnostic_key_id: Some(key_id.to_owned()),
            diagnostic_request_hash: Some(request_hash.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoDiagnosticClaim {
    pub claimed_at_ms: i64,
}
