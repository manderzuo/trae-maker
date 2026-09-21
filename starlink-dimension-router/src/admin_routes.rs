use std::{collections::BTreeSet, sync::Arc};

use aiwork_core::{CoreAdminSummary, NewUser, QuotaGrant, UsageTrendPoint, UserRole};
use axum::{extract::{Extension, Path, Query, State}, http::{HeaderMap, StatusCode}, response::{Html, IntoResponse, Response}, routing::{get, post, put}, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::{admin_auth::{admin_error, clear_session_cookie, cookie_value, session_cookie, session_principal, SESSION_COOKIE_NAME}, admin_session::{hash_password, verify_password}, bridge_config::{BridgeConfigCandidate, BridgeConfigStore}, migration, state::StarlinkRouterState};

#[derive(Debug, Deserialize)]
pub struct UserInput { pub id: String, pub name: String, #[serde(default = "default_role")] pub role: String }
fn default_role() -> String { "user".into() }

#[derive(Debug, Deserialize)]
pub struct ApiKeyInput {
    pub user_id: String,
    pub name: String,
    #[serde(default)] pub scopes: Vec<String>,
    #[serde(default = "default_max_concurrency")] pub max_concurrency: i64,
}
fn default_max_concurrency() -> i64 { 1 }

#[derive(Debug, Deserialize)]
pub struct UserQuotaInput { pub user_id: String, pub resource_kind: String, pub amount: i64, pub reason: String }

#[derive(Debug, Deserialize)]
pub struct MigrationInput { pub source_root: String, pub migration_id: String, pub confirmed: bool }

#[derive(Debug, Deserialize)]
pub struct BootstrapInput {
    pub id: String,
    pub name: String,
    #[serde(default = "default_admin_username")]
    pub username: String,
    #[serde(default)]
    pub password: Option<String>,
}

fn default_admin_username() -> String { "admin".into() }

#[derive(Debug, Deserialize)]
pub struct LoginInput { pub username: String, pub password: String }

#[derive(Debug, Deserialize)]
pub struct PasswordChangeInput { pub current_password: String, pub new_password: String }

#[derive(Debug, Serialize)]
pub struct SummaryResponse { pub core: CoreAdminSummary, pub bridge: serde_json::Value }

#[derive(Debug, Deserialize)]
pub struct TrendQuery {
    #[serde(default = "default_trend_window")]
    pub window: String,
}

#[derive(Debug, Serialize)]
pub struct UsageTrendResponse {
    pub window: String,
    pub start_ms: i64,
    pub end_ms: i64,
    pub bucket_ms: i64,
    pub points: Vec<UsageTrendPoint>,
}

pub fn router() -> Router<Arc<StarlinkRouterState>> {
    Router::new()
        .route("/admin/v1/summary", get(summary))
        .route("/admin/v1/usage-trend", get(usage_trend))
        .route("/admin/v1/bridge/status", get(bridge_status))
        .route("/admin/v1/bridge/test", post(bridge_test))
        .route("/admin/v1/bridge/config", put(bridge_config_save))
        .route("/admin/v1/users", get(users).post(create_user))
        .route("/admin/v1/api-keys", post(issue_key))
        .route("/admin/v1/api-keys/:key_id/revoke", post(revoke_key))
        .route("/admin/v1/quota/grant", post(grant_quota))
        .route("/admin/v1/migration/inspect", post(migration_inspect))
        .route("/admin/v1/migration/apply", post(migration_apply))
        .route("/admin/v1/logout", post(logout))
        .route("/admin/v1/password", post(change_password))
}

pub fn public_router() -> Router<Arc<StarlinkRouterState>> {
    Router::new()
        .route("/admin/v1/login", post(login))
        .route("/admin/v1/session", get(session))
        .route("/admin/v1/bootstrap", post(bootstrap))
}

pub async fn page() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

pub async fn bootstrap(State(state): State<Arc<StarlinkRouterState>>, Json(input): Json<BootstrapInput>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if input.username != "admin" {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_admin_username", "message": "管理员账户名必须是 admin"}}))));
    }
    if state.store.find_admin_credential(&input.username).map_err(internal)?.is_some() {
        return Err((StatusCode::CONFLICT, Json(json!({"error": {"type": "admin_already_initialized", "message": "管理员账户已经初始化"}}))));
    }
    let password = input.password.or_else(|| std::env::var("STARLINK_ADMIN_INITIAL_PASSWORD").ok()).ok_or_else(|| (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "initial_password_required", "message": "首次初始化必须提供初始密码"}}))))?;
    let user = state.store.create_bootstrap_admin(NewUser { id: input.id, name: input.name, role: UserRole::Admin }, "bootstrap").map_err(internal)?;
    let hash = hash_password(&password).map_err(internal)?;
    state.store.upsert_admin_credential(aiwork_core::NewAdminCredential { user_id: user.id.clone(), username: input.username.clone(), password_hash: hash.hash, salt: hash.salt, iterations: hash.iterations, must_change_password: true }).map_err(internal)?;
    Ok(Json(json!({"ok": true, "user_id": user.id, "username": input.username, "must_change_password": true})))
}

pub async fn login(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Json(input): Json<LoginInput>) -> Response {
    let username = input.username.trim().to_owned();
    let throttle_key = format!("{}:{}", headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()).unwrap_or("local"), username);
    let now = chrono::Utc::now().timestamp_millis();
    if !state.login_throttle.allow(&throttle_key, now) {
        return admin_error(StatusCode::TOO_MANY_REQUESTS, "admin_login_rate_limited", "登录尝试过于频繁，请稍后再试");
    }
    let credential = state.store.find_admin_credential(&username).ok().flatten();
    let Some(credential) = credential.filter(|record| verify_password(&input.password, record)) else {
        state.login_throttle.record_failure(&throttle_key, now);
        return admin_error(StatusCode::UNAUTHORIZED, "admin_login_failed", "账户或密码错误");
    };
    state.login_throttle.clear(&throttle_key);
    let (token, _) = state.admin_sessions.issue(credential.user_id, now);
    let mut response = Json(json!({"authenticated": true, "username": credential.username, "must_change_password": credential.must_change_password})).into_response();
    response.headers_mut().insert("set-cookie", session_cookie(&token, secure_cookie_from_headers(&headers)));
    response
}

pub async fn session(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap) -> Response {
    let Some(token) = cookie_value(headers.get("cookie"), SESSION_COOKIE_NAME) else {
        return Json(json!({"authenticated": false})).into_response();
    };
    let Some(session) = state.admin_sessions.lookup(token, chrono::Utc::now().timestamp_millis()) else {
        return Json(json!({"authenticated": false})).into_response();
    };
    let principal = session_principal(&session);
    if state.store.authorize_admin_principal(&principal).is_err() {
        return Json(json!({"authenticated": false})).into_response();
    }
    let must_change_password = state.store.find_admin_credential("admin").ok().flatten().map(|record| record.must_change_password).unwrap_or(false);
    Json(json!({"authenticated": true, "username": "admin", "must_change_password": must_change_password})).into_response()
}

pub async fn logout(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(headers.get("cookie"), SESSION_COOKIE_NAME) {
        state.admin_sessions.revoke(token);
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert("set-cookie", clear_session_cookie(secure_cookie_from_headers(&headers)));
    response
}

pub async fn change_password(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>, headers: HeaderMap, Json(input): Json<PasswordChangeInput>) -> Response {
    if input.new_password.len() < 10 {
        return admin_error(StatusCode::BAD_REQUEST, "password_too_short", "新密码至少需要 10 个字符");
    }
    let Some(credential) = state.store.find_admin_credential("admin").ok().flatten() else {
        return admin_error(StatusCode::UNAUTHORIZED, "admin_login_required", "管理员账户未初始化");
    };
    if credential.user_id != principal.user_id || !verify_password(&input.current_password, &credential) {
        return admin_error(StatusCode::UNAUTHORIZED, "password_change_failed", "当前密码错误");
    }
    let hash = match hash_password(&input.new_password) { Ok(value) => value, Err(_) => return admin_error(StatusCode::BAD_REQUEST, "password_change_failed", "新密码无效") };
    if state.store.mark_admin_password_changed("admin", hash.hash, hash.salt, hash.iterations).is_err() {
        return admin_error(StatusCode::BAD_REQUEST, "password_change_failed", "密码更新失败");
    }
    state.admin_sessions.revoke_user(&principal.user_id);
    let (token, _) = state.admin_sessions.issue(principal.user_id, chrono::Utc::now().timestamp_millis());
    let mut response = Json(json!({"ok": true, "must_change_password": false})).into_response();
    response.headers_mut().insert("set-cookie", session_cookie(&token, secure_cookie_from_headers(&headers)));
    response
}

fn secure_cookie_from_headers(headers: &HeaderMap) -> bool {
    std::env::var("STARLINK_ROUTER_SECURE_COOKIES").map(|value| value.eq_ignore_ascii_case("true")).unwrap_or(false)
        || headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()).is_some_and(|value| value.eq_ignore_ascii_case("https"))
}

async fn summary(State(state): State<Arc<StarlinkRouterState>>) -> Result<Json<SummaryResponse>, (StatusCode, Json<serde_json::Value>)> {
    let core = state.store.admin_summary(chrono::Utc::now().timestamp_millis()).map_err(internal)?;
    let bridge = BridgeConfigStore::new(state).public_status();
    Ok(Json(SummaryResponse { core, bridge }))
}

fn default_trend_window() -> String { "24h".into() }

fn trend_window(window: &str, end_ms: i64) -> Result<(i64, i64, i64), String> {
    let (range_ms, bucket_ms) = match window.trim().to_ascii_lowercase().as_str() {
        "24h" => (24 * 60 * 60 * 1_000, 60 * 60 * 1_000),
        "7d" => (7 * 24 * 60 * 60 * 1_000, 24 * 60 * 60 * 1_000),
        "30d" => (30 * 24 * 60 * 60 * 1_000, 24 * 60 * 60 * 1_000),
        other => return Err(format!("不支持的趋势时间范围：{other}")),
    };
    if end_ms <= range_ms {
        return Err("趋势时间范围无效".into());
    }
    Ok((end_ms - range_ms, end_ms, bucket_ms))
}

async fn usage_trend(State(state): State<Arc<StarlinkRouterState>>, Query(query): Query<TrendQuery>) -> Result<Json<UsageTrendResponse>, (StatusCode, Json<serde_json::Value>)> {
    let end_ms = chrono::Utc::now().timestamp_millis();
    let (start_ms, end_ms, bucket_ms) = trend_window(&query.window, end_ms).map_err(internal)?;
    let points = state.store.usage_trend(start_ms, end_ms, bucket_ms).map_err(internal)?;
    Ok(Json(UsageTrendResponse {
        window: query.window,
        start_ms,
        end_ms,
        bucket_ms,
        points,
    }))
}

async fn bridge_status(State(state): State<Arc<StarlinkRouterState>>) -> Json<serde_json::Value> {
    Json(BridgeConfigStore::new(state).public_status())
}

async fn bridge_test(Json(candidate): Json<BridgeConfigCandidate>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let client = crate::bridge_client::BridgeClient::new(candidate.base_url.trim(), candidate.api_key.trim());
    let value = client.test().map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_test_failed", "message": error}}))))?;
    Ok(Json(json!({"ok": true, "status": value})))
}

async fn bridge_config_save(State(state): State<Arc<StarlinkRouterState>>, Json(candidate): Json<BridgeConfigCandidate>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let saved = BridgeConfigStore::new(state).test_then_save(candidate).map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_config_failed", "message": error.to_string()}}))))?;
    Ok(Json(serde_json::to_value(saved).unwrap_or_else(|_| json!({"ok": true}))))
}

async fn users(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>) -> Result<Json<Vec<aiwork_core::CoreUserAdminView>>, (StatusCode, Json<serde_json::Value>)> {
    state.store.list_users_as_admin(&principal).map(Json).map_err(internal)
}

async fn create_user(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>, Json(input): Json<UserInput>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let role = match input.role.as_str() { "admin" => UserRole::Admin, "operator" => UserRole::Operator, _ => UserRole::User };
    state.store.create_user_as_admin(NewUser { id: input.id, name: input.name, role }, &principal).map(|user| Json(json!({"id": user.id, "name": user.name, "created": true}))).map_err(internal)
}

async fn issue_key(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>, Json(input): Json<ApiKeyInput>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if input.scopes.is_empty() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": "至少选择一个作用域"}}))));
    }
    let max_concurrency = input.max_concurrency;
    state.store.issue_api_key_as_admin_with_max_concurrency(&input.user_id, &input.name, input.scopes.into_iter().collect::<BTreeSet<_>>(), max_concurrency, &principal)
        .map(|key| Json(json!({"id": key.id, "user_id": key.user_id, "prefix": key.prefix, "plaintext": key.plaintext, "scopes": key.scopes, "max_concurrency": max_concurrency})))
        .map_err(internal)
}

async fn revoke_key(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>, Path(key_id): Path<String>) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    state.store.revoke_api_key_as_admin(&principal, &key_id).map_err(internal)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn grant_quota(State(state): State<Arc<StarlinkRouterState>>, Extension(principal): Extension<aiwork_core::Principal>, Json(input): Json<UserQuotaInput>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    state.store.grant_as_admin(QuotaGrant { user_id: input.user_id, resource_kind: input.resource_kind, amount: input.amount, actor_user_id: principal.user_id.clone(), reason: input.reason }, &principal)
        .map(|balance| Json(json!({"resource_kind": balance.resource_kind, "available": balance.available, "held": balance.held})))
        .map_err(internal)
}

async fn migration_inspect(Json(input): Json<MigrationInput>) -> Result<Json<migration::MigrationReport>, (StatusCode, Json<serde_json::Value>)> {
    migration::inspect(input.source_root, r"D:\gpt\starlink-dimension-router-data", &input.migration_id).map(Json).map_err(internal)
}

async fn migration_apply(Json(input): Json<MigrationInput>) -> Result<Json<migration::MigrationReport>, (StatusCode, Json<serde_json::Value>)> {
    let report = migration::inspect(&input.source_root, r"D:\gpt\starlink-dimension-router-data", &input.migration_id).map_err(internal)?;
    migration::apply(&report, input.confirmed).map(Json).map_err(internal)
}

fn internal<E: ToString>(error: E) -> (StatusCode, Json<serde_json::Value>) {
    (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "admin_operation_failed", "message": error.to_string()}})))
}

#[cfg(test)]
mod tests {
    use super::{default_role, trend_window};
    #[test]
    fn ordinary_users_default_to_user_role() { assert_eq!(default_role(), "user"); }

    #[test]
    fn usage_trend_window_uses_hour_or_day_buckets() {
        assert_eq!(trend_window("24h", 2_000_000_000_000).unwrap().2, 3_600_000);
        assert_eq!(trend_window("7d", 2_000_000_000_000).unwrap().2, 86_400_000);
        assert_eq!(trend_window("30d", 2_000_000_000_000).unwrap().2, 86_400_000);
        assert!(trend_window("all", 2_000_000_000_000).is_err());
    }

}
