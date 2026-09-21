use std::{collections::BTreeSet, sync::Arc};

use aiwork_core::Principal;
use axum::{extract::{Request, State}, http::{HeaderValue, StatusCode}, middleware::Next, response::{IntoResponse, Response}, Json};
use serde_json::json;

use crate::{admin_session::AdminSession, auth::{auth_response, require_core_principal}, state::StarlinkRouterState};

pub const SESSION_COOKIE_NAME: &str = "starlink_admin_session";

pub async fn require_admin(
    State(state): State<Arc<StarlinkRouterState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if let Some(token) = cookie_value(request.headers().get("cookie"), SESSION_COOKIE_NAME) {
        if let Some(session) = state.admin_sessions.lookup(token, now_ms()) {
            if let Err(response) = validate_session_request(&state, &request, &session) {
                return response;
            }
            let principal = session_principal(&session);
            request.extensions_mut().insert(session);
            request.extensions_mut().insert(principal);
            return next.run(request).await;
        }
    }

    let authorization = request.headers().get("authorization").and_then(|value| value.to_str().ok());
    match require_core_principal(&state.store, authorization)
        .and_then(|principal| state.store.authorize_admin_principal(&principal).map(|_| principal).map_err(|_| auth_response(StatusCode::FORBIDDEN, "admin_required", "需要管理员权限"))) {
        Ok(principal) => { request.extensions_mut().insert(principal); next.run(request).await }
        Err(response) => response,
    }
}

pub fn session_principal(session: &AdminSession) -> Principal {
    Principal { user_id: session.user_id.clone(), key_id: format!("admin_session:{}", session.token_hash), scopes: BTreeSet::from(["admin:*".to_string()]) }
}

pub fn session_cookie(token: &str, secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!("{}={}; Path=/admin; HttpOnly; SameSite=Strict{}; Max-Age={}", SESSION_COOKIE_NAME, token, if secure { "; Secure" } else { "" }, 12 * 60 * 60)).expect("session cookie is valid")
}

pub fn clear_session_cookie(secure: bool) -> HeaderValue {
    HeaderValue::from_str(&format!("{}=; Path=/admin; HttpOnly; SameSite=Strict{}; Max-Age=0", SESSION_COOKIE_NAME, if secure { "; Secure" } else { "" })).expect("session cookie is valid")
}

pub fn cookie_value<'a>(header: Option<&'a HeaderValue>, name: &str) -> Option<&'a str> {
    header?.to_str().ok()?.split(';').map(str::trim).find_map(|part| part.strip_prefix(&format!("{name}=")))
}

pub fn admin_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(json!({"error": {"type": "authentication_error", "code": code, "message": message}}))).into_response()
}

fn validate_session_request(state: &StarlinkRouterState, request: &Request, session: &AdminSession) -> Result<(), Response> {
    let principal = session_principal(session);
    state.store.authorize_admin_principal(&principal).map_err(|_| admin_error(StatusCode::FORBIDDEN, "admin_required", "需要管理员权限"))?;
    if request.method() != axum::http::Method::GET && request.method() != axum::http::Method::HEAD && request.method() != axum::http::Method::OPTIONS {
        if let Some(origin) = request.headers().get("origin").and_then(|value| value.to_str().ok()) {
            let host = request.headers().get("host").and_then(|value| value.to_str().ok()).unwrap_or_default();
            let same_origin = origin.rsplit_once("://").is_some_and(|(_, authority)| authority == host);
            if !same_origin {
                return Err(admin_error(StatusCode::FORBIDDEN, "csrf_origin_mismatch", "管理请求来源无效"));
            }
        }
    }
    if request.uri().path() != "/admin/v1/password" && request.uri().path() != "/admin/v1/logout" && request.uri().path() != "/admin/v1/session" {
        if let Ok(Some(credential)) = state.store.find_admin_credential("admin") {
            if credential.must_change_password {
                return Err(admin_error(StatusCode::FORBIDDEN, "password_change_required", "首次登录必须修改密码"));
            }
        }
    }
    Ok(())
}

fn now_ms() -> i64 { chrono::Utc::now().timestamp_millis() }

#[cfg(test)]
mod tests {
    use super::{cookie_value, SESSION_COOKIE_NAME};
    use axum::http::HeaderValue;

    #[test]
    fn cookie_parser_returns_only_requested_value() {
        let header = HeaderValue::from_static("other=x; starlink_admin_session=abc123; flag=yes");
        assert_eq!(cookie_value(Some(&header), SESSION_COOKIE_NAME), Some("abc123"));
    }
}
