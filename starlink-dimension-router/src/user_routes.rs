use std::{collections::{BTreeMap, BTreeSet, HashMap}, sync::Arc};

use aiwork_core::{CoreError, CoreStore, Principal, PreflightReserveInput, PreflightReserveResult, RequestResult, RequestState, Settlement};
use axum::{body::{Body, Bytes}, extract::{Extension, Path, Query, State}, http::{HeaderMap, StatusCode}, response::{IntoResponse, Response}, Json};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::{json, Value};

use crate::state::{StarlinkRouterState, UserVideoJob};
use crate::video_billing::{admit_video_request, VideoAdmission};

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

const MAX_VISION_DATA_URL_BYTES: usize = 6 * 1024 * 1024;

fn is_seedance_model(model: &str) -> bool { model.trim().eq_ignore_ascii_case("seedance") }

fn request_error(status: StatusCode, error_type: &str, message: impl Into<String>) -> Response {
    (status, Json(json!({"error": {"type": error_type, "message": message.into()}}))).into_response()
}

fn validate_vision_data_urls(body: &Value) -> Result<(), Response> {
    let mut total = 0_usize;
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return Ok(()); };
    for message in messages {
        let Some(parts) = message.get("content").and_then(Value::as_array) else { continue; };
        for part in parts {
            let Some(url) = part.get("image_url").and_then(Value::as_object).and_then(|image| image.get("url")).and_then(Value::as_str) else { continue; };
            if url.starts_with("data:") {
                total = total.saturating_add(url.len());
                if total > MAX_VISION_DATA_URL_BYTES {
                    return Err(request_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "图片 data URL 总大小超过限制"));
                }
            }
        }
    }
    Ok(())
}

fn materialize_text_asset_ids(
    state: &StarlinkRouterState,
    principal: &Principal,
    body: &mut Value,
) -> Result<(), Response> {
    if body.get("video_asset_ids").is_some() {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "文字模型不支持 video_asset_ids"));
    }
    let Some(value) = body.get("image_asset_ids") else { return Ok(()); };
    let Some(ids) = value.as_array() else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "image_asset_ids 必须是字符串数组"));
    };
    let mut image_parts = Vec::with_capacity(ids.len());
    let mut total = 0_usize;
    for value in ids {
        let Some(id) = value.as_str().map(str::trim).filter(|value| !value.is_empty()) else {
            return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "image_asset_ids 只能包含非空字符串"));
        };
        let asset = crate::assets::read_owned(&state.store, &state.config.data_dir, principal, id)
            .map_err(crate::assets::response)?;
        let encoded = STANDARD.encode(asset.bytes);
        let url = format!("data:{};base64,{}", asset.record.mime_type, encoded);
        total = total.saturating_add(url.len());
        if total > MAX_VISION_DATA_URL_BYTES {
            return Err(request_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "图片 data URL 总大小超过限制"));
        }
        image_parts.push(json!({"type":"image_url","image_url":{"url":url}}));
    }
    if image_parts.is_empty() {
        body.as_object_mut().map(|object| { object.remove("image_asset_ids"); });
        return Ok(());
    }
    let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "使用 image_asset_ids 时必须提供 messages"));
    };
    let Some(message) = messages.iter_mut().rev().find(|message| message.get("role").and_then(Value::as_str).map(|role| role.eq_ignore_ascii_case("user")).unwrap_or(false)) else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "使用 image_asset_ids 时必须有 user 消息"));
    };
    let Some(content) = message.get_mut("content") else {
        return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "user 消息缺少 content"));
    };
    match content {
        Value::String(text) => {
            let mut parts = vec![json!({"type":"text","text":text.clone()})];
            parts.extend(image_parts);
            *content = Value::Array(parts);
        }
        Value::Array(parts) => parts.extend(image_parts),
        _ => return Err(request_error(StatusCode::BAD_REQUEST, "invalid_request_error", "user content 必须是字符串或数组")),
    }
    if let Some(object) = body.as_object_mut() { object.remove("image_asset_ids"); }
    Ok(())
}

#[derive(Default)]
struct BridgeAssetMap {
    image: Vec<String>,
    video: Vec<String>,
}

struct PendingAsset {
    core_id: String,
    filename: String,
    mime_type: String,
    bytes: Vec<u8>,
}

async fn materialize_bridge_assets(
    state: &StarlinkRouterState,
    principal: &Principal,
    body: &mut Value,
    request_id: &str,
) -> Result<BridgeAssetMap, Response> {
    let mut requested = Vec::<(String, String)>::new();
    for field in ["image_asset_ids", "video_asset_ids"] {
        let Some(value) = body.get(field) else { continue; };
        let Some(ids) = value.as_array() else {
            return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": format!("{field} 必须是字符串数组")}}))).into_response());
        };
        for id in ids {
            let Some(id) = id.as_str().map(str::trim).filter(|value| !value.is_empty()) else {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error": {"type": "invalid_request_error", "message": format!("{field} 只能包含非空字符串")}}))).into_response());
            };
            requested.push((field.to_string(), id.to_string()));
        }
    }
    if requested.is_empty() { return Ok(BridgeAssetMap::default()); }

    let mut pending = Vec::<PendingAsset>::new();
    let mut seen = HashMap::<String, usize>::new();
    for (_, id) in &requested {
        if seen.contains_key(id) { continue; }
        let asset = crate::assets::read_owned(&state.store, &state.config.data_dir, principal, id)
            .map_err(crate::assets::response)?;
        seen.insert(id.clone(), pending.len());
        pending.push(PendingAsset { core_id: id.clone(), filename: asset.record.filename, mime_type: asset.record.mime_type, bytes: asset.bytes });
    }

    let mut bridge_ids = HashMap::<String, String>::new();
    for asset in pending {
        let bridge_id = state.bridge.lock().unwrap().upload_asset(&asset.filename, &asset.mime_type, &asset.bytes, request_id)
            .map_err(|error| (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id}}))).into_response())?;
        bridge_ids.insert(asset.core_id, bridge_id);
    }

    let mut result = BridgeAssetMap::default();
    for (field, id) in requested {
        let bridge_id = bridge_ids.get(&id).expect("bridge asset map must contain every validated asset").clone();
        if field == "image_asset_ids" { result.image.push(bridge_id.clone()); } else { result.video.push(bridge_id.clone()); }
        let values = body.get_mut(&field).and_then(Value::as_array_mut).expect("asset id field was validated as array");
        for value in values { if value.as_str() == Some(id.as_str()) { *value = Value::String(bridge_id.clone()); } }
    }
    Ok(result)
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

fn require_video_admission(
    state: &StarlinkRouterState,
    principal: &Principal,
    model: &str,
    body: &Value,
) -> Result<VideoAdmission, Response> {
    match admit_video_request(&state.store, principal, model, body) {
        Ok(VideoAdmission::Active | VideoAdmission::DiagnosticClaimed) => Ok(
            VideoAdmission::Active,
        ),
        Ok(VideoAdmission::Paused) => Err(request_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "video_billing_paused",
            "视频计费闸门当前处于暂停状态，尚未取得可核验的单任务积分回执",
        )),
        Err(error) => Err(request_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "core_error",
            error.to_string(),
        )),
    }
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
    let model = value.get("model").and_then(Value::as_str).unwrap_or(&state.config.default_model).to_string();
    let seedance = is_seedance_model(&model);
    if seedance && value.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return request_error(StatusCode::BAD_REQUEST, "seedance_stream_unsupported", "Seedance Chat 兼容入口暂不支持 stream=true，请使用非流式请求");
    }
    if let Err(response) = authorize_scope(&principal, if seedance { "videos:submit" } else { "chat:invoke" }) { return response; }
    if seedance {
        if let Err(response) = require_video_admission(&state, &principal, &model, &value) {
            return response;
        }
    }
    let endpoint = if seedance { "videos" } else { "chat" };
    let prepared = match preflight(&state.store, &principal, endpoint, &model, idempotency(&headers), &value) { Ok(value) => value, Err(response) => return response };
    let (request_handle, reservation) = match prepared {
        PreflightReserveResult::Conflict => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        PreflightReserveResult::Insufficient { available, required } => return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"type": "insufficient_quota", "message": format!("积分不足：可用 {available}，需要 {required}")}}))).into_response(),
        PreflightReserveResult::Existing { request, reservation } => {
            if seedance {
                let job = state.jobs.lock().unwrap().values().find(|job| job.request_id == request.id && job.user_id == principal.user_id).cloned();
                return match job {
                    Some(job) if !job.reconcile_required && job.upstream_id.is_some() => Json(json!({"task": {"id": job.id, "status": job.status}, "core_replay": true})).into_response(),
                    _ => (StatusCode::CONFLICT, Json(json!({"error": {"type": "reconcile_required", "message": "任务结果尚未确认，请勿重复提交", "request_id": request.id}}))).into_response(),
                };
            }
            return Json(json!({"id": request.id, "object": "chat.completion", "choices": [], "core_replay": true, "reservation": reservation.map(|value| value.id)})).into_response();
        },
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
    };
    let request_id = request_handle.id.clone();
    let mut forward_value = value;
    let has_asset_ids = forward_value.get("image_asset_ids").is_some() || forward_value.get("video_asset_ids").is_some();
    if seedance {
        if has_asset_ids {
            if let Err(response) = materialize_bridge_assets(&state, &principal, &mut forward_value, &request_id).await {
                let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status().as_u16() as i64), error_code: Some("asset_materialization_failed".into()) }));
                return response;
            }
        }
        if let Err(response) = validate_vision_data_urls(&forward_value) {
            let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status().as_u16() as i64), error_code: Some("vision_input_invalid".into()) }));
            return response;
        }
    } else {
        if let Err(response) = materialize_text_asset_ids(&state, &principal, &mut forward_value) {
            let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status().as_u16() as i64), error_code: Some("vision_input_invalid".into()) }));
            return response;
        }
        if let Err(response) = validate_vision_data_urls(&forward_value) {
            let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status().as_u16() as i64), error_code: Some("vision_input_invalid".into()) }));
            return response;
        }
    }
    let forward_body = if has_asset_ids || !seedance && forward_value.get("image_asset_ids").is_none() && forward_value != serde_json::from_slice::<Value>(&body).unwrap_or(Value::Null) {
        serde_json::to_vec(&forward_value).unwrap_or_else(|_| body.to_vec())
    } else {
        body.to_vec()
    };
    if seedance {
        return match state.bridge.lock().unwrap().forward("POST", "/v1/chat/completions", &forward_body, &header_map(&headers), &request_id) {
            Ok(response) if (200..300).contains(&response.status) => {
                let value = serde_json::from_slice::<Value>(&response.body).unwrap_or(Value::Null);
                let Some(task_id) = extract_upstream_task_id(&value) else {
                    let task_id = format!("task-{request_id}");
                    state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id.clone(), Some(reservation.id.clone()), "bridge_task_id_missing"));
                    state.persist_jobs();
                    return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "reconcile_required", "message": "上游已接受请求但没有返回可查询的视频任务 ID，请勿重复提交", "request_id": request_id}}))).into_response();
                };
                state.jobs.lock().unwrap().insert(task_id.clone(), accepted_video_job(&principal, &request_id, task_id, Some(reservation.id.clone()), "queued", None));
                state.persist_jobs();
                proxy(response.status, response.headers, response.body)
            }
            Ok(response) => {
                let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status as i64), error_code: Some("bridge_http_error".into()) }));
                proxy(response.status, response.headers, response.body)
            }
            Err(error) => {
                let task_id = format!("task-{request_id}");
                state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id, Some(reservation.id.clone()), "bridge_result_unknown"));
                state.persist_jobs();
                (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id, "reconcile_required": true}}))).into_response()
            }
        };
    }
    match state.bridge.lock().unwrap().forward("POST", "/v1/chat/completions", &forward_body, &header_map(&headers), &request_id) {
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
    if let Err(response) = require_video_admission(&state, &principal, &model, &value) {
        return response;
    }
    let prepared = match preflight(&state.store, &principal, "videos", &model, idempotency(&headers), &value) { Ok(value) => value, Err(response) => return response };
    let (request_handle, reservation) = match prepared {
        PreflightReserveResult::Conflict => return (StatusCode::CONFLICT, Json(json!({"error": {"type": "idempotency_conflict", "message": "Idempotency-Key 与历史请求内容不一致"}}))).into_response(),
        PreflightReserveResult::Insufficient { available, required } => return (StatusCode::TOO_MANY_REQUESTS, Json(json!({"error": {"type": "insufficient_quota", "message": format!("积分不足：可用 {available}，需要 {required}")}}))).into_response(),
        PreflightReserveResult::Existing { request, .. } => {
            let job = state.jobs.lock().unwrap().values().find(|job| job.request_id == request.id && job.user_id == principal.user_id).cloned();
            return match job {
                Some(job) if !job.reconcile_required && job.upstream_id.is_some() => Json(json!({"task": {"id": job.id, "status": job.status}, "core_replay": true})).into_response(),
                _ => (StatusCode::CONFLICT, Json(json!({"error": {"type": "reconcile_required", "message": "任务结果尚未确认，请勿重复提交", "request_id": request.id}}))).into_response(),
            };
        },
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
    };
    let request_id = request_handle.id.clone();
    let mut forward_value = value;
    let has_asset_ids = forward_value.get("image_asset_ids").is_some() || forward_value.get("video_asset_ids").is_some();
    if has_asset_ids {
        if let Err(response) = materialize_bridge_assets(&state, &principal, &mut forward_value, &request_id).await {
            let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status().as_u16() as i64), error_code: Some("asset_materialization_failed".into()) }));
            return response;
        }
    }
    let forward_body = if has_asset_ids {
        match serde_json::to_vec(&forward_value) {
            Ok(body) => body,
            Err(error) => {
                let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(500), error_code: Some("asset_request_encoding_failed".into()) }));
                return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": {"type": "internal_error", "message": error.to_string()}}))).into_response();
            }
        }
    } else { body.to_vec() };
    let response = match state.bridge.lock().unwrap().forward("POST", "/v1/videos/generations", &forward_body, &header_map(&headers), &request_id) {
        Ok(response) => response,
        Err(error) => {
            let task_id = format!("task-{request_id}");
            state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id, Some(reservation.id.clone()), "bridge_result_unknown"));
            state.persist_jobs();
            return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error, "request_id": request_id, "reconcile_required": true}}))).into_response();
        }
    };
    if !(200..300).contains(&response.status) {
        let _ = state.store.settle_request(&principal, &reservation.id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(response.status as i64), error_code: Some("bridge_http_error".into()) }));
        return proxy(response.status, response.headers, response.body);
    }
    let upstream_id = serde_json::from_slice::<Value>(&response.body).ok().and_then(|value| {
        extract_upstream_task_id(&value)
    });
    let Some(task_id) = upstream_id else {
        let task_id = format!("task-{request_id}");
        state.jobs.lock().unwrap().insert(task_id.clone(), unknown_video_job(&principal, &request_id, task_id.clone(), Some(reservation.id.clone()), "bridge_task_id_missing"));
        state.persist_jobs();
        return (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "reconcile_required", "message": "上游已接受请求但没有返回可查询的视频任务 ID，请勿重复提交", "request_id": request_id}}))).into_response();
    };
    state.jobs.lock().unwrap().insert(task_id.clone(), accepted_video_job(&principal, &request_id, task_id, Some(reservation.id.clone()), "queued", None));
    state.persist_jobs();
    proxy(response.status, response.headers, response.body)
}

pub async fn video_task(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}");
    let result = state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id);
    match result {
        Ok(response) => {
            if (200..300).contains(&response.status) {
                match serde_json::from_slice::<Value>(&response.body) {
                    Ok(value) => { let _ = settle_video_job(&state, &task_id, &value); }
                    Err(_) => mark_video_reconcile(&state, &task_id, "video_status_invalid_json", None),
                }
            } else {
                mark_video_reconcile(&state, &task_id, "video_status_bridge_http_error", None);
            }
            proxy(response.status, response.headers, response.body)
        }
        Err(error) => {
            mark_video_reconcile(&state, &task_id, "video_status_bridge_error", None);
            (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response()
        }
    }
}

pub async fn video_content(State(state): State<Arc<StarlinkRouterState>>, Path(task_id): Path<String>, headers: HeaderMap, Extension(principal): Extension<Principal>) -> Response {
    let job = match state.jobs.lock().unwrap().get(&task_id).cloned() { Some(job) if job.user_id == principal.user_id => job, Some(_) => return StatusCode::NOT_FOUND.into_response(), None => return StatusCode::NOT_FOUND.into_response() };
    let upstream_id = job.upstream_id.as_deref().unwrap_or(&task_id);
    let path = format!("/v1/videos/{upstream_id}/content");
    match state.bridge.lock().unwrap().forward("GET", &path, &[], &header_map(&headers), &job.request_id) { Ok(response) => proxy(response.status, response.headers, response.body), Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({"error": {"type": "bridge_error", "message": error}}))).into_response() }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoBillingDecision {
    Held,
    Settled,
    Released,
    ReconcileRequired,
}

fn extract_upstream_task_id(value: &Value) -> Option<String> {
    value
        .pointer("/task/id")
        .or_else(|| value.pointer("/data/task/id"))
        .or_else(|| value.get("id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn extract_upstream_task_status(value: &Value) -> Option<String> {
    value
        .pointer("/task/status")
        .or_else(|| value.pointer("/data/task/status"))
        .or_else(|| value.get("status"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .map(ToString::to_string)
}

fn extract_billing_value(value: &Value) -> Option<&Value> {
    value
        .pointer("/task/billing")
        .or_else(|| value.pointer("/data/task/billing"))
        .or_else(|| value.get("billing"))
}

fn normalize_credit_decimal(value: &Value) -> Option<String> {
    let raw = match value {
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.trim().to_owned(),
        _ => return None,
    };
    if raw.is_empty() || raw.len() > 64 || raw.starts_with('-') || raw.contains(['e', 'E']) {
        return None;
    }
    let (whole, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    if whole.is_empty()
        || whole.len() > 24
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 6
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let mut canonical = String::with_capacity(whole.len() + 7);
    canonical.push_str(whole);
    canonical.push('.');
    canonical.push_str(fraction);
    for _ in fraction.len()..6 {
        canonical.push('0');
    }
    Some(canonical)
}

fn whole_credit_amount(canonical: &str) -> Option<i64> {
    let (whole, fraction) = canonical.split_once('.')?;
    if fraction.bytes().any(|byte| byte != b'0') {
        return None;
    }
    whole.parse::<i64>().ok()
}

fn update_video_job(state: &StarlinkRouterState, task_id: &str, update: impl FnOnce(&mut UserVideoJob)) {
    let changed = {
        let mut jobs = state.jobs.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(job) = jobs.get_mut(task_id) {
            update(job);
            true
        } else {
            false
        }
    };
    if changed {
        state.persist_jobs();
    }
}

fn mark_video_reconcile(
    state: &StarlinkRouterState,
    task_id: &str,
    error_code: &str,
    actual_credits: Option<String>,
) {
    let now = Utc::now().timestamp_millis();
    update_video_job(state, task_id, |job| {
        if job.billing_state == "settled" || job.billing_state == "released" {
            return;
        }
        job.status = "reconcile_required".into();
        job.billing_state = "reconcile_required".into();
        job.reconcile_required = true;
        job.error_code = Some(error_code.into());
        if actual_credits.is_some() {
            job.actual_credits = actual_credits;
        }
        job.last_reconciled_at_ms = Some(now);
    });
}

fn accepted_video_job(
    principal: &Principal,
    request_id: &str,
    task_id: String,
    reservation_id: Option<String>,
    status: &str,
    error_code: Option<String>,
) -> UserVideoJob {
    UserVideoJob {
        id: task_id.clone(),
        user_id: principal.user_id.clone(),
        api_key_id: principal.key_id.clone(),
        request_id: request_id.into(),
        upstream_id: Some(task_id),
        status: status.into(),
        output_ref: None,
        error_code,
        reconcile_required: false,
        reservation_id,
        billing_state: "held".into(),
        actual_credits: None,
        last_reconciled_at_ms: None,
    }
}

fn unknown_video_job(
    principal: &Principal,
    request_id: &str,
    task_id: String,
    reservation_id: Option<String>,
    error_code: &str,
) -> UserVideoJob {
    UserVideoJob {
        id: task_id,
        user_id: principal.user_id.clone(),
        api_key_id: principal.key_id.clone(),
        request_id: request_id.into(),
        upstream_id: None,
        status: "reconcile_required".into(),
        output_ref: None,
        error_code: Some(error_code.into()),
        reconcile_required: true,
        reservation_id,
        billing_state: "reconcile_required".into(),
        actual_credits: None,
        last_reconciled_at_ms: None,
    }
}

pub(crate) fn settle_video_job(
    state: &StarlinkRouterState,
    job_id: &str,
    upstream: &Value,
) -> Result<VideoBillingDecision, CoreError> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(job_id)
        .cloned()
        .ok_or_else(|| CoreError::InvalidConfiguration {
            key: "video_jobs".into(),
            value: job_id.into(),
        })?;
    if job.billing_state == "settled" {
        return Ok(VideoBillingDecision::Settled);
    }
    if job.billing_state == "released" {
        return Ok(VideoBillingDecision::Released);
    }

    let Some(upstream_id) = job.upstream_id.as_deref() else {
        mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let Some(observed_task_id) = extract_upstream_task_id(upstream) else {
        mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    if observed_task_id != upstream_id {
        mark_video_reconcile(state, job_id, "upstream_task_id_mismatch", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    let Some(status) = extract_upstream_task_status(upstream) else {
        mark_video_reconcile(state, job_id, "upstream_status_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let terminal_success = matches!(status.as_str(), "completed" | "succeeded" | "success");
    let terminal_failure = matches!(status.as_str(), "failed" | "error" | "canceled" | "cancelled");
    let Some(billing) = extract_billing_value(upstream) else {
        if terminal_success || terminal_failure {
            mark_video_reconcile(state, job_id, "billing_receipt_missing", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
        update_video_job(state, job_id, |stored| {
            stored.status = status.clone();
            stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
        });
        return Ok(VideoBillingDecision::Held);
    };

    let actual_credits = billing
        .get("actual_credits")
        .and_then(normalize_credit_decimal);
    let receipt_is_verified = billing.get("status").and_then(Value::as_str) == Some("verified")
        && billing.get("unit").and_then(Value::as_str) == Some("credits")
        && billing.get("task_ref").and_then(Value::as_str) == Some(upstream_id);
    if !receipt_is_verified || actual_credits.is_none() {
        if terminal_success || terminal_failure {
            mark_video_reconcile(state, job_id, "billing_receipt_unverified", actual_credits);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
        update_video_job(state, job_id, |stored| {
            stored.status = status.clone();
            stored.actual_credits = actual_credits.clone();
            stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
        });
        return Ok(VideoBillingDecision::Held);
    }

    let actual_credits = actual_credits.expect("checked above");
    let Some(actual_amount) = whole_credit_amount(&actual_credits) else {
        mark_video_reconcile(state, job_id, "fractional_credits_require_reconciliation", Some(actual_credits));
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let Some(reservation_id) = job.reservation_id.as_deref() else {
        mark_video_reconcile(state, job_id, "reservation_id_missing", Some(actual_credits));
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let Some(reservation) = state.store.reservation_for_request(&job.request_id)? else {
        mark_video_reconcile(state, job_id, "reservation_missing", Some(actual_credits));
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    if reservation.id != reservation_id || actual_amount < 0 || actual_amount > reservation.amount {
        mark_video_reconcile(state, job_id, "actual_credits_exceed_hold", Some(actual_credits));
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    if !terminal_success && !terminal_failure {
        update_video_job(state, job_id, |stored| {
            stored.status = status.clone();
            stored.actual_credits = Some(actual_credits.clone());
            stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
        });
        return Ok(VideoBillingDecision::Held);
    }

    let principal = Principal {
        user_id: job.user_id.clone(),
        key_id: job.api_key_id.clone(),
        scopes: BTreeSet::new(),
    };
    let final_state = if terminal_success { RequestState::Succeeded } else { RequestState::Failed };
    state.store.settle_request(
        &principal,
        reservation_id,
        Settlement::Commit { actual_amount: Some(actual_amount) },
        final_state,
        Some(RequestResult { status: Some(if terminal_success { 200 } else { 500 }), error_code: None }),
    )?;
    update_video_job(state, job_id, |stored| {
        stored.status = status.clone();
        stored.reconcile_required = false;
        stored.billing_state = "settled".into();
        stored.actual_credits = Some(actual_credits.clone());
        stored.error_code = None;
        stored.last_reconciled_at_ms = Some(Utc::now().timestamp_millis());
    });
    Ok(VideoBillingDecision::Settled)
}

pub(crate) fn reconcile_video_job_once(
    state: &StarlinkRouterState,
    job_id: &str,
) -> Result<VideoBillingDecision, CoreError> {
    let job = state
        .jobs
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(job_id)
        .cloned()
        .ok_or_else(|| CoreError::InvalidConfiguration {
            key: "video_jobs".into(),
            value: job_id.into(),
        })?;
    let Some(upstream_id) = job.upstream_id.as_deref() else {
        mark_video_reconcile(state, job_id, "upstream_task_id_missing", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    };
    let path = format!("/v1/videos/{upstream_id}");
    let response = match state.bridge.lock().unwrap().forward(
        "GET",
        &path,
        &[],
        &BTreeMap::new(),
        &job.request_id,
    ) {
        Ok(response) => response,
        Err(_) => {
            mark_video_reconcile(state, job_id, "reconcile_bridge_error", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
    };
    if !(200..300).contains(&response.status) {
        mark_video_reconcile(state, job_id, "reconcile_bridge_http_error", None);
        return Ok(VideoBillingDecision::ReconcileRequired);
    }
    let value = match serde_json::from_slice::<Value>(&response.body) {
        Ok(value) => value,
        Err(_) => {
            mark_video_reconcile(state, job_id, "reconcile_invalid_json", None);
            return Ok(VideoBillingDecision::ReconcileRequired);
        }
    };
    settle_video_job(state, job_id, &value)
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
