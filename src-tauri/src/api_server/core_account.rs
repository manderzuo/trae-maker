use std::sync::Arc;

use aiwork_core::{require_scope, CoreQuotaUsageView, Principal};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api_server::{ApiSharedState, CoreMode};

#[derive(Debug, Deserialize, Default)]
pub(crate) struct UsageQuery {
    limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UsageError {
    NotEnabled,
    Unauthorized,
    InsufficientScope,
    InvalidLimit,
}

fn validate_usage_request(
    mode: CoreMode,
    principal: Option<&Principal>,
    limit: Option<usize>,
) -> Result<usize, UsageError> {
    if mode != CoreMode::Enforce {
        return Err(UsageError::NotEnabled);
    }
    let principal = principal.ok_or(UsageError::Unauthorized)?;
    require_scope(principal, "usage:read").map_err(|_| UsageError::InsufficientScope)?;
    let limit = limit.unwrap_or(100);
    if !(1..=100).contains(&limit) {
        return Err(UsageError::InvalidLimit);
    }
    Ok(limit)
}

fn usage_payload(view: &CoreQuotaUsageView, limit: usize) -> Value {
    json!({
        "object": "user_usage",
        "balances": &view.balances,
        "ledger": &view.ledger,
        "limit": limit,
    })
}

fn usage_error_response(error: UsageError) -> Response {
    let (status, code, message) = match error {
        UsageError::NotEnabled => (
            StatusCode::NOT_IMPLEMENTED,
            "core_usage_not_enabled",
            "user usage is not enabled by Core enforce mode",
        ),
        UsageError::Unauthorized => (
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Core Principal required",
        ),
        UsageError::InsufficientScope => (
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "missing required scope: usage:read",
        ),
        UsageError::InvalidLimit => (
            StatusCode::BAD_REQUEST,
            "invalid_usage_limit",
            "limit must be between 1 and 100",
        ),
    };
    (
        status,
        Json(json!({
            "error": {
                "message": message,
                "type": "invalid_request_error",
                "code": code,
            }
        })),
    )
        .into_response()
}

fn usage_storage_error_response() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": {
                "message": "Core usage query failed",
                "type": "server_error",
                "code": "core_error",
            }
        })),
    )
        .into_response()
}

pub(crate) async fn usage(
    State(state): State<Arc<ApiSharedState>>,
    principal: Option<Extension<Principal>>,
    Query(query): Query<UsageQuery>,
) -> Response {
    let Some(bridge) = state.core.as_ref() else {
        return usage_error_response(UsageError::NotEnabled);
    };
    let principal = principal.as_ref().map(|extension| &extension.0);
    let limit = match validate_usage_request(bridge.mode, principal, query.limit) {
        Ok(limit) => limit,
        Err(error) => return usage_error_response(error),
    };
    let Some(principal) = principal else {
        return usage_error_response(UsageError::Unauthorized);
    };
    match bridge.store.quota_usage_for_principal(principal, limit) {
        Ok(view) => Json(usage_payload(&view, limit)).into_response(),
        Err(_) => usage_storage_error_response(),
    }
}

#[cfg(test)]
mod tests {
    use aiwork_core::{CoreQuotaBalanceView, CoreQuotaLedgerView, CoreQuotaUsageView, Principal};
    use serde_json::Value;

    use super::*;

    fn principal(scopes: &[&str]) -> Principal {
        Principal {
            user_id: "usage-user".into(),
            key_id: "usage-key".into(),
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        }
    }

    fn view() -> CoreQuotaUsageView {
        CoreQuotaUsageView {
            balances: vec![CoreQuotaBalanceView {
                resource_kind: "chat_request".into(),
                available: 8,
                held: 2,
                settled: 4,
            }],
            ledger: vec![CoreQuotaLedgerView {
                resource_kind: "chat_request".into(),
                event_kind: "commit".into(),
                amount: 4,
                delta: 0,
                request_id: Some("request-1".into()),
                created_at_ms: 1,
            }],
        }
    }

    #[test]
    fn usage_gate_requires_enforce_mode_scope_and_bounded_limit() {
        let user = principal(&["usage:read"]);
        assert_eq!(validate_usage_request(CoreMode::Enforce, Some(&user), None), Ok(100));
        assert_eq!(validate_usage_request(CoreMode::Enforce, Some(&user), Some(1)), Ok(1));
        assert_eq!(
            validate_usage_request(CoreMode::Off, Some(&user), Some(1)),
            Err(UsageError::NotEnabled)
        );
        assert_eq!(
            validate_usage_request(CoreMode::Shadow, Some(&user), Some(1)),
            Err(UsageError::NotEnabled)
        );
        assert_eq!(
            validate_usage_request(CoreMode::Enforce, None, Some(1)),
            Err(UsageError::Unauthorized)
        );
        assert_eq!(
            validate_usage_request(CoreMode::Enforce, Some(&principal(&["chat:invoke"])), Some(1)),
            Err(UsageError::InsufficientScope)
        );
        for limit in [Some(0), Some(101)] {
            assert_eq!(
                validate_usage_request(CoreMode::Enforce, Some(&user), limit),
                Err(UsageError::InvalidLimit)
            );
        }
    }

    #[test]
    fn usage_payload_contains_only_the_user_scoped_projection() {
        let payload = usage_payload(&view(), 1);
        assert_eq!(payload["object"], "user_usage");
        assert_eq!(payload["limit"], 1);
        assert_eq!(payload["balances"][0]["held"], 2);
        assert_eq!(payload["balances"][0]["settled"], 4);
        let serialized = serde_json::to_string(&payload).unwrap();
        for forbidden in ["user_id", "actor_user_id", "reason", "prompt", "credential", "account_ref"] {
            assert!(!serialized.contains(forbidden), "usage payload leaked {forbidden}");
        }
        assert!(matches!(payload, Value::Object(_)));
    }
}
