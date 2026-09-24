use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use aiwork_core::Principal;

use super::api_keys::{self, ApiKeysFile, KeyCheck, QuotaKind, ResolvedKey};
use super::core_bridge::{CoreBridge, CoreMode};
use super::usage::KeyId;
use super::ApiSharedState;

/// Shadow mode leaves legacy authorization in charge, but records whether the
/// presented credential also matched a Core key.  It is deliberately not a
/// Principal: shadow mode must not make Core identity authoritative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreAuthObservation {
    pub presented: bool,
    pub matched: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CoreAuthFailure {
    MissingCredentials,
    InvalidCredentials,
    Unavailable,
}

/// API Key 鉴权中间件：
/// - /health 跳过鉴权
/// - Core enforce 使用已启动的 CoreStore；off/shadow 保留 data/api_keys.json，
///   支持 Authorization: Bearer <key>（OpenAI 风格）或 x-api-key: <key>（Anthropic 风格）
/// - 存在启用 Key 时必须鉴权；无任何启用 Key 时：auth_disabled=true（显式关闭鉴权）放行记 anonymous，
///   否则拒绝（默认）并返回引导提示（仅 legacy off/shadow 路径）
/// - Key 每次命中即累加当日用量并写盘，超配额返回 429
/// - 每次请求重读 api_keys.json：新增/删除/禁用/开关立即生效
/// 校验通过后向 request extensions 插入命中的 Key 标识（KeyId），供 handler 用量记账；
/// Core enforce 另外插入权威 Principal。
pub async fn bearer_auth(
    State(state): State<Arc<ApiSharedState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if is_public_liveness_path(request.uri().path())
        || is_public_asset_content_path(request.uri().path())
    {
        return next.run(request).await;
    }
    // 浏览器 CORS 预检不携带 API Key；实际业务请求仍由后续鉴权严格保护。
    if request.method() == axum::http::Method::OPTIONS {
        return next.run(request).await;
    }

    // 提取呈现的 Key（Bearer 优先，其次 x-api-key）
    let authz = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    let bearer = authz
        .filter(|s| s.len() > 7 && s[..7].eq_ignore_ascii_case("Bearer "))
        .map(|s| s[7..].to_string());
    let xkey = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let presented = bearer.or(xkey);
    let bridge_required = is_bridge_required(&state.data_dir, request.uri().path());

    // Internal bridge routes (and all /v1 routes in bridge-only mode) are
    // authenticated by AI Work's dedicated bridge credential in every Core
    // mode. Core API Keys must not inherit access to the bridge control plane.
    if bridge_required {
        let capability = capability_for_request(&mut request).await;
        let data_dir = state.data_dir.clone();
        let today = super::usage::key_quota_day();
        let check = tokio::task::spawn_blocking(move || {
            let Some(presented) = presented.as_deref() else {
                return None;
            };
            let check = api_keys::verify_and_consume_locked_for_capability(
                &data_dir,
                presented,
                &today,
                capability,
            );
            Some(match check {
                KeyCheck::Ok(resolved)
                    if api_keys::is_active_bridge_key_id(&data_dir, &resolved.id) =>
                {
                    KeyCheck::Ok(resolved)
                }
                KeyCheck::Ok(_) | KeyCheck::Invalid => KeyCheck::Invalid,
                other => other,
            })
        })
        .await
        .unwrap_or(Some(KeyCheck::Invalid));

        return match check {
            Some(KeyCheck::Ok(resolved)) => {
                let attribution = match core_attribution_headers(&request) {
                    Ok(attribution) => attribution,
                    Err(response) => return response,
                };
                match persist_core_attribution(&state, attribution).await {
                    Ok(Some(attribution)) => request.extensions_mut().insert(attribution),
                    Ok(None) => None,
                    Err(response) => return response,
                };
                let id = resolved.id.clone();
                request.extensions_mut().insert(resolved);
                request.extensions_mut().insert(KeyId(id));
                next.run(request).await
            }
            Some(KeyCheck::QuotaExceeded { limit, kind }) => quota_exceeded(limit, kind),
            Some(KeyCheck::CapabilityNotAllowed { capability }) => {
                capability_not_allowed(&capability)
            }
            Some(KeyCheck::Invalid) | None => bridge_only_rejected(),
        };
    }

    match state
        .core
        .as_ref()
        .map(|bridge| bridge.mode)
        .unwrap_or(CoreMode::Off)
    {
        CoreMode::Enforce => {
            return match authenticate_enforce_request(
                &mut request,
                state.core.as_ref(),
                presented.as_deref(),
            ) {
                Ok(()) => next.run(request).await,
                Err(_) => core_auth_rejected(),
            };
        }
        CoreMode::Shadow => {
            observe_shadow_auth(&mut request, state.core.as_ref(), presented.as_deref());
        }
        CoreMode::Off => {}
    }

    // 鉴权热路径（P1 修复5b）：load + verify_and_consume_locked（内含记账写盘）
    // 全为同步磁盘 IO，整体移入 spawn_blocking（参数转 owned），避免阻塞
    // async 调度线程；锁与原子语义不变（verify_and_consume_locked 进程级锁内完成）
    let (auth_required, check) = {
        let data_dir = state.data_dir.clone();
        let capability = capability_for_request(&mut request).await;
        tokio::task::spawn_blocking(move || {
            let keys: ApiKeysFile = api_keys::load(&data_dir);
            // 存在启用 Key 时必须鉴权；无启用 Key 时由显式开关决定放行或拒绝
            let auth_required = keys.has_enabled() || !keys.auth_disabled;
            let check = presented.map(|p| {
                api_keys::verify_and_consume_locked_for_capability(
                    &data_dir,
                    &p,
                    &super::usage::key_quota_day(),
                    capability,
                )
            });
            (auth_required, check)
        })
        .await
        // join 失败（panic 等）按最严格处理：要求鉴权 + 视为无效 Key → 401
        .unwrap_or((true, Some(KeyCheck::Invalid)))
    };

    match check {
        Some(KeyCheck::Ok(rk)) => {
            let active_bridge = api_keys::is_active_bridge_key_id(&data_dir_for_auth(&state), &rk.id);
            if bridge_required && !active_bridge {
                return bridge_only_rejected();
            }
            if active_bridge {
                let attribution = match core_attribution_headers(&request) {
                    Ok(attribution) => attribution,
                    Err(response) => return response,
                };
                match persist_core_attribution(&state, attribution).await {
                    Ok(Some(attribution)) => request.extensions_mut().insert(attribution),
                    Ok(None) => None,
                    Err(response) => return response,
                };
            }
            let id = rk.id.clone();
            request.extensions_mut().insert(rk);
            request.extensions_mut().insert(KeyId(id));
            return next.run(request).await;
        }
        Some(KeyCheck::QuotaExceeded { limit, kind }) => {
            return quota_exceeded(limit, kind);
        }
        Some(KeyCheck::CapabilityNotAllowed { capability }) => {
            return capability_not_allowed(&capability);
        }
        Some(KeyCheck::Invalid) => {}
        None => {
            // 未携带 Key
            if !auth_required {
                request.extensions_mut().insert(KeyId("anonymous".into()));
                return next.run(request).await;
            }
            return auth_required_rejected().into_response();
        }
    }
    // 无启用 Key 且显式关闭鉴权：放行携带未知 Key 的请求并记为 anonymous
    if !auth_required {
        request.extensions_mut().insert(KeyId("anonymous".into()));
        return next.run(request).await;
    }

    (StatusCode::UNAUTHORIZED, "invalid api key").into_response()
}

/// Core attribution headers are trusted only after the dedicated AI Work
/// bridge credential has been authenticated. A regular client API Key cannot
/// forge a per-Core-Key association by supplying these headers itself.
fn core_attribution_headers(
    request: &Request,
) -> Result<Option<super::bridge_billing::CoreRequestAttribution>, Response> {
    if !matches!(request.uri().path(), "/v1/chat/completions" | "/v1/videos/generations") {
        return Ok(None);
    }
    let request_header_count = request.headers().get_all("x-core-request-id").iter().count();
    let key_header_count = request.headers().get_all("x-core-key-id").iter().count();
    let quote_header_count = request.headers().get_all("x-core-quote-id").iter().count();
    if request_header_count == 0 && key_header_count == 0 {
        return Ok(None);
    }
    if request_header_count != 1 || key_header_count != 1 || quote_header_count > 1 {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":{"code":"invalid_core_attribution","message":"Core request attribution headers must be supplied exactly once as a pair"}})),
        ).into_response());
    }
    let request_id = request.headers().get("x-core-request-id")
        .and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
    let key_id = request.headers().get("x-core-key-id")
        .and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
    let quote_id = request.headers().get("x-core-quote-id")
        .and_then(|value| value.to_str().ok()).unwrap_or_default();
    let one_shot_test = quote_id.starts_with("authorized-one-shot-test-");
    if !super::bridge_billing::valid_request_id(&request_id)
        || !super::bridge_billing::valid_core_key_id(&key_id)
        || (one_shot_test && quote_id != format!("authorized-one-shot-test-{request_id}"))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            axum::Json(json!({"error":{"code":"invalid_core_attribution","message":"Core request attribution identifiers are invalid"}})),
        ).into_response());
    }
    Ok(Some(super::bridge_billing::CoreRequestAttribution {
        request_id,
        core_key_id: key_id,
        one_shot_test,
    }))
}

async fn persist_core_attribution(
    state: &Arc<ApiSharedState>,
    attribution: Option<super::bridge_billing::CoreRequestAttribution>,
) -> Result<Option<super::bridge_billing::CoreRequestAttribution>, Response> {
    let Some(attribution) = attribution else {
        return Ok(None);
    };
    let data_dir = state.data_dir.clone();
    let request_id = attribution.request_id.clone();
    let key_id = attribution.core_key_id.clone();
    let one_shot_test = attribution.one_shot_test;
    let result = tokio::task::spawn_blocking(move || {
        let mut store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        store.record_core_request_with_mode(&request_id, &key_id, one_shot_test)
    }).await;
    match result {
        Ok(Ok(super::bridge_billing::CoreRequestRecord::Created | super::bridge_billing::CoreRequestRecord::Duplicate)) => Ok(Some(attribution)),
        Ok(Ok(super::bridge_billing::CoreRequestRecord::Conflict)) => Err((
            StatusCode::CONFLICT,
            axum::Json(json!({"error":{"code":"core_request_key_conflict","message":"Core request ID is already associated with a different API Key"}})),
        ).into_response()),
        Ok(Err(_)) | Err(_) => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":{"code":"core_attribution_store_unavailable","message":"Core request attribution is temporarily unavailable"}})),
        ).into_response()),
    }
}

fn data_dir_for_auth(state: &ApiSharedState) -> std::path::PathBuf {
    state.data_dir.clone()
}

fn is_bridge_required(data_dir: &std::path::Path, path: &str) -> bool {
    path.starts_with("/internal/bridge/")
        || (api_keys::bridge_only(data_dir) && path.starts_with("/v1/"))
}

fn bridge_only_rejected() -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": {
                "message": "AI Work 当前仅接受星链维度分流系统的桥接 API Key",
                "type": "permission_error",
                "code": "bridge_key_required",
            }
        })),
    )
        .into_response()
}

/// 供测试与本地路由守卫复用的桥接决策；不返回任何密钥内容。
pub(crate) fn authenticate_bridge_only(resolved: Option<&ResolvedKey>, is_active_bridge: bool) -> Result<(), Response> {
    if resolved.is_some() && is_active_bridge {
        Ok(())
    } else {
        Err(bridge_only_rejected())
    }
}

fn capability_for_path(path: &str) -> Option<&'static str> {
    match path {
        "/v1/chat/completions"
        | "/v1/completions"
        | "/v1/messages"
        | "/v1/responses" => Some(api_keys::CAPABILITY_CHAT),
        "/v1/images/generations" | "/v1/images/edits" => Some(api_keys::CAPABILITY_CHAT),
        "/v1/assets" => Some(api_keys::CAPABILITY_ASSETS),
        "/v1/videos/generations" => Some(api_keys::CAPABILITY_VIDEO),
        path if path.starts_with("/v1/videos/") => Some(api_keys::CAPABILITY_VIDEO),
        _ => None,
    }
}

const AUTH_BODY_PEEK_MAX_BYTES: usize = 8 * 1024 * 1024;

async fn capability_for_request(request: &mut Request) -> Option<&'static str> {
    let path = request.uri().path().to_owned();
    if path != "/v1/chat/completions" {
        return capability_for_path(&path);
    }

    let body = std::mem::take(request.body_mut());
    match axum::body::to_bytes(body, AUTH_BODY_PEEK_MAX_BYTES).await {
        Ok(bytes) => {
            let capability = capability_for_request_body(&path, &bytes);
            *request.body_mut() = Body::from(bytes);
            capability
        }
        Err(_) => capability_for_path(&path),
    }
}

fn capability_for_request_body(path: &str, body: &[u8]) -> Option<&'static str> {
    if path == "/v1/chat/completions"
        && serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|value| value.get("model").and_then(|model| model.as_str()).map(super::seedance_chat::is_seedance_model))
            .unwrap_or(false)
    {
        return Some(api_keys::CAPABILITY_VIDEO);
    }
    capability_for_path(path)
}

fn capability_not_allowed(capability: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        axum::Json(json!({
            "error": {
                "message": format!("API Key 未启用能力: {capability}"),
                "type": "permission_error",
                "code": "capability_not_allowed",
            }
        })),
    )
        .into_response()
}

/// Enforce-mode authentication is intentionally isolated from the legacy JSON
/// key store.  A successful Core lookup supplies both the authoritative
/// Principal and the existing KeyId extension used by legacy handlers.
fn authenticate_enforce_request(
    request: &mut Request,
    core: Option<&Arc<CoreBridge>>,
    presented: Option<&str>,
) -> Result<(), CoreAuthFailure> {
    let presented = presented.ok_or(CoreAuthFailure::MissingCredentials)?;
    let bridge = core
        .filter(|bridge| bridge.mode == CoreMode::Enforce)
        .ok_or(CoreAuthFailure::Unavailable)?;
    let principal = bridge
        .store
        .authenticate_api_key(presented)
        .map_err(|_| CoreAuthFailure::InvalidCredentials)?;
    let key_id = principal.key_id.clone();
    request.extensions_mut().insert(principal);
    request.extensions_mut().insert(KeyId(key_id));
    Ok(())
}

/// Shadow mode is observational only.  It never inserts a Principal and does
/// not affect the legacy JSON-key decision made below.
fn observe_shadow_auth(
    request: &mut Request,
    core: Option<&Arc<CoreBridge>>,
    presented: Option<&str>,
) {
    let matched = core
        .filter(|bridge| bridge.mode == CoreMode::Shadow)
        .and_then(|bridge| presented.and_then(|key| bridge.store.authenticate_api_key(key).ok()))
        .is_some();
    request.extensions_mut().insert(CoreAuthObservation {
        presented: presented.is_some(),
        matched,
    });
}

/// Scope checks are based only on the middleware-installed Core Principal.
/// The requested scope is intentionally not echoed to avoid disclosing policy
/// details to unauthenticated or partially authenticated callers.
pub fn require_request_scope(request: &Request, scope: &str) -> Result<(), Response> {
    let principal = request
        .extensions()
        .get::<Principal>()
        .ok_or_else(core_auth_rejected)?;
    aiwork_core::require_scope(principal, scope).map_err(|_| insufficient_scope())
}

/// 不带凭证的探活端点。只允许严格的根路径，避免把 `/status` 等诊断接口
/// 意外变成公网匿名接口。
fn is_public_liveness_path(path: &str) -> bool {
    matches!(path, "/health" | "/healthz")
}

/// 素材公网内容链接使用随机 token 鉴权，不再叠加 API Key（Trae 上游不会携带
/// 本地 Key）。只放行严格的 `/v1/assets/<opaque-id>/content` 形状，上传、列表及
/// 其它资产路径仍走常规 API Key 鉴权。
fn is_public_asset_content_path(path: &str) -> bool {
    let mut parts = path.split('/');
    let shape = matches!(
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(""), Some("v1"), Some("assets"), Some(id), Some("content"))
            if !id.is_empty() && id.len() <= 160 && id.bytes().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    );
    shape && parts.next().is_none() && !path.ends_with('/')
}

/// 401：要求鉴权但不满足（未配置启用 Key 且未显式关闭鉴权，或未携带 Key）。
/// JSON 错误体给出下一步指引（OpenAI/Anthropic 客户端均可解析 message）。
fn auth_required_rejected() -> axum::response::Response {
    let body = json!({
        "error": {
            "message": "API 服务已启用鉴权：请在应用的「API 服务」页创建并启用 API Key，\
                        或在该页显式关闭鉴权（不推荐，任何本机程序均可调用）",
            "type": "invalid_request_error",
            "code": "api_key_required",
        }
    });
    (
        StatusCode::UNAUTHORIZED,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

/// 429 配额超限响应（JSON 错误体，OpenAI/Anthropic 客户端均可解析 message）。
///
/// `quota_exceeded` 是统一的 canonical code；`legacy_code` 保留旧客户端按
/// `daily_quota_exceeded` 判断错误的兼容入口。
pub(super) fn quota_exceeded(limit: u64, kind: QuotaKind) -> Response {
    let (unit, field) = match kind {
        QuotaKind::Requests => ("次/日", "daily_requests"),
        QuotaKind::Tokens => ("Token/日", "daily_tokens"),
    };
    let body = json!({
        "error": {
            "message": format!("API Key 已达今日配额上限（{limit} {unit}），请明天再试或调整限额"),
            "type": "quota_exceeded",
            "code": "quota_exceeded",
            "legacy_code": "daily_quota_exceeded",
            "param": field,
        }
    });
    (
        StatusCode::TOO_MANY_REQUESTS,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body.to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, sync::Arc};

    use aiwork_core::{CoreStore, NewUser, UserRole};
    use axum::{body::Body, http::Request, routing::get, Router};
    use tower::ServiceExt;

    use super::{
        authenticate_enforce_request, capability_for_request_body, is_public_asset_content_path,
        is_public_liveness_path, require_request_scope,
    };
    use crate::api_server::{CoreBridge, CoreMode};

    fn core_fixture_with_mode(mode: CoreMode) -> (Arc<CoreBridge>, String) {
        let dir = std::env::temp_dir().join(format!(
            "twa-auth-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "core-user".into(),
                    name: "Core user".into(),
                    role: UserRole::User,
                },
                "bootstrap",
            )
            .unwrap();
        let issued = store
            .issue_api_key(
                "core-user",
                "test",
                BTreeSet::from(["models:read".to_owned(), "chat:invoke".to_owned()]),
                "bootstrap",
            )
            .unwrap();
        let bridge = Arc::new(CoreBridge::new(store, mode));
        // The CoreStore owns the temporary directory through the test process; clean up is
        // intentionally best-effort because Windows may still hold SQLite handles briefly.
        let _ = fs::remove_dir_all(&dir);
        (bridge, issued.plaintext)
    }

    fn core_fixture() -> (Arc<CoreBridge>, String) {
        core_fixture_with_mode(CoreMode::Enforce)
    }

    fn bridge_auth_fixture() -> (Router, String, String, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "twa-bridge-auth-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "regular-user".into(),
                    name: "Regular user".into(),
                    role: UserRole::User,
                },
                "bootstrap",
            )
            .unwrap();
        let regular_key = store
            .issue_api_key(
                "regular-user",
                "regular",
                BTreeSet::from(["models:read".to_owned()]),
                "bootstrap",
            )
            .unwrap()
            .plaintext;
        let bridge_key = super::api_keys::issue_bridge_key(&dir, "router bridge")
            .unwrap()
            .plaintext;
        let state = Arc::new(crate::api_server::ApiSharedState {
            core: Some(Arc::new(CoreBridge::new(store, CoreMode::Enforce))),
            pool: crate::api_server::pool::ApiPool::new(),
            wb_pool: crate::api_server::pool::ApiPool::new(),
            wb_enabled: std::sync::atomic::AtomicBool::new(false),
            wb_sanitize: std::sync::atomic::AtomicBool::new(true),
            wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
            wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
            wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_sticky: crate::api_server::wb_sticky::StickyStore::default(),
            pool_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
            model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
            default_model: "test-model".into(),
            data_dir: dir.clone(),
            video_payloads: crate::api_server::video_payload::VideoPayloadStore::new(&dir),
            cors_origins: String::new(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limiter: crate::api_server::limits::RateLimiter::with_config(
                crate::api_server::limits::LimitConfig {
                    max_inflight: 4,
                    max_video_jobs: 2,
                    asset_uploads_per_minute: 10,
                    asset_bytes_per_hour: 1024,
                    video_submissions_per_minute: 10,
                },
            ),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            logger: crate::api_server::ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: std::sync::Mutex::new(crate::api_server::usage::UsageFile::default()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        });
        let app = Router::new()
            .route("/internal/bridge/test", get(|| async { axum::http::StatusCode::OK }))
            .route("/v1/chat/completions", get(|attribution: Option<axum::Extension<crate::api_server::bridge_billing::CoreRequestAttribution>>| async move {
                let body = attribution.map(|axum::Extension(value)| serde_json::json!({
                    "request_id": value.request_id,
                    "core_key_id": value.core_key_id,
                })).unwrap_or(serde_json::Value::Null);
                axum::Json(body)
            }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), super::bearer_auth))
            .with_state(state);
        (app, regular_key, bridge_key, dir)
    }

    #[test]
    fn public_liveness_paths_are_strictly_scoped() {
        assert!(is_public_liveness_path("/health"));
        assert!(is_public_liveness_path("/healthz"));
        assert!(!is_public_liveness_path("/status"));
        assert!(!is_public_liveness_path("/health/extra"));
    }

    #[test]
    fn public_asset_path_is_strictly_scoped() {
        assert!(is_public_asset_content_path("/v1/assets/asset-1/content"));
        assert!(!is_public_asset_content_path("/v1/assets/asset-1/content/"));
        assert!(!is_public_asset_content_path("/v1/assets/asset-1"));
        assert!(!is_public_asset_content_path("/v1/assets/../content"));
        assert!(!is_public_asset_content_path("/v1/assets/asset-1/content/extra"));
    }

    #[test]
    fn legacy_chat_capability_switches_to_video_for_seedance_model() {
        assert_eq!(
            capability_for_request_body(
                "/v1/chat/completions",
                br#"{"model":"seedance","messages":[]}"#,
            ),
            Some(super::api_keys::CAPABILITY_VIDEO)
        );
        assert_eq!(
            capability_for_request_body(
                "/v1/chat/completions",
                br#"{"model":"gpt-4o","messages":[]}"#,
            ),
            Some(super::api_keys::CAPABILITY_CHAT)
        );
    }

    #[test]
    fn enforce_auth_inserts_core_principal_and_key_id_not_request_user_id() {
        let (bridge, plaintext) = core_fixture();
        let mut request = Request::builder()
            .uri("/v1/models?user_id=attacker")
            .header("x-user-id", "attacker")
            .body(Body::from(r#"{"user_id":"attacker"}"#))
            .unwrap();

        authenticate_enforce_request(&mut request, Some(&bridge), Some(&plaintext)).unwrap();

        let principal = request
            .extensions()
            .get::<aiwork_core::Principal>()
            .unwrap();
        assert_eq!(principal.user_id, "core-user");
        assert_eq!(
            request.extensions().get::<super::KeyId>().unwrap().0,
            principal.key_id
        );
    }

    #[test]
    fn enforce_auth_without_core_key_is_rejected() {
        let (bridge, _) = core_fixture();
        let mut request = Request::new(Body::empty());

        assert!(matches!(
            authenticate_enforce_request(&mut request, Some(&bridge), None),
            Err(super::CoreAuthFailure::MissingCredentials)
        ));
    }

    #[test]
    fn shadow_auth_only_records_observation_without_principal() {
        let (bridge, plaintext) = core_fixture_with_mode(CoreMode::Shadow);
        let mut request = Request::new(Body::empty());

        super::observe_shadow_auth(&mut request, Some(&bridge), Some(&plaintext));

        assert_eq!(
            request
                .extensions()
                .get::<super::CoreAuthObservation>(),
            Some(&super::CoreAuthObservation {
                presented: true,
                matched: true,
            })
        );
        assert!(request
            .extensions()
            .get::<aiwork_core::Principal>()
            .is_none());
    }

    #[test]
    fn core_auth_rejection_is_unauthorized_and_minimal() {
        assert_eq!(super::core_auth_rejected().status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn require_request_scope_reads_only_the_core_principal() {
        let principal = aiwork_core::Principal {
            user_id: "core-user".into(),
            key_id: "core-key".into(),
            scopes: BTreeSet::from(["models:read".to_owned()]),
        };
        let mut request = Request::builder()
            .uri("/v1/chat/completions?user_id=attacker")
            .header("x-user-id", "attacker")
            .body(Body::from(r#"{"user_id":"attacker"}"#))
            .unwrap();
        request.extensions_mut().insert(principal);

        assert!(require_request_scope(&request, "models:read").is_ok());
        let response = require_request_scope(&request, "chat:invoke").unwrap_err();
        assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn quota_error_keeps_compatible_fields_and_identifies_token_limit() {
        let response = super::quota_exceeded(10, super::api_keys::QuotaKind::Tokens);
        assert_eq!(response.status(), axum::http::StatusCode::TOO_MANY_REQUESTS);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["type"], "quota_exceeded");
        assert_eq!(payload["error"]["code"], "quota_exceeded");
        assert_eq!(payload["error"]["legacy_code"], "daily_quota_exceeded");
        assert_eq!(payload["error"]["param"], "daily_tokens");
        assert!(payload["error"]["message"].as_str().unwrap().contains("Token"));
    }

    #[tokio::test]
    async fn internal_bridge_route_rejects_regular_core_keys_and_accepts_active_bridge_key() {
        let (app, regular_key, bridge_key, dir) = bridge_auth_fixture();
        let request = |key: &str| {
            Request::builder()
                .uri("/internal/bridge/test")
                .header("authorization", format!("Bearer {key}"))
                .body(Body::empty())
                .unwrap()
        };

        let regular_status = app
            .clone()
            .oneshot(request(&regular_key))
            .await
            .unwrap()
            .status();
        let bridge_status = app
            .oneshot(request(&bridge_key))
            .await
            .unwrap()
            .status();

        assert_eq!(regular_status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(bridge_status, axum::http::StatusCode::OK);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn authenticated_bridge_attribution_is_injected_only_after_persistence() {
        let (app, _regular_key, bridge_key, dir) = bridge_auth_fixture();
        let request = Request::builder()
            .uri("/v1/chat/completions")
            .header("authorization", format!("Bearer {bridge_key}"))
            .header("x-core-request-id", "core-request-17")
            .header("x-core-key-id", "key_server_17")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["request_id"], "core-request-17");
        assert_eq!(payload["core_key_id"], "key_server_17");

        let db_path = dir.join("bridge-billing.sqlite3");
        let connection = rusqlite::Connection::open(db_path).unwrap();
        let persisted: (String, String) = connection.query_row(
            "SELECT request_id, core_key_id FROM bridge_core_requests WHERE request_id = ?1",
            ["core-request-17"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(persisted, ("core-request-17".into(), "key_server_17".into()));
        let _ = fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod bridge_only_tests {
    use axum::http::StatusCode;

    use super::authenticate_bridge_only;

    #[test]
    fn bridge_only_rejects_a_legacy_user_key_before_dispatch() {
        let response = authenticate_bridge_only(None, false).unwrap_err();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}

fn core_auth_rejected() -> Response {
    (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
}

fn insufficient_scope() -> Response {
    (StatusCode::FORBIDDEN, "insufficient scope").into_response()
}
