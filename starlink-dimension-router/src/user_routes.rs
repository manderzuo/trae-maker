use std::{collections::BTreeMap, sync::Arc};

use aiwork_core::{CoreError, CoreStore, Principal, PreflightReserveInput, PreflightReserveResult, RequestResult, RequestState, Settlement};
use axum::{body::{Body, Bytes}, extract::{Extension, Path, Query, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use chrono::Utc;
use serde_json::{json, Value};

use crate::state::{StarlinkRouterState, UserVideoJob};

fn request_id() -> String { format!("core-{}-{:016x}", Utc::now().timestamp_millis(), rand::random::<u64>()) }

fn header_map(headers: &HeaderMap) -> BTreeMap<String, String> {
    headers.iter().filter_map(|(key, value)| value.to_str().ok().map(|value| (key.as_str().to_ascii_lowercase(), value.to_string()))).collect()
}

fn idempotency(headers: &HeaderMap) -> String {
    headers.get("idempotency-key").and_then(|value| value.to_str().ok()).filter(|value| !value.trim().is_empty()).map(ToString::to_string).unwrap_or_else(request_id)
}

fn authorize_scope(principal: &Principal, scope: &str) -> Result<(), Response> {
    if principal.scopes.contains(scope) || principal.scopes.contains("admin:*") { Ok(()) } else {
        Err((StatusCode::FORBIDDEN, Json(json!({"error": {"type": "permission_error", "code": "insufficient_scope", "message": format!("需要作用域 {scope}")}}))).into_response())
    }
}

pub async fn assets_upload(
    State(state): State<Arc<StarlinkRouterState>>,
    Extension(principal): Extension<Principal>,
    body: Bytes,
) -> Response {
    if let Err(response) = authorize_scope(&principal, "assets:write") { return response; }
    let parsed = match crate::assets::parse_upload(&body) {
        Ok(parsed) => parsed,
        Err(error) => return crate::assets::response(error),
    };
    let _permit = match state.asset_limiter.acquire(&principal.key_id, body.len()) {
        Ok(permit) => permit,
        Err(error) => return crate::assets::response(error),
    };
    let stored = match crate::assets::write_asset(&state.config.data_dir, &principal, parsed) {
        Ok(stored) => stored,
        Err(error) => return crate::assets::response(error),
    };
    let record = match crate::assets::persist_asset(&state.store, &principal, &stored) {
        Ok(record) => record,
        Err(error) => return crate::assets::response(error),
    };
    Json(json!({
        "object": "asset",
        "id": record.id,
        "filename": record.filename,
        "mime_type": record.mime_type,
        "bytes": record.size,
        "sha256": record.sha256,
        "created_at": record.created_at_ms / 1000,
        "expires_at": record.expires_at_ms / 1000,
        "content_url": crate::assets::content_url(&state.config, &record.id, &stored.public_token),
    })).into_response()
}

#[derive(serde::Deserialize)]
pub struct AssetContentQuery {
    token: Option<String>,
}

pub async fn assets_content(
    State(state): State<Arc<StarlinkRouterState>>,
    Path(asset_id): Path<String>,
    Query(query): Query<AssetContentQuery>,
) -> Response {
    let Some(token) = query.token else { return crate::assets::response(crate::assets::AssetError::NotFound); };
    let asset = match crate::assets::read_public(&state.store, &state.config.data_dir, &asset_id, &token) {
        Ok(asset) => asset,
        Err(error) => return crate::assets::response(error),
    };
    let mut response = Response::new(Body::from(asset.bytes));
    response.headers_mut().insert("content-type", asset.record.mime_type.parse().unwrap_or_else(|_| "application/octet-stream".parse().unwrap()));
    response.headers_mut().insert("cache-control", "no-store".parse().unwrap());
    response.headers_mut().insert("x-content-type-options", "nosniff".parse().unwrap());
    response.headers_mut().insert("referrer-policy", "no-referrer".parse().unwrap());
    response
}

fn preflight(store: &CoreStore, principal: &Principal, endpoint: &str, model: &str, key: String, body: &Value) -> Result<PreflightReserveResult, Response> {
    store.preflight_reserve(PreflightReserveInput {
        request: aiwork_core::BeginRequestInput {
        user_id: principal.user_id.clone(),
        api_key_id: principal.key_id.clone(),
        protocol: "openai".into(),
        endpoint: endpoint.into(),
        model: model.into(),
        idempotency_key: key,
        body: body.clone(),
        },
        resource_kind: "credits".into(),
        amount: 1,
        ttl_ms: 120_000,
    }).map_err(|error| {
        let (status, error_type, message) = match &error {
            CoreError::KeyConcurrencyExceeded { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "concurrency_limit",
                error.to_string(),
            ),
            CoreError::KeyQuotaNotConfigured { .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                "insufficient_quota",
                "该 API Key 尚未分配积分额度，请联系管理员配置后再试".to_owned(),
            ),
            _ => (StatusCode::BAD_REQUEST, "core_error", error.to_string()),
        };
        (status, Json(json!({"error": {"type": error_type, "message": message}}))).into_response()
    })
}

pub async fn models(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(_principal): Extension<Principal>) -> Response {
    let request_id = request_id();
    match state.bridge.lock().unwrap().forward("GET", "/v1/models", &[], &header_map(&headers), &request_id) {
        Ok(response) => proxy(response.status, response.headers, response.body),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response(),
    }
}

pub async fn chat_completions(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(principal): Extension<Principal>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": "请求体必须是 JSON"}}))).into_response(),
    };
    if let Err(response) = authorize_scope(&principal, "chat:invoke") { return response; }
    let model = value.get("model").and_then(Value::as_str).unwrap_or(&state.config.default_model).to_string();
    let prepared = match preflight(&state.store, &principal, "chat", &model, idempotency(&headers), &value) { Ok(value) => value, Err(response) => return response };
    let (request_handle, reservation) = match prepared {
        PreflightReserveResult::Conflict => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        PreflightReserveResult::Insufficient { available, required } => return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"type": "insufficient_quota", "message": format!("积分不足：可用 {available}，需要 {required}")}}))).into_response(),
        PreflightReserveResult::Existing { request, reservation } => return Json(json!({"id": request.id, "object": "chat.completion", "choices": [], "core_replay": true, "reservation": reservation.map(|value| value.id)})).into_response(),
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
    };
    let request_id = request_handle.id.clone();
    match state.bridge.lock().unwrap().forward("POST", "/v1/chat/completions", &body, &header_map(&headers), &request_id) {
        Ok(response) if (200..300).contains(&response.status) => { let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Commit { actual_amount: Some(1) }, RequestState::Succeeded, Some(RequestResult { status: Some(response.status as i64), error_code: None })); proxy(response.status, response.headers, response.body) }
        Ok(response) => { let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status as i64), error_code: Some("bridge_http_error".into()) })); proxy(response.status, response.headers, response.body) }
        Err(error) => { let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Unknown, RequestState::Unknown, Some(RequestResult { status: None, error_code: Some("bridge_result_unknown".into()) })); (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id}}))).into_response() }
    }
}

pub async fn video_generations(State(state): State<Arc<StarlinkRouterState>>, headers: HeaderMap, Extension(principal): Extension<Principal>, body: Bytes) -> Response {
    let value: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": "请求体必须是 JSON"}}))).into_response(),
    };
    if let Err(response) = authorize_scope(&principal, "videos:submit") { return response; }
    let model = value.get("model").and_then(Value::as_str).unwrap_or("seedance").to_string();
    let prepared = match preflight(&state.store, &principal, "videos", &model, idempotency(&headers), &value) { Ok(value) => value, Err(response) => return response };
    let (request_handle, reservation) = match prepared {
        PreflightReserveResult::Conflict => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        PreflightReserveResult::Insufficient { available, required } => return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"type": "insufficient_quota", "message": format!("积分不足：可用 {available}，需要 {required}")}}))).into_response(),
        PreflightReserveResult::Existing { request, reservation } => return Json(json!({"id": request.id, "status": "replay", "core_replay": true, "reservation": reservation.map(|value| value.id)})).into_response(),
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
    };
    let request_id = request_handle.id.clone();
    let response = match state.bridge.lock().unwrap().forward("POST", "/v1/videos/generations", &body, &header_map(&headers), &request_id) {
        Ok(response) => response,
        Err(error) => {
            let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Unknown, RequestState::Unknown, Some(RequestResult { status: None, error_code: Some("bridge_result_unknown".into()) }));
            let task_id = format!("task-{request_id}");
            state.jobs.lock().unwrap().insert(task_id.clone(), UserVideoJob { id: task_id, user_id: principal.user_id.clone(), request_id: request_id.clone(), upstream_id: None, status: "reconcile_required".into(), output_ref: None, error_code: Some("bridge_result_unknown".into()), reconcile_required: true });
            state.persist_jobs();
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id, "reconcile_required": true}}))).into_response();
        }
    };
    if !(200..300).contains(&response.status) {
        let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status as i64), error_code: Some("bridge_http_error".into()) }));
        return proxy(response.status, response.headers, response.body);
    }
    let upstream_id = serde_json::from_slice::<Value>(&response.body).ok().and_then(|value| value.get("id").and_then(Value::as_str).map(ToString::to_string));
    let task_id = upstream_id.clone().unwrap_or_else(|| format!("task-{}", request_id));
    state.jobs.lock().unwrap().insert(task_id.clone(), UserVideoJob { id: task_id, user_id: principal.user_id.clone(), request_id: request_id.clone(), upstream_id, status: "queued".into(), output_ref: None, error_code: None, reconcile_required: false });
    state.persist_jobs();
    let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Commit { actual_amount: Some(1) }, RequestState::Succeeded, Some(RequestResult { status: Some(response.status as i64), error_code: None }));
    proxy(response.status, response.headers, response.body)
}

pub async fn video_task(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}");
    match state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id) { Ok(response) => proxy(response.status, response.headers, response.body), Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response() }
}

pub async fn video_content(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}/content");
    match state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id) { Ok(response) => proxy(response.status, response.headers, response.body), Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response() }
}

fn proxy(status: u16, headers: BTreeMap<String, String>, body: Vec<u8>) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
    if let Some(content_type) = headers.get("content-type") { response.headers_mut().insert("content-type", content_type.parse().unwrap_or_else(|_| "application/json".parse().unwrap())); }
    response
}

#[cfg(test)]
mod tests {
    use super::authorize_scope;
    use aiwork_core::Principal;
    use std::collections::BTreeSet;

    #[test]
    fn ordinary_user_scope_is_checked_before_forwarding() {
        let principal = Principal { user_id: "u".into(), key_id: "k".into(), scopes: BTreeSet::from(["chat:invoke".into()]) };
        assert!(authorize_scope(&principal, "videos:submit").is_err());
        assert!(authorize_scope(&principal, "chat:invoke").is_ok());
    }
}
