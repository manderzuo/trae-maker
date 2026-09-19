use std::collections::BTreeSet;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{CoreError, PreflightReserveInput, RequestHandle, UpstreamAccountState, UpstreamLease};

pub(crate) const MAX_OBSERVATION_SUMMARY_BYTES: usize = 4 * 1024;
pub(crate) const DEFAULT_RECONCILE_TTL_MS: i64 = 600_000;
const HARD_CREDIT_COOLDOWN_MS: i64 = 24 * 60 * 60 * 1_000;
const PLAN_LIMIT_COOLDOWN_MS: i64 = 12 * 60 * 60 * 1_000;
const SHORT_COOLDOWN_MS: i64 = 60 * 1_000;
const CLIENT_COOLDOWN_MS: i64 = 10 * 60 * 1_000;
const SERVER_COOLDOWN_MS: i64 = 30 * 60 * 1_000;
const SERVER_MAX_COOLDOWN_MS: i64 = 6 * 60 * 60 * 1_000;

/// Scheduler-specific input. `preflight` remains the source of request identity and user quota.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchedulerLeaseRequest {
    pub preflight: PreflightReserveInput,
    pub provider_hint: Option<String>,
    pub required_capabilities: Vec<String>,
    pub region: Option<String>,
    pub predicted_units: i64,
    pub safety_margin_units: i64,
    pub observation_max_age_ms: i64,
    pub allowed_accounts: Option<Vec<String>>,
    pub dedicated_account: Option<String>,
    pub selection_strategy: SelectionStrategy,
    /// Injected time keeps Core scheduler decisions deterministic and testable.
    pub now_ms: i64,
    pub lease_ttl_ms: i64,
    pub reconcile_ttl_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionStrategy {
    HighestNormalizedAvailable,
    LeastActiveSlots,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpstreamLeaseGrant {
    pub lease_id: String,
    pub account_ref: String,
    /// An opaque locator only; callers must never persist or log a resolved secret.
    pub credentials_ref: String,
    pub observation_id: String,
    pub predicted_units: i64,
    pub lease_expires_at_ms: i64,
}

/// Only a newly created result grants permission to dispatch upstream work.
/// A replay intentionally exposes persisted state rather than credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchedulerLeaseResult {
    Acquired(UpstreamLeaseGrant),
    Replay { request: RequestHandle, lease: UpstreamLease },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaseOutcome {
    Success {
        actual_units: Option<i64>,
        upstream_request_ref: Option<String>,
        now_ms: i64,
    },
    Rejected {
        status: i64,
        code: Option<String>,
        accepted: bool,
        now_ms: i64,
    },
    TransportUnknown {
        reason: String,
        upstream_request_ref: Option<String>,
        now_ms: i64,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ScheduleError {
    #[error("scheduler request identity does not match the principal")]
    InvalidRequestIdentity,
    #[error("scheduler request is missing required scope: {0}")]
    MissingScope(String),
    #[error("idempotency key conflicts with an existing request")]
    IdempotencyConflict,
    #[error("no fresh upstream observation is eligible")]
    NoFreshObservation,
    #[error("no upstream capacity is eligible")]
    NoUpstreamCapacity,
    #[error("no upstream account matches the requested capabilities")]
    CapabilityMismatch,
    #[error("upstream account is cooling or disabled")]
    AccountCooling,
    #[error("upstream lease {0} was not found")]
    LeaseNotFound(String),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Core(#[from] CoreError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CandidateAccount {
    pub id: String,
    pub provider: String,
    pub credentials_ref: String,
    pub value_scale: i64,
    pub normalized_available_units: i64,
    pub observation_id: String,
    pub max_concurrency: i64,
    pub active_slots: i64,
}

pub(crate) fn validate_scheduler_request(input: &SchedulerLeaseRequest) -> Result<(), ScheduleError> {
    if input.preflight.amount <= 0
        || input.preflight.ttl_ms < 0
        || input.predicted_units <= 0
        || input.safety_margin_units < 0
        || input.observation_max_age_ms < 0
        || input.lease_ttl_ms <= 0
        || input.reconcile_ttl_ms <= 0
    {
        return Err(ScheduleError::Core(CoreError::InvalidQuotaAmount));
    }
    if input.required_capabilities.iter().any(|capability| capability.trim().is_empty()) {
        return Err(ScheduleError::CapabilityMismatch);
    }
    Ok(())
}

pub(crate) fn account_matches_constraints(
    account_id: &str,
    provider: &str,
    region: Option<&str>,
    capabilities: &BTreeSet<String>,
    input: &SchedulerLeaseRequest,
) -> bool {
    input.provider_hint.as_deref().map_or(true, |hint| hint == provider)
        && input.region.as_deref().map_or(true, |wanted| region == Some(wanted))
        && input.required_capabilities.iter().all(|capability| capabilities.contains(capability))
        && input.allowed_accounts.as_ref().map_or(true, |allowed| allowed.iter().any(|allowed| allowed == account_id))
        && input.dedicated_account.as_deref().map_or(true, |dedicated| dedicated == account_id)
}

pub(crate) fn selection_reason(input: &SchedulerLeaseRequest) -> &'static str {
    if input.dedicated_account.is_some() {
        "dedicated_account"
    } else if input.allowed_accounts.is_some() {
        "allowed_account_ranked"
    } else {
        match input.selection_strategy {
            SelectionStrategy::HighestNormalizedAvailable => "highest_normalized_available_units",
            SelectionStrategy::LeastActiveSlots => "least_active_slots",
        }
    }
}

pub(crate) fn sanitize_error_category(value: &str, transport: bool) -> &'static str {
    if transport {
        if value.to_ascii_lowercase().contains("timeout") || value.to_ascii_lowercase().contains("deadline") {
            "transport_timeout"
        } else {
            "transport_unknown"
        }
    } else {
        match value {
            "invalid_request" => "invalid_request",
            "rate_limited" => "rate_limited",
            "forbidden" => "forbidden",
            "not_found" => "not_found",
            "insufficient_credits" => "insufficient_credits",
            _ => "upstream_rejected",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccountHealthDecision {
    pub category: &'static str,
    pub state: UpstreamAccountState,
    pub enabled: bool,
    pub cooldown_until_ms: Option<i64>,
    pub cooldown_reason: Option<&'static str>,
    pub consecutive_errors: i64,
}

/// Maps adapter/legacy error labels to the small persistent health vocabulary.
/// The returned value is safe to persist and safe to expose as an error category.
pub(crate) fn normalize_health_category(value: &str) -> &'static str {
    match value.trim().to_ascii_lowercase().as_str() {
        "success" | "ok" | "none" => "success",
        "hardcredit" | "hard_credit" | "insufficient_credits" | "insufficient_credit" => "hard_credit",
        "planlimit" | "plan_limit" => "plan_limit",
        "softrate" | "soft_rate" | "rate_limited" | "ratelimited" => "soft_rate",
        "notfound" | "not_found" => "not_found",
        "sessiondead" | "session_dead" => "session_dead",
        "forbidden" => "forbidden",
        "server" | "upstream_execution_unknown" => "server",
        "transport_timeout" | "timeout" => "transport_timeout",
        "transport_unknown" | "disconnected" => "transport_unknown",
        "reader_failure" | "reader_unavailable" => "reader_failure",
        "client" | "invalid_request" | "upstream_rejected" => "client",
        _ => "client",
    }
}

pub(crate) fn account_health_decision(
    category: &str,
    previous_errors: i64,
    now_ms: i64,
) -> AccountHealthDecision {
    let category = normalize_health_category(category);
    let consecutive_errors = if category == "success" || category == "reader_failure" {
        0
    } else {
        previous_errors.saturating_add(1)
    };
    let cooldown = |duration: i64| now_ms.checked_add(duration).or(Some(i64::MAX));
    match category {
        "success" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Available,
            enabled: true,
            cooldown_until_ms: None,
            cooldown_reason: None,
            consecutive_errors,
        },
        "hard_credit" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Cooling,
            enabled: true,
            cooldown_until_ms: None,
            cooldown_reason: Some("hard_credit"),
            consecutive_errors,
        },
        "plan_limit" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Cooling,
            enabled: true,
            cooldown_until_ms: cooldown(PLAN_LIMIT_COOLDOWN_MS),
            cooldown_reason: Some("plan_limit"),
            consecutive_errors,
        },
        "soft_rate" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Cooling,
            enabled: true,
            cooldown_until_ms: cooldown(SHORT_COOLDOWN_MS),
            cooldown_reason: Some("soft_rate"),
            consecutive_errors,
        },
        "not_found" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Cooling,
            enabled: true,
            cooldown_until_ms: cooldown(SHORT_COOLDOWN_MS),
            cooldown_reason: Some("not_found"),
            consecutive_errors,
        },
        "session_dead" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Disabled,
            enabled: false,
            cooldown_until_ms: cooldown(HARD_CREDIT_COOLDOWN_MS),
            cooldown_reason: Some("session_dead"),
            consecutive_errors,
        },
        "forbidden" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Forbidden,
            enabled: false,
            cooldown_until_ms: None,
            cooldown_reason: Some("forbidden"),
            consecutive_errors,
        },
        "transport_timeout" | "transport_unknown" | "server" => {
            let exponent = (previous_errors.max(0) as u32).min(3);
            let multiplier = 1_i64.checked_shl(exponent).unwrap_or(8);
            let duration = SERVER_COOLDOWN_MS
                .saturating_mul(multiplier)
                .min(SERVER_MAX_COOLDOWN_MS);
            AccountHealthDecision {
                category,
                state: UpstreamAccountState::Cooling,
                enabled: true,
                cooldown_until_ms: cooldown(duration),
                cooldown_reason: Some(category),
                consecutive_errors,
            }
        }
        "reader_failure" => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Available,
            enabled: true,
            cooldown_until_ms: None,
            cooldown_reason: None,
            consecutive_errors,
        },
        _ => AccountHealthDecision {
            category,
            state: UpstreamAccountState::Cooling,
            enabled: true,
            cooldown_until_ms: cooldown(CLIENT_COOLDOWN_MS),
            cooldown_reason: Some("client"),
            consecutive_errors,
        },
    }
}

pub(crate) fn audit_hash(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn audit_identifier(value: Option<&str>) -> Option<String> {
    value
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 128
                && value.chars().all(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
                })
        })
        .map(ToOwned::to_owned)
}

pub(crate) fn audit_label(value: &str) -> String {
    if !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
    {
        value.to_owned()
    } else {
        format!("hash:{}", audit_hash(value))
    }
}

pub(crate) fn sanitize_upstream_request_ref(value: Option<String>) -> Option<String> {
    value.filter(|value| {
        !value.is_empty()
            && value.len() <= 128
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
            })
    })
}

pub(crate) fn validate_required(field: &str, value: &str) -> Result<(), CoreError> {
    if value.trim().is_empty() {
        return Err(CoreError::Validation {
            field: field.into(),
            reason: "must not be empty".into(),
        });
    }
    Ok(())
}

pub(crate) fn validate_opaque_credentials_ref(value: &str) -> Result<(), CoreError> {
    validate_required("credentials_ref", value)?;
    let Some((scheme, locator)) = value.split_once("://") else {
        return Err(CoreError::Validation {
            field: "credentials_ref".into(),
            reason: "must be an opaque vault or keychain reference".into(),
        });
    };
    if !matches!(
        scheme,
        "vault" | "keychain" | "keyring" | "credential" | "opaque" | "tauri" | "tauri-vault" | "legacy"
    ) || locator.is_empty()
        || locator.len() > 192
        || !locator.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | '/')
        })
    {
        return Err(CoreError::Validation {
            field: "credentials_ref".into(),
            reason: "must be an opaque vault or keychain reference".into(),
        });
    }
    Ok(())
}

pub(crate) fn validate_observation_summary(summary: &Value) -> Result<String, CoreError> {
    let object = summary.as_object().ok_or_else(|| CoreError::Validation {
        field: "summary".into(),
        reason: "must be a redacted summary object".into(),
    })?;
    for (field, value) in object {
        let valid = match field.as_str() {
            "available" | "total" | "general" | "work" => value.is_number(),
            "expires_at_ms" | "observed_at_ms" | "value_scale" => value.as_i64().is_some(),
            "eligible" => value.is_boolean(),
            "provider" | "source" | "resource_kind" | "status" | "reason" | "observation_kind" => {
                value.as_str().is_some_and(valid_summary_label)
            }
            _ => false,
        };
        if !valid {
            return Err(CoreError::Validation {
                field: "summary".into(),
                reason: format!("contains a disallowed or non-redacted field: {field}"),
            });
        }
    }
    let serialized = serde_json::to_string(summary)?;
    if serialized.len() > MAX_OBSERVATION_SUMMARY_BYTES {
        return Err(CoreError::Validation {
            field: "summary".into(),
            reason: format!("must not exceed {MAX_OBSERVATION_SUMMARY_BYTES} bytes"),
        });
    }
    Ok(serialized)
}

fn valid_summary_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
}
