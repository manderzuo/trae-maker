use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use aiwork_core::{require_scope, ChatExecutionRequest, ChatExecutionResult, ChatExecutor, CoreError, Principal, RequestState, UpstreamError};

use super::custom_route;
use super::assets;
use super::core_bridge::ChatOutcome;
use super::dispatch::{self, DispatchError, TargetPool};
use super::retry::{retry_plan, RetryAction};
use super::sse;
use super::unified_catalog;
use super::usage::{extract_tokens, KeyId};
use super::wb_catalog;
use super::wb_model_route;
use super::wb_route;
use super::video;
use super::pool::ResourceKind;
use super::limits::{LimitError, LimitKind};
use super::{classify_error, classify_solo_error, streaming_agent, ApiSharedState, ErrKind,
            InflightGuard,
            AGENT_HOST, APP_ID, EP_LLM_CHAT, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

const MAX_BODY_BYTES: usize = 8 << 20;

/// 客户端协议：决定响应/流事件的输出格式（请求侧均已统一转为 OpenAI 内部格式）
#[derive(Clone, Copy, PartialEq)]
pub enum Protocol {
    OpenAi,
    /// OpenAI legacy text completions（/v1/completions）
    OpenAiText,
    Anthropic,
    /// Codex Responses API（/v1/responses，T4.1/F-40）
    Responses,
}

impl Protocol {
    pub(super) fn log_path(self) -> &'static str {
        match self {
            Protocol::OpenAi => "/v1/chat/completions",
            Protocol::OpenAiText => "/v1/completions",
            Protocol::Anthropic => "/v1/messages",
            Protocol::Responses => "/v1/responses",
        }
    }
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// keep-alive 退出信号（P1 修复）：主任务（spawn_blocking）结束时 Drop 触发
/// watch 通知，ticker 收到后退出 → sender 全部关闭 → 流可正常终结。
/// Drop 兜底覆盖 panic 展开与提前 return 路径
struct DoneSignal(tokio::sync::watch::Sender<bool>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(true);
    }
}

/// 聚合路径失败（P1 修复1）：区分「无健康账号」（维持原 503/格式）与
/// 「Fatal 上游错误透传」（携带上游状态码与错误体摘要）
enum AggregateFail {
    NoHealthy { message: String },
    Incomplete { message: String },
    Upstream(u16, String),
}

struct CoreChatContext {
    bridge: Arc<super::CoreBridge>,
    principal: Principal,
    request_id: String,
    reservation_id: String,
    reservation_amount: i64,
}

fn core_enforcing(state: &ApiSharedState) -> bool {
    state
        .core
        .as_ref()
        .map(|bridge| bridge.mode == super::CoreMode::Enforce)
        .unwrap_or(false)
}

fn core_scope_error(scope: &str) -> Response {
    openai_error(
        StatusCode::FORBIDDEN,
        "insufficient_scope",
        &format!("missing required scope: {scope}"),
    )
}

fn core_principal_or_unauthorized(principal: Option<&Extension<Principal>>) -> Result<Principal, Response> {
    principal
        .map(|extension| extension.0.clone())
        .ok_or_else(|| openai_error(StatusCode::UNAUTHORIZED, "unauthorized", "Core Principal required"))
}

fn core_error_response(error: CoreError) -> Response {
    match error {
        CoreError::MissingScope { scope } => core_scope_error(&scope),
        CoreError::BudgetPolicyMissing { .. } => openai_error(
            StatusCode::BAD_REQUEST,
            "budget_policy_missing",
            "no Core cost policy matches this request",
        ),
        CoreError::QuotaInsufficient { .. } => openai_error(
            StatusCode::TOO_MANY_REQUESTS,
            "insufficient_quota",
            "Core quota is insufficient",
        ),
        CoreError::IdempotencyConflict => openai_error(
            StatusCode::CONFLICT,
            "idempotency_conflict",
            "Idempotency-Key was already used for a different request",
        ),
        CoreError::InvalidRequestIdentity { .. } => {
            openai_error(StatusCode::UNAUTHORIZED, "unauthorized", "invalid Core request identity")
        }
        CoreError::CoreModeNotEnforcing { .. } => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "core_mode_error",
            "Core enforce mode is unavailable",
        ),
        other => openai_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", &other.to_string()),
    }
}

fn with_core_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn core_replay_response(preflight: &super::core_bridge::PreflightResult) -> Response {
    let successful_result = preflight.result.as_ref().filter(|result| {
        matches!(result.status, Some(status) if (200..300).contains(&status))
            && result.error_code.is_none()
    });
    let (status, code, message, body) = if successful_result.is_some() {
        (
            StatusCode::OK,
            "idempotent_replay",
            "request already completed; response replay is safe",
            json!({
                "id": preflight.request_id,
                "object": "chat.completion",
                "choices": [],
                "idempotent_replay": true,
            }),
        )
    } else if preflight.result.is_none()
        && !matches!(
            preflight.state,
            RequestState::Settled | RequestState::Succeeded | RequestState::Failed | RequestState::Unknown
        ) {
        (
            StatusCode::CONFLICT,
            "request_in_progress",
            "request is already in progress; upstream was not called again",
            json!({"request_id": preflight.request_id}),
        )
    } else if matches!(
        preflight.result.as_ref().and_then(|result| result.error_code.as_deref()),
        Some("upstream_uncertain")
    ) || preflight.result.is_none() {
        (
            StatusCode::CONFLICT,
            "idempotent_replay_unknown",
            "request outcome is unknown; upstream was not called again",
            json!({"request_id": preflight.request_id}),
        )
    } else {
        (
            StatusCode::CONFLICT,
            "idempotent_replay_failed",
            "request already failed; upstream was not called again",
            json!({"request_id": preflight.request_id}),
        )
    };
    let response = if status == StatusCode::OK {
        Response::builder()
            .status(status)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    } else {
        openai_error(status, code, message)
    };
    with_core_request_id(response, &preflight.request_id)
}

fn settle_core_outcome(
    context: &CoreChatContext,
    mut response: Response,
    outcome: ChatOutcome,
) -> Response {
    response.headers_mut().remove("x-aiwork-core-actual-amount");
    response.headers_mut().remove("x-aiwork-core-outcome");
    match context.bridge.settle_chat(&context.principal, &context.reservation_id, outcome) {
        Ok(()) => with_core_request_id(response, &context.request_id),
        Err(error) => core_error_response(error),
    }
}

fn settle_core_response(context: &CoreChatContext, response: Response) -> Response {
    let actual_amount = response
        .headers()
        .get("x-aiwork-core-actual-amount")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value >= 0 && *value <= context.reservation_amount);
    let uncertain = response
        .headers()
        .get("x-aiwork-core-outcome")
        .and_then(|value| value.to_str().ok())
        == Some("unknown");
    let outcome = if response.status().is_success() {
        ChatOutcome::Success(ChatExecutionResult {
            status: response.status().as_u16(),
            body: json!({}),
            actual_amount,
        })
    } else if uncertain || response.status().is_server_error() {
        ChatOutcome::Upstream(UpstreamError::Disconnected)
    } else {
        ChatOutcome::Failure(UpstreamError::Rejected {
            status: response.status().as_u16(),
            code: None,
        })
    };
    settle_core_outcome(context, response, outcome)
}

#[cfg(test)]
thread_local! {
    static CORE_TEST_EXECUTOR: std::cell::RefCell<Option<Arc<dyn ChatExecutor>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn install_core_test_executor(executor: Arc<dyn ChatExecutor>) {
    CORE_TEST_EXECUTOR.with(|slot| *slot.borrow_mut() = Some(executor));
}

#[cfg(test)]
fn clear_core_test_executor() {
    CORE_TEST_EXECUTOR.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn core_test_executor() -> Option<Arc<dyn ChatExecutor>> {
    CORE_TEST_EXECUTOR.with(|slot| slot.borrow().clone())
}

// ==================== Handlers ====================

/// WB 上游模型目录命中（T2.1 原始判定，保留供 /v1/models 与诊断复用）
#[allow(dead_code)]
fn wb_model_requested(state: &ApiSharedState, model: &str) -> bool {
    wb_catalog::find(&wb_catalog::load(&state.data_dir), model).is_some()
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap()
}

/// T5.2/F-61 四段模型路由解析（别名→规则→系列通配→后缀）+ T5.6③ 后台任务降级。
/// 返回 (最终模型, 路由级 effort 注入提示)；全未命中目录 → None（走 SOLO 上游）。
fn resolve_wb_target(
    state: &ApiSharedState,
    model: &str,
    body: &Value,
) -> Option<(String, Option<String>)> {
    let cfg = wb_model_route::load_config(&state.data_dir);
    let catalog = wb_catalog::load(&state.data_dir);
    let r = wb_model_route::resolve(&cfg, &catalog, model);
    if wb_catalog::find(&catalog, &r.model).is_none() {
        return None;
    }
    // T5.6③ 后台任务降级（显式开启才生效）：标题/摘要类短请求 → 目录最低倍率模型
    let final_model = if state.wb_bg_downgrade.load(std::sync::atomic::Ordering::Relaxed)
        && wb_model_route::is_background_task(body)
    {
        wb_model_route::cheapest_catalog_model(&catalog).unwrap_or(r.model)
    } else {
        r.model
    };
    Some((final_model, r.effort_hint))
}

/// T5.3/F-62 默认深度思考：客户端未显式请求 effort 且无路由级提示时默认 high。
/// `explicit_effort` 由调用方按协议判定（OpenAI: reasoning_effort 字段；Anthropic: thinking 参数）
fn effective_effort_hint(
    state: &ApiSharedState,
    route_hint: Option<String>,
    explicit_effort: bool,
) -> Option<String> {
    if route_hint.is_some() {
        return route_hint;
    }
    if !explicit_effort
        && state
            .wb_default_thinking
            .load(std::sync::atomic::Ordering::Relaxed)
    {
        return Some("high".to_string());
    }
    None
}

/// 注入 effort 提示到请求体字节流（T5.2④/T5.3）
fn apply_effort_hint(body_vec: Vec<u8>, hint: Option<String>) -> Vec<u8> {
    wb_model_route::inject_effort_hint(&body_vec, &hint)
}

/// 模型级冷却快速失败（T2.7/F-34：优先级高于 Key 级）
fn model_cooling_response(state: &ApiSharedState, model: &str, proto: Protocol) -> Response {
    let rem = wb_route::model_cooling_remaining(state, model).unwrap_or(0);
    let msg = format!("model {} cooling down, retry after {}s", model, rem);
    match proto {
        Protocol::Anthropic => anthropic_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg),
        _ => openai_error(StatusCode::TOO_MANY_REQUESTS, "model_cooldown", &msg),
    }
}

/// 调度错误矩阵 → 按客户端协议格式化响应（§4.3/§4.5；统一调度分流点专用）
fn dispatch_error_response(err: DispatchError, proto: Protocol, model: &str) -> Response {
    match err {
        DispatchError::WbDisabled => {
            let msg = "该模型属 WorkBuddy 上游，但 WB 上游未启用（api_pool.json wb_enabled）";
            match proto {
                Protocol::Anthropic => {
                    anthropic_error(StatusCode::BAD_REQUEST, "invalid_request_error", msg)
                }
                _ => openai_error(StatusCode::BAD_REQUEST, "wb_upstream_disabled", msg),
            }
        }
        DispatchError::ModelCooling(rem) => {
            let msg = format!("model {} cooling down, retry after {}s", model, rem);
            match proto {
                Protocol::Anthropic => {
                    anthropic_error(StatusCode::TOO_MANY_REQUESTS, "rate_limit_error", &msg)
                }
                _ => openai_error(StatusCode::TOO_MANY_REQUESTS, "model_cooldown", &msg),
            }
        }
        DispatchError::NoHealthy(_) => {
            let msg = "no healthy account available";
            match proto {
                Protocol::Anthropic => {
                    anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", msg)
                }
                _ => openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", msg),
            }
        }
    }
}

pub(crate) fn public_health_payload() -> Value {
    json!({
        "status": "ok",
        "running": true,
    })
}

pub async fn health(State(_state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    Json(public_health_payload())
}

/// /healthz（T2.3/F-32）：无健康账号（两个池都没有）→ 503，供探活/看门狗
fn healthz_payload(solo_ok: bool, wb_enabled: bool, wb_ok: bool) -> (StatusCode, Value) {
    if solo_ok || (wb_enabled && wb_ok) {
        (StatusCode::OK, json!({ "status": "ok", "running": true }))
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "status": "unavailable", "running": true, "reason": "no healthy account" }),
        )
    }
}

pub async fn healthz(State(state): State<Arc<ApiSharedState>>) -> Response {
    let pool = state.pool.status_list();
    let wb_pool = state.wb_pool.status_list();
    let solo_ok = pool.iter().any(|p| !p.disabled && !p.cooling);
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_ok = wb_pool.iter().any(|p| !p.disabled && !p.cooling);
    let (status, payload) = healthz_payload(solo_ok, wb_enabled, wb_ok);
    (status, axum::Json(payload)).into_response()
}

pub async fn status(State(state): State<Arc<ApiSharedState>>) -> impl IntoResponse {
    let pool = state.pool.status_list();
    let now: i64 = now_ts() as i64;
    let total = state.total_requests.load(std::sync::atomic::Ordering::Relaxed);
    let active = safe_lock(&state.active_uid).clone();
    let last_err = safe_lock(&state.last_error).clone();

    // 汇总统计
    let total_accounts = pool.len();
    let available = pool.iter().filter(|p| !p.disabled && !p.cooling).count();
    let cooling = pool.iter().filter(|p| p.cooling).count();
    let disabled = pool.iter().filter(|p| p.disabled).count();
    let total_general_credits: f64 = pool.iter().filter_map(|p| p.general_credits.or(p.credits)).sum();
    let total_work_credits: f64 = pool.iter().filter_map(|p| p.work_credits).sum();
    let total_credits = ((total_general_credits + total_work_credits) * 100.0).round() / 100.0;
    let total_general_credits = (total_general_credits * 100.0).round() / 100.0;
    let total_work_credits = (total_work_credits * 100.0).round() / 100.0;

    // 账号明细
    let accounts: Vec<Value> = pool.iter().map(|p| {
        let status = if p.disabled {
            "disabled"
        } else if p.cooling {
            "cooling"
        } else if p.credits_expire_at.map_or(false, |exp| exp < now) {
            "expired"
        } else if p.credits.map_or(false, |c| c <= 0.0) {
            "no_credits"
        } else {
            "available"
        };
        json!({
            "uid": p.uid,
            "name": p.name,
            "status": status,
            "credits": p.credits,
            "general_credits": p.general_credits,
            "work_credits": p.work_credits,
            "total_credits": p.total_credits,
            "credits_expire_at": p.credits_expire_at,
            "cooling": p.cooling,
            "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason,
            "disabled": p.disabled,
            "err_count": p.err_count,
            "state": p.state,
        })
    }).collect();

    // WB 池画像（T2.3/F-32）
    let wb_pool = state.wb_pool.status_list();
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    let wb_accounts: Vec<Value> = wb_pool.iter().map(|p| {
        json!({
            "uid": p.uid, "name": p.name, "credits": p.credits,
            "cooling": p.cooling, "cooldown_until": p.cooldown_until,
            "cooldown_reason": p.cooldown_reason, "disabled": p.disabled,
            "general_credits": p.general_credits, "work_credits": p.work_credits,
            "total_credits": p.total_credits,
            "err_count": p.err_count, "state": p.state,
        })
    }).collect();
    let model_cooldowns: Vec<Value> = {
        let map = safe_lock(&state.model_cooldowns);
        map.iter()
            .filter(|(_, (until, _))| *until > now)
            .map(|(m, (until, fails))| json!({
                "model": m, "until": until, "remaining_s": until - now, "fails": fails,
            }))
            .collect()
    };

    Json(json!({
        "running": true,
        "total_requests": total,
        // 当前并发数（统一网关 §4.5）：InflightGuard RAII 维护，覆盖 6 业务端点
        "inflight": state.inflight.load(std::sync::atomic::Ordering::Relaxed),
        "active_uid": active,
        "last_error": last_err,
        "summary": {
            "total_accounts": total_accounts,
            "available": available,
            "cooling": cooling,
            "disabled": disabled,
            "total_credits": total_credits,
            "general_credits": total_general_credits,
            "work_credits": total_work_credits,
        },
        "accounts": accounts,
        "wb": {
            "enabled": wb_enabled,
            "total_accounts": wb_pool.len(),
            "accounts": wb_accounts,
            "model_cooldowns": model_cooldowns,
            "sticky_sessions": state.wb_sticky.len(),
            // 上游健康探针（F-34 ④/§2.2）：-1 未探测 / 0 不可达 / 1 在线
            "probe_ok": state.wb_probe_ok.load(std::sync::atomic::Ordering::Relaxed),
            "probe_ts_ms": state.wb_probe_ts_ms.load(std::sync::atomic::Ordering::Relaxed),
        },
    }))
}

pub async fn models(
    State(state): State<Arc<ApiSharedState>>,
    principal: Option<Extension<Principal>>,
) -> Response {
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        if require_scope(&principal, "models:read").is_err() {
            return core_scope_error("models:read");
        }
    }
    // 统一模型目录（§3.4）：实时聚合 data/api_models.json（Trae，元数据四层链）
    // 与 data/wb_model_catalog.json（Buddy），纯派生不落盘。官网/目录同步后
    // 无需重启 API 服务即可通过 /v1/models 看到最新列表
    let wb_enabled = state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed);
    // 可用性标记运行时派生（§3.3 #5）：HTTP 端点用实时池健康
    let trae_ok = state.pool.has_selectable();
    let buddy_ok = state.wb_pool.has_selectable();
    let data_dir = state.data_dir.clone();
    let list = tokio::task::spawn_blocking(move || {
        unified_catalog::unified_models(&data_dir, wb_enabled, trae_ok, buddy_ok)
    })
    .await
    .unwrap_or_default();
    // wb_enabled=false：仅 Buddy 源的模型过滤，双源模型保留（仍可由 Trae 源服务 §3.4）
    let data: Vec<Value> = list
        .iter()
        .filter(|m| wb_enabled || !m.sources.iter().all(|s| s.pool == "buddy"))
        .map(|m| {
            json!({
                "id": m.id,
                "object": "model",
                "created": 1753600000,
                "owned_by": "unified",
                "display": m.display,
                "rate": m.rate,
                "context_length": m.context_length,
                "max_tokens": m.max_tokens,
                "supports_image": m.supports_image,
                "supported_efforts": m.efforts,
                // 来源池集合：[{pool: "trae"|"buddy", rate, enabled}]（徽章/降级判定
                // 由客户端按元数据自决，勿硬编码 §3.4）
                "sources": m.sources,
                "manual": m.manual,
            })
        })
        .collect();
    Json(json!({ "object": "list", "data": data })).into_response()
}

pub async fn chat_completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    principal: Option<Extension<Principal>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    // T5.6② 单端口协议区分：anthropic-version 头出现 → 客户端实为 Anthropic
    // Messages 协议，按路径分流给出明确指引（避免三协议混投后字段级静默错乱）
    if headers.contains_key("anthropic-version") {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "wrong_endpoint",
            "检测到 anthropic-version 头：该请求应为 Anthropic Messages 协议，请改用 POST /v1/messages（本网关单端口三协议按路径区分）",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 校验 JSON：无效请求体直接 400，不转发上游（与 /v1/messages 行为对齐）
    let mut peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    // 阶段 C：显式会话才启用本地历史补齐；无 ID 的旧客户端保持无状态。
    let conversation_id = super::conversation::ensure_request_id(&headers, &mut peek);
    if let Some(id) = conversation_id.as_deref() {
        super::conversation::merge_history(&state.data_dir, id, &mut peek);
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    let mut core_context: Option<CoreChatContext> = None;
    #[cfg(test)]
    let mut core_execution: Option<ChatExecutionRequest> = None;
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        if require_scope(&principal, "chat:invoke").is_err() {
            return core_scope_error("chat:invoke");
        }
        if stream {
            return openai_error(
                StatusCode::NOT_IMPLEMENTED,
                "stream_not_enabled_in_phase1",
                "streaming chat is not enabled in Core phase 1",
            );
        }
        let idempotency_key = headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let idempotency_key = match idempotency_key {
            Some(key) => key,
            None => {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "idempotency_key_required",
                    "Idempotency-Key is required in Core enforce mode",
                )
            }
        };
        if peek.get("model").is_none() {
            peek["model"] = json!(model.clone());
        }
        let bridge = state.core.as_ref().expect("Core enforce mode requires a bridge");
        let preflight = match bridge.preflight_chat(
            &principal,
            &key_str,
            Some(idempotency_key),
            &peek,
        ) {
            Ok(preflight) => preflight,
            Err(error) => return core_error_response(error),
        };
        if preflight.execution.is_none() {
            return core_replay_response(&preflight);
        }
        let reservation = match preflight.reservation.as_ref() {
            Some(reservation) => reservation,
            None => return core_error_response(CoreError::ReservationNotFound {
                reservation_id: preflight.request_id,
            }),
        };
        #[cfg(test)]
        {
            core_execution = preflight.execution;
        }
        core_context = Some(CoreChatContext {
            bridge: state.core.as_ref().unwrap().clone(),
            principal,
            request_id: preflight.request_id,
            reservation_id: reservation.id.clone(),
            reservation_amount: reservation.amount,
        });
    }

    let body_vec = serde_json::to_vec(&peek).unwrap_or_else(|_| body.to_vec());

    #[cfg(test)]
    if let (Some(context), Some(execution), Some(executor)) =
        (core_context.as_ref(), core_execution.take(), core_test_executor())
    {
        let result = executor.execute(execution);
        let response = match result {
            Ok(result) => {
                let is_success = (200..300).contains(&result.status);
                let settlement_result = ChatExecutionResult {
                    status: result.status,
                    body: result.body.clone(),
                    actual_amount: result
                        .actual_amount
                        .filter(|actual_amount| *actual_amount >= 0 && *actual_amount <= context.reservation_amount),
                };
                let mut response = Response::builder()
                    .status(StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK))
                    .header("content-type", "application/json")
                    .body(Body::from(result.body.to_string()))
                    .unwrap();
                if let Some(actual_amount) = result
                    .actual_amount
                    .filter(|actual_amount| *actual_amount >= 0 && *actual_amount <= context.reservation_amount)
                {
                    if let Ok(value) = HeaderValue::from_str(&actual_amount.to_string()) {
                        response.headers_mut().insert("x-aiwork-core-actual-amount", value);
                    }
                }
                let outcome = if is_success {
                    ChatOutcome::Success(settlement_result)
                } else if result.status >= 500 {
                    ChatOutcome::Upstream(UpstreamError::Disconnected)
                } else {
                    ChatOutcome::Failure(UpstreamError::Rejected {
                        status: result.status,
                        code: None,
                    })
                };
                settle_core_outcome(context, response, outcome)
            }
            Err(error) => {
                let response = openai_error(StatusCode::BAD_GATEWAY, "upstream_error", &error.to_string());
                settle_core_outcome(context, response, ChatOutcome::Upstream(error))
            }
        };
        return response;
    }

    // 统一调度分流点（§4.1 ③~⑥）：resolve_target 决定资源池/会话池粘性/跨池回退/
    // 错误矩阵，替代原 resolve_wb_target 单向判定；默认策略下行为与改造前一致（§9.1）。
    // inflight guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = state.inflight_guard();
    let response = match dispatch::resolve_target(&state, &model, &peek) {
        Err(e) => dispatch_error_response(e, Protocol::OpenAi, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考：客户端未带 reasoning_effort 时注入 high
                let explicit = peek.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAi, key_str, guard)
                } else {
                    wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard).await
                }
            }
            TargetPool::Trae => {
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard, conversation_id.clone())
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard, conversation_id.clone()).await
                }
            }
            TargetPool::Custom => {
                // 自定义模型直达（custom_models 命中即 Custom，§dispatch ⓪）；
                // 条目可能热更新，执行时重读，缺失（已被删）按 404 语义报错
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::OpenAi, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::OpenAi, key_str, guard).await
                        }
                    }
                    None => openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
                }
            }
        },
    };
    match core_context.as_ref() {
        Some(context) => settle_core_response(context, response),
        None => response,
    }
}

/// Codex Responses API 端点（T4.1/F-40）：POST /v1/responses
///
/// 请求投影为 OpenAI 内部格式后复用 WB 上游既有管线（取号/重试/粘性/脱敏一份）。
/// 仅支持 WB 上游模型（Codex CLI `wire_api="responses"` 直配 base_url 的目标场景）；
/// 脱敏沿用全局 `wb_sanitize` 开关，审核命中按既有分级重试表退回重试。
pub async fn responses_api(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    // Responses → OpenAI chat 内部格式（纯投影，失败即 400）
    let mut chat_body: Value = match super::wb_responses::responses_to_chat(&peek) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &e);
        }
    };
    if let Some(obj) = chat_body.as_object_mut() {
        obj.insert("model".into(), json!(model.clone()));
        obj.insert("stream".into(), json!(stream));
    }
    let conversation_id = super::conversation::ensure_request_id(&headers, &mut chat_body);
    if let Some(id) = conversation_id.as_deref() {
        super::conversation::merge_history(&state.data_dir, id, &mut chat_body);
    }

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    // inflight guard：随执行路径持有至请求结束（§4.5）
    let guard = state.inflight_guard();

    // 统一池间路由：Responses 既可投影到 WorkBuddy，也可投影到 Trae SOLO。
    // 这样 Codex/Responses 客户端与 OpenAI/Anthropic 客户端使用同一套资源调度与
    // 账号轮换逻辑；GPT Image 等图片服务仍不在此路径内。
    let route = match dispatch::resolve_target(&state, &model, &chat_body) {
        Ok(route) => route,
        Err(error) => return dispatch_error_response(error, Protocol::Responses, &model),
    };
    let explicit = chat_body.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
    let hint = effective_effort_hint(&state, route.effort_hint, explicit);
    let body_vec = apply_effort_hint(serde_json::to_vec(&chat_body).unwrap_or_default(), hint);
    match route.pool {
        TargetPool::Buddy => {
            if state.wb_tool_exec.load(std::sync::atomic::Ordering::Relaxed)
                && super::wb_toolexec::responses_declares_web_search(&peek)
            {
                return wb_route::wb_tool_exec_chat(
                    state_clone, chat_body, route.model, stream, start_ts, key_str, guard,
                )
                .await;
            }
            if stream {
                wb_route::wb_stream_chat(state_clone, body_vec, route.model, start_ts, Protocol::Responses, key_str, guard)
            } else {
                wb_route::wb_aggregate_chat(state_clone, body_vec, route.model, stream, start_ts, Protocol::Responses, key_str, guard).await
            }
        }
        TargetPool::Trae => {
            // Trae SOLO 的 Responses 适配由 sse.rs 完成；非流式结果会包装为标准
            // Responses 对象，流式结果发送 response.* 事件。
            if stream {
                stream_chat(state_clone, body_vec, route.model, true, start_ts, Protocol::Responses, key_str, guard, conversation_id.clone())
            } else {
                aggregate_chat(state_clone, body_vec, route.model, false, start_ts, Protocol::Responses, key_str, guard, conversation_id.clone()).await
            }
        }
        TargetPool::Custom => {
            match super::custom_models::find_enabled(&state.data_dir, &model) {
                Some(cm) => {
                    if stream {
                        custom_route::custom_stream_chat(state_clone, body_vec, route.model, cm, start_ts, Protocol::Responses, key_str, guard)
                    } else {
                        custom_route::custom_aggregate_chat(state_clone, body_vec, route.model, cm, false, start_ts, Protocol::Responses, key_str, guard).await
                    }
                }
                None => openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
            }
        }
    }
}

/// Anthropic Messages 端点（F-39：+Anthropic 适配）
/// 请求：POST /v1/messages，鉴权支持 x-api-key 或 Authorization: Bearer
pub async fn messages(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return anthropic_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "invalid_request_error",
            "request body exceeds 8MB limit",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 校验 JSON 并预读 stream/model，再整体转为 OpenAI 内部格式复用现有链路
    let mut peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return anthropic_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    if peek.get("messages").and_then(|m| m.as_array()).is_none() {
        return anthropic_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "messages: field required",
        );
    }
    let conversation_id = super::conversation::ensure_request_id(&headers, &mut peek);
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    let mut internal: Value = serde_json::from_slice(&super::payload::anthropic_to_openai(&body))
        .unwrap_or_else(|_| json!({"messages": []}));
    if let Some(id) = conversation_id.as_deref() {
        internal["conversation_id"] = json!(id);
        super::conversation::merge_history(&state.data_dir, id, &mut internal);
    }
    let body_vec = serde_json::to_vec(&internal).unwrap_or_else(|_| super::payload::anthropic_to_openai(&body));
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 统一调度分流点（§4.1）：resolve_target 决定资源池/回退/错误矩阵；
    // guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = state.inflight_guard();
    match dispatch::resolve_target(&state, &model, &internal) {
        Err(e) => dispatch_error_response(e, Protocol::Anthropic, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考：Anthropic 侧 thinking 参数视为显式请求
                let explicit = peek.get("thinking").map_or(false, |t| !t.is_null());
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::Anthropic, key_str, guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard).await;
            }
            TargetPool::Trae => {
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard, conversation_id.clone())
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, guard, conversation_id.clone()).await
                }
            }
            TargetPool::Custom => {
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::Anthropic, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::Anthropic, key_str, guard).await
                        }
                    }
                    None => anthropic_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
                }
            }
        },
    }
}

/// OpenAI legacy text completions 端点（T9）
/// prompt（string 或 string[]）转单条 user message 复用现有链路，响应包装回
/// text_completion 结构。suffix/echo/logprobs/n 等参数不支持（忽略，上游单次补全）
pub async fn completions(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {}", e),
            )
        }
    };
    let prompt_text = match peek.get("prompt") {
        Some(Value::String(s)) => s.clone(),
        // 多段 prompt：拼接为单个 prompt（上游一次只产出一个补全，无法返回多 choice）
        Some(Value::Array(arr)) => {
            let parts: Vec<String> = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
            if parts.is_empty() {
                return openai_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "prompt: array must contain strings",
                );
            }
            parts.join("\n\n")
        }
        _ => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "prompt: field required",
            )
        }
    };
    if prompt_text.trim().is_empty() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "prompt: must not be empty",
        );
    }
    let stream = peek.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&state.default_model)
        .to_string();

    // prompt → user message，复用 /v1/chat/completions 内部链路
    let internal = json!({
        "model": model,
        "stream": stream,
        "messages": [{ "role": "user", "content": prompt_text }],
    });
    let body_vec = internal.to_string().into_bytes();

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());

    // 统一调度分流点（§4.1）；guard 随执行路径持有至请求结束
    let guard = state.inflight_guard();
    match dispatch::resolve_target(&state, &model, &internal) {
        Err(e) => dispatch_error_response(e, Protocol::OpenAiText, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考（text completions 无 effort 字段 → 默认思考直接生效）
                let hint = effective_effort_hint(&state, r.effort_hint, false);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAiText, key_str, guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard).await;
            }
            TargetPool::Trae => {
                if stream {
                    stream_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard, None)
                } else {
                    aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, guard, None).await
                }
            }
            TargetPool::Custom => {
                match super::custom_models::find_enabled(&state.data_dir, &model) {
                    Some(cm) => {
                        if stream {
                            custom_route::custom_stream_chat(state_clone, body_vec, r.model, cm, start_ts, Protocol::OpenAiText, key_str, guard)
                        } else {
                            custom_route::custom_aggregate_chat(state_clone, body_vec, r.model, cm, stream, start_ts, Protocol::OpenAiText, key_str, guard).await
                        }
                    }
                    None => openai_error(StatusCode::NOT_FOUND, "model_not_found", &format!("自定义模型 {} 已被删除", model)),
                }
            }
        },
    }
}

/// /v1/embeddings：上游 SOLO 无向量能力，明确返回 501（不做假实现）
pub async fn embeddings() -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_supported",
        "上游服务无 embeddings 能力，本网关不支持 /v1/embeddings，请使用 /v1/chat/completions 或 /v1/completions",
    )
}

/// /v1/images/generations 文生图（T5.4/F-63）：投影 WB 上游生图端点
pub async fn images_generations(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, false).await
}

/// /v1/images/edits 图生图（T5.4/F-63）：接受 JSON（image 为 base64/data URL）。
/// 注：OpenAI SDK 默认 multipart/form-data；本端点仅接受 JSON 变体（零新增依赖红线），
/// 客户端需将图像读为 base64 后以 JSON 提交。
pub async fn images_edits(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(state, key_id, body, true).await
}

/// 参考图/参考视频素材上传。素材只绑定当前 API Key，落盘后等待 Trae
/// 原生资源上传适配器消费；不返回本地文件路径，也不把素材写入 API 日志。
/// 为保持零新增依赖，客户端发送 JSON `{filename,mime_type,data_base64}`；
/// 也接受完整 data URL 作为 `data_base64`。
pub async fn assets_upload(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
) -> Response {
    let _guard = state.inflight_guard();
    let owner = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    if owner == "anonymous" {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "api_key_required",
            "素材上传必须使用已启用的 API Key",
        );
    }
    let input: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("素材上传 JSON 无效: {error}"),
            )
        }
    };
    let filename = input
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("upload")
        .to_string();
    if filename.len() > 128 {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", "filename 过长");
    }
    let mut declared_mime = input
        .get("mime_type")
        .or_else(|| input.get("content_type"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let encoded = input
        .get("data_base64")
        .or_else(|| input.get("data"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let Some(encoded) = encoded else {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "缺少 data_base64 字段",
        );
    };
    let encoded = if let Some((header, data)) = encoded.split_once(",") {
        if header.starts_with("data:") {
            if declared_mime.is_none() {
                declared_mime = header
                    .strip_prefix("data:")
                    .and_then(|value| value.split(';').next())
                    .map(str::to_string);
            }
            data
        } else {
            encoded
        }
    } else {
        encoded
    };
    let bytes = match base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        encoded,
    ) {
        Ok(bytes) => bytes,
        Err(error) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_asset",
                &format!("data_base64 无效: {error}"),
            )
        }
    };
    let _limit_permit = match state
        .limiter
        .acquire(&owner, LimitKind::AssetUpload, bytes.len() as u64)
    {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    match assets::create(
        &state.data_dir,
        &owner,
        &filename,
        declared_mime.as_deref(),
        &bytes,
    ) {
        Ok(record) => {
            let mut response = json!({
                "object": "asset",
                "id": record.id,
                "filename": record.filename,
                "mime_type": record.mime_type,
                "bytes": record.size,
                "sha256": record.sha256,
                "created_at": record.created_at,
                "expires_at": record.expires_at,
            });
            // 公网素材基址是显式 opt-in；未设置时不返回任何可访问地址。
            if let Ok(url) = assets::public_url_for_owned(&state.data_dir, &owner, &record.id) {
                response["content_url"] = json!(url);
            }
            Json(response).into_response()
        }
        Err(error) => openai_error(StatusCode::BAD_REQUEST, "invalid_asset", &error),
    }
}

#[derive(Debug, serde::Deserialize)]
pub struct PublicAssetQuery {
    token: String,
}

/// 短时素材链接的内容端点。只有带随机 token 的地址可读；API Key 不会出现在
/// URL 中，也不会写入日志。该端点只在用户显式配置公网素材基址并将链接交给
/// 上游时使用，普通 API 客户端仍应通过 `/v1/assets` 上传并用 Key 管理素材。
pub async fn assets_content(
    State(state): State<Arc<ApiSharedState>>,
    Path(asset_id): Path<String>,
    Query(query): Query<PublicAssetQuery>,
) -> Response {
    match assets::read_public(&state.data_dir, &asset_id, &query.token) {
        Ok((record, bytes)) => {
            let mut builder = Response::builder()
                .status(StatusCode::OK)
                .header("content-type", record.mime_type)
                .header("content-length", bytes.len());
            for (name, value) in asset_security_headers() {
                builder = builder.header(name, value);
            }
            builder
                .body(Body::from(bytes))
                .unwrap_or_else(|_| internal_error_response())
        }
        Err(_) => openai_error(StatusCode::NOT_FOUND, "asset_not_found", "素材不存在、已过期或链接无效"),
    }
}

fn asset_security_headers() -> [(&'static str, &'static str); 3] {
    [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
    ]
}

/// W-02 Seedance 文生视频入口。Work 积分账号异步转发到 Trae Work CN
/// 原生 SSE 接口，客户端通过任务查询接口获取最终资源地址。
pub async fn videos_generations(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let _guard = state.inflight_guard();
    let key_str = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }
    let input: Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
        Err(error) => {
            return openai_error(
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                &format!("invalid JSON body: {error}"),
            )
        }
    };
    let (model, prompt) = match video::validate_request(&input) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &error),
    };
    let input = match video::resolve_asset_references(&state.data_dir, &key_str, &input) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_asset", &error),
    };
    let idempotency_key = headers
        .get("idempotency-key")
        .or_else(|| headers.get("x-request-id"))
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let scoped_idempotency_key = idempotency_key
        .as_deref()
        .map(|key| video::scoped_idempotency_key(&key_str, key));
    if let Some(key) = scoped_idempotency_key.as_deref() {
        if let Some(task) = video::find_idempotent(key) {
            let detail = json!({ "task": task, "request_key": key_str, "idempotent_replay": true });
            return Response::builder()
                .status(StatusCode::ACCEPTED)
                .header("content-type", "application/json")
                .body(Body::from(detail.to_string()))
                .unwrap_or_else(|_| internal_error_response());
        }
    }
    let _limit_permit = match state
        .limiter
        .acquire(&key_str, LimitKind::VideoSubmission, body.len() as u64)
    {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    let tried = HashSet::new();
    let account = match state.pool.pick_excluding_for(&tried, ResourceKind::Work) {
        Some(account) => account,
        None => {
            return openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_work_credits",
                "没有可用的 Trae Work 账号或 Work 积分已耗尽",
            )
        }
    };
    let task = video::create_pending_for(model.clone(), prompt, &key_str);
    if let Some(key) = scoped_idempotency_key.as_deref() {
        video::bind_idempotency(key, &task.id);
    }
    video::start_native_task(state.clone(), task.id.clone(), input, account);
    state.logger.log_request(
        "trae",
        "POST",
        "/v1/videos/generations",
        &model,
        false,
        202,
        "queued",
        0,
        Some("Seedance native SSE task queued"),
    );
    let detail = json!({ "task": task, "request_key": key_str });
    Response::builder()
        .status(StatusCode::ACCEPTED)
        .header("content-type", "application/json")
        .body(Body::from(detail.to_string()))
        .unwrap_or_else(|_| internal_error_response())
}

/// 查询 Seedance 任务状态；仅返回本地任务索引，不伪造上游完成结果。
pub async fn video_task(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    Path(task_id): Path<String>,
) -> Response {
    let owner_key_id = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    match video::get(&task_id) {
        Some(mut task) if video::visible_to(&task, &owner_key_id) => {
            // 运行地址可能在重启/迁移后变化；内容地址按当前部署环境动态重算。
            if super::video_store::artifact_path(&state.data_dir, &task_id)
                .map(|path| path.is_file())
                .unwrap_or(false)
            {
                task.content_url = super::video_store::content_url(&task_id).ok();
            }
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&task).unwrap_or_else(|_| "{}".into())))
                .unwrap_or_else(|_| internal_error_response())
        }
        Some(_) => openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found"),
        None => openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found"),
    }
}

/// 分发已缓存的视频文件。文件通过后台线程分块读取，不把整个视频载入内存；
/// 任务仍按 API Key 所有权隔离。若本地缓存失败但上游地址仍可用，返回 307
/// 作为兼容性兜底，客户端可继续从上游取回本次结果。
pub async fn video_content(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    Path(task_id): Path<String>,
) -> Response {
    let owner_key_id = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    let Some(task) = video::get(&task_id) else {
        return openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found");
    };
    if !video::visible_to(&task, &owner_key_id) {
        return openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found");
    }

    let path = match super::video_store::artifact_path(&state.data_dir, &task_id) {
        Ok(path) => path,
        Err(_) => return openai_error(StatusCode::NOT_FOUND, "video_not_found", "video artifact not found"),
    };
    let metadata = std::fs::metadata(&path).ok().filter(|m| m.is_file());
    if metadata.is_none() {
        if let Some(url) = task.video_url {
            return Response::builder()
                .status(StatusCode::TEMPORARY_REDIRECT)
                .header("location", url)
                .header("cache-control", "no-store")
                .body(Body::empty())
                .unwrap_or_else(|_| internal_error_response());
        }
        return openai_error(StatusCode::NOT_FOUND, "video_not_found", "video artifact not found");
    }
    let size = metadata.map(|m| m.len()).unwrap_or(0);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    let path_for_reader = path.clone();
    std::thread::spawn(move || {
        let mut file = match std::fs::File::open(path_for_reader) {
            Ok(file) => file,
            Err(error) => {
                let _ = tx.blocking_send(Err(error));
                return;
            }
        };
        let mut buffer = vec![0u8; 128 * 1024];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(Ok(bytes::Bytes::copy_from_slice(&buffer[..n]))).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = tx.blocking_send(Err(error));
                    break;
                }
            }
        }
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "video/mp4")
        .header("content-length", size)
        .header("cache-control", "private, max-age=3600")
        .body(Body::from_stream(ReceiverStream::new(rx)))
        .unwrap_or_else(|_| internal_error_response())
}

async fn images_entry(
    state: Arc<ApiSharedState>,
    key_id: Option<Extension<KeyId>>,
    body: axum::body::Bytes,
    is_edit: bool,
) -> Response {
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // inflight guard（§4.5）：async fn 全程 inline await，作用域即请求生命周期
    let _guard = state.inflight_guard();
    if body.len() > MAX_BODY_BYTES {
        return openai_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "request body exceeds 8MB limit");
    }
    let peek: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &format!("invalid JSON body: {}", e)),
    };
    let model = peek
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("hy4")
        .to_string();
    let prompt = peek.get("prompt").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let image_b64 = peek.get("image").and_then(|v| v.as_str()).map(str::to_string);
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let start_ts = std::time::Instant::now();

    // 校验（目录命中 + 图片模态 + prompt/image 非空）
    let catalog = wb_catalog::load(&state.data_dir);
    if let Err((code, msg)) = super::wb_images::validate(
        &catalog,
        &model,
        &prompt,
        if is_edit { Some(image_b64.as_deref().unwrap_or("")) } else { None },
    ) {
        return openai_error(
            StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_REQUEST),
            "invalid_request_error",
            &msg,
        );
    }

    // 取健康 WB 账号（生图无粘性语义，任一健康账号）；携带当前请求 Key 的
    // 约束（P1 修复4c）：白名单 allowed_accounts 过滤 + dedicated 专一锁定，
    // 与 wb_route 同款解析；Key 无约束/匿名（constraints_for 为 None）时不限制
    let (allowed_set, dedicated) = {
        let key_constraints = super::api_keys::constraints_for(&state.data_dir, &key_str);
        let allowed: Option<HashSet<String>> = key_constraints
            .as_ref()
            .map(|k| k.allowed_accounts.iter().cloned().collect())
            .filter(|s: &HashSet<String>| !s.is_empty());
        let dedicated: Option<String> = key_constraints
            .as_ref()
            .filter(|k| k.schedule_mode == super::api_keys::MODE_DEDICATED)
            .map(|k| {
                if k.dedicated_account.is_empty() {
                    k.allowed_accounts.first().cloned().unwrap_or_default()
                } else {
                    k.dedicated_account.clone()
                }
            })
            .filter(|s: &String| !s.is_empty());
        (allowed, dedicated)
    };
    let picked = {
        let tried = HashSet::new();
        state
            .wb_pool
            .pick_excluding_constrained(&tried, allowed_set.as_ref(), dedicated.as_deref())
    };
    let Some(picked) = picked else {
        return openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", "no healthy WB account available");
    };
    let creds = super::wb_upstream::WbCreds {
        id: picked.uid.clone(),
        uid: picked.uid.clone(),
        name: String::new(),
        token: picked.jwt.clone(),
        domain: picked.domain.clone(),
        enterprise_id: picked.enterprise_id.clone(),
        global_region: picked.global_region,
    };

    let result = tokio::task::spawn_blocking(move || super::wb_images::generate(&creds, &peek))
        .await
        .unwrap_or_else(|e| Err((500u16, format!("task join error: {e}"))));
    let duration_ms = start_ts.elapsed().as_millis() as u64;
    match result {
        Ok(resp) => {
            // P1 修复4a：WB 账号服务的请求记 wb 桶（is_wb=true），与 Trae 侧分账
            state.record_usage(true, &model, &picked.uid, &key_str, true, false, duration_ms, 0, 0);
            state.wb_pool.note_success(&picked.uid);
            // P1 修复4b：上游池为 buddy，日志池归属同步纠正
            state.logger.log_request(
                "buddy", "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, 200, &picked.uid, duration_ms, None,
            );
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(resp.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Err((code, msg)) => {
            state.record_usage(true, &model, &picked.uid, &key_str, false, false, duration_ms, 0, 0);
            state.logger.log_request(
                "buddy", "POST",
                if is_edit { "/v1/images/edits" } else { "/v1/images/generations" },
                &model, false, code, &picked.uid, duration_ms, Some(&msg),
            );
            openai_error(
                StatusCode::from_u16(code).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
                &msg,
            )
        }
    }
}

// ==================== Streaming ====================

#[allow(clippy::too_many_arguments)]
fn stream_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard, conversation_id: Option<String>) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // SSE keep-alive 15s（T2.7/F-34 §5.5 #7）：防中间层回收长流。
    // P1 修复：原 ticker 独占持有 sender 克隆，主任务发完 [DONE] 后流因
    // sender 未全部关闭而无法终结（普通 HTTP 客户端只能靠断连收尾）。
    // 改用 tokio::sync::watch：主任务结束（DoneSignal Drop）置 done=true，
    // ticker select! 收到退出信号即退出 → rx 关闭 → 流正常结束
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.tick().await; // 首个 tick 立即返回，跳过
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        if tx2
                            .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    // 主任务已结束：ticker 退出，放行流终结
                    _ = done_rx.changed() => break,
                }
            }
        });
    }

    tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至流结束（§4.5：客户端断连/流终止由
        // 任务结束 Drop 兜底释放）
        let _inflight = guard;
        // 主任务结束（含 panic 展开）→ 通知 keep-alive ticker 退出（P1 修复3）
        let _done = DoneSignal(done_tx);
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            // Responses 仅走 WB 上游；solo 管线不会收到，兜底给 resp_ id
            Protocol::Responses => format!("resp_{}", now_ts()),
        };
        let mut tried = HashSet::new();

        // 每个请求最多尝试池内全部账号；`tried` 保证同一请求不会重复取号。
        // 旧实现固定只轮换 3 个账号，池规模超过 3 时会过早返回 no healthy account。
        let max_rotate = state.pool.count().max(1);
        for _ in 0..max_rotate {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let converted = super::payload::prepare_llm_chat_body_with_conversation(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                conversation_id.as_deref(),
            );

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限为本次池内账号数
            let mut same_attempt: u32 = 0;
            loop {
                // TTFB 计时（请求发起 → 上游首行到达，与 wb_route 同语义）：
                // AtomicU64 0 哨兵 = 尚未读到首行（首字超时等失败路径不产出 ttfb）
                let ttfb_start = std::time::Instant::now();
                let ttfb_us = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok(reader) => {
                        // 首字超时 10s（T2.7/F-34，与 wb_upstream 同款包装）：建连后
                        // 首字节 10s 未到视为上游故障 → 冷却换号；首字节到达后正常
                        // 流速不受限（后续行无超时）
                        let lines = match super::wb_upstream::lines_with_first_byte_timeout(reader) {
                            Ok(l) => l,
                            Err(()) => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} first-byte timeout(10s)", picked.uid));
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    504, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    Some("first byte timeout"),
                                );
                                break; // 换号
                            }
                        };
                        // 首行打点包装：首次成功读到上游行即记录 ttfb（0 哨兵防重复
                        // 覆盖；叠加首字超时包装，语义为「请求发起 → 首行到达」）
                        let lines = {
                            let ttfb_flag = ttfb_us.clone();
                            lines.map(move |l| {
                                let _ = ttfb_flag.compare_exchange(
                                    0,
                                    ttfb_start.elapsed().as_micros() as u64,
                                    std::sync::atomic::Ordering::Relaxed,
                                    std::sync::atomic::Ordering::Relaxed,
                                );
                                l
                            })
                        };
                        let lines =
                            Box::new(lines) as Box<dyn Iterator<Item = String> + Send>;
                        // 连接成功 → 开始流式转换，mid-stream error 只冷却不轮换
                        let (error_info, sent_any, up_usage, assistant_message) = match proto {
                            Protocol::OpenAi => {
                                sse::stream_convert_lines_capture(lines, tx.clone(), &chat_id)
                            }
                            Protocol::OpenAiText => {
                                sse::stream_convert_text_lines_capture(lines, tx.clone(), &chat_id, &model)
                            }
                            Protocol::Anthropic => {
                                sse::stream_convert_anthropic_lines_capture(lines, tx.clone(), &chat_id, &model)
                            }
                            Protocol::Responses => {
                                sse::stream_convert_responses_lines_capture(
                                    lines,
                                    tx.clone(),
                                    &chat_id,
                                    &model,
                                )
                            }
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        // TTFB：首行到达耗时（未读到首行 → None，不输出该字段）
                        let ttfb_ms = {
                            let us = ttfb_us.load(std::sync::atomic::Ordering::Relaxed);
                            if us == 0 { None } else { Some(us / 1000) }
                        };
                        // 用量记账（流式结束即落盘）
                        {
                            let (pt, ct) = up_usage.as_ref().map(extract_tokens).unwrap_or((0, 0));
                            state.record_usage(
                                false, &model, &picked.uid, &key_id, error_info.is_none(), true,
                                duration_ms, pt, ct,
                            );
                        }
                        let stream_succeeded = error_info.is_none();
                        if stream_succeeded {
                            if let (Some(cid), Some(assistant)) =
                                (conversation_id.as_deref(), assistant_message.as_ref())
                            {
                                super::conversation::record_turn(
                                    &state.data_dir, cid, &model, "trae", &body_vec,
                                    Some(assistant),
                                );
                            }
                        }
                        if let Some((code, msg)) = error_info {
                            let kind = classify_solo_error(code, &msg);
                            if kind != ErrKind::None {
                                state.pool.note_error(&picked.uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                            }
                            // 关键换号边界：上游已返回业务错误、但转换器尚未向客户端
                            // 发出任何事件时，可以安全丢弃本次流并尝试下一个账号。
                            // 一旦已经发出内容，重放会造成客户端收到两段拼接响应，仍按
                            // 原语义就地结束并透传错误。
                            if !sent_any && kind != ErrKind::None {
                                state.logger.log_request_ttfb(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &picked.uid, duration_ms, ttfb_ms, Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                                }
                                break; // 首字节前失败：换号重放，客户端尚未看到半截响应
                            }
                            if !sent_any {
                                // 流未开始：错误延迟下发（sse 层未透传，由这里统一发）
                                send_stream_error(&tx, proto, code, &msg);
                            }
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, ttfb_ms, Some(&msg),
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                            }
                        } else {
                            state.pool.note_success(&picked.uid);
                            state.logger.log_request_ttfb(
                                "trae", "POST", proto.log_path(), &model, stream,
                                200, &picked.uid, duration_ms, ttfb_ms, None,
                            );
                            if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                state.logger.log_debug(&picked.uid, &converted, None, 200, None);
                            }
                        }
                        return; // 流式结束后直接返回
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 分级重试策略表（T2.2/F-33 v1.2，与 wb_route 保持一致）
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                // 同账号重试：不 note_error 不冷却
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                let kind = classify_error(status, &resp_body);
                                state.pool.note_error(&picked.uid, kind);
                                let preview = safe_slice(&resp_body, 200);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={} body={}", picked.uid, status, preview));
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    Some(&format!("upstream status={}", status)),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&preview));
                                }
                                break; // 换号（same_attempt 随新账号归零）
                            }
                            RetryAction::Fatal => {
                                // 不冷却：请求本身问题（换号无意义），终止并透传上游错误体
                                let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&msg));
                                }
                                send_stream_error(&tx, proto, status as i64, &msg);
                                return;
                            }
                        }
                    }
                }
            }
        }

        // 所有账号不可用
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        state.record_usage(false, &model, "none", &key_id, false, true, duration_ms, 0, 0);
        let diag = state.pool.diagnose();
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "trae", "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, Some("no healthy account"),
        );
        // 写入 app.log 供排查
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let local_ts = now + 8 * 3600;
            let h = (local_ts % 86400) / 3600;
            let m = (local_ts % 3600) / 60;
            let s = local_ts % 60;
            let diag_line = format!(
                "NO_HEALTHY_ACCOUNT [{:02}:{:02}:{:02}] tried={} pool={} reasons=[{}]",
                h, m, s, tried.len(), diag.len(), diag_summary.join(", "),
            );
            if let Some(mut f) = state.logger.get_writer() {
                use std::io::Write;
                let _ = writeln!(f, "[DEBUG] {}", diag_line);
            }
        }
        match proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(
                    "data: {\"error\":{\"message\":\"no healthy account available\",\"type\":\"api_error\",\"code\":\"no_healthy_account\"}}\n\n",
                )));
                let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
            }
            Protocol::Anthropic => {
                let err = json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": "no healthy account available",
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: error\ndata: {}\n\n",
                    err
                ))));
            }
            Protocol::Responses => {
                let body = json!({
                    "type": "response.failed",
                    "response": {
                        "id": format!("resp_{}", now_ts()),
                        "object": "response",
                        "status": "failed",
                        "output": [],
                        "error": {"code": "no_healthy_account", "message": "no healthy account available"},
                    },
                });
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: response.failed\ndata: {}\n\n",
                    body
                ))));
            }
        }
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal server error"))
                .unwrap()
        })
}

// ==================== Non-streaming ====================

async fn aggregate_chat(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard, conversation_id: Option<String>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）
        let _inflight = guard;
        let mut tried = HashSet::new();

        let max_rotate = state.pool.count().max(1);
        for _ in 0..max_rotate {
            let picked = match state.pool.pick_excluding(&tried) {
                Some(p) => p,
                None => break,
            };
            tried.insert(picked.uid.clone());
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let converted = super::payload::prepare_llm_chat_body_with_conversation(
                &body_vec, &state.default_model, &picked.uid, &picked.device_id, &picked.machine_id,
                conversation_id.as_deref(),
            );

            // 分级重试（T2.2/F-33，与 wb_route 同一张表）：same_attempt 为同账号
            // 重试计数，换号后随新账号归零；总轮换上限为本次池内账号数
            let mut same_attempt: u32 = 0;
            loop {
                match make_upstream_request(&picked.jwt, &picked.uid, &picked.device_id, &picked.machine_id, &converted) {
                    Ok(reader) => {
                        let chat_id = match proto {
                            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
                            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
                            Protocol::Anthropic => format!("msg_{}", now_ts()),
                            Protocol::Responses => format!("resp_{}", now_ts()),
                        };
                        let (resp, error_info) = match proto {
                            Protocol::OpenAi => sse::aggregate(reader, &chat_id),
                            Protocol::OpenAiText => sse::aggregate_text(reader, &chat_id, &model),
                            Protocol::Anthropic => sse::aggregate_anthropic(reader, &chat_id, &model),
                            Protocol::Responses => sse::aggregate(reader, &chat_id),
                        };
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(r), None) => {
                                if let Some(cid) = conversation_id.as_deref() {
                                    super::conversation::record_turn(
                                        &state.data_dir, cid, &model, "trae", &body_vec,
                                        Some(&r),
                                    );
                                }
                                // 用量记账（成功：token 数从聚合响应 usage 提取）
                                let (pt, ct) = r.get("usage").map(extract_tokens).unwrap_or((0, 0));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, true, stream,
                                    duration_ms, pt, ct,
                                );
                                state.pool.note_success(&picked.uid);
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &picked.uid, duration_ms, None,
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(r.to_string().as_bytes()), 200, None);
                                }
                                let response = if proto == Protocol::Responses {
                                    super::wb_responses::completion_to_responses(&r, &chat_id, &model)
                                } else {
                                    r
                                };
                                return Ok(response);
                            }
                            (None, Some((code, msg))) => {
                                if code == sse::INCOMPLETE_STREAM_ERROR_CODE {
                                    return Err(AggregateFail::Incomplete { message: msg });
                                }
                                let kind = classify_solo_error(code, &msg);
                                state.pool.note_error(&picked.uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} code={} msg={}", picked.uid, code, msg));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    duration_ms, 0, 0,
                                );
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    200, &picked.uid, duration_ms, Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 200, Some(&msg));
                                }
                                break; // 流内错误：冷却换号
                            }
                            _ => {
                                state.pool.note_error(&picked.uid, ErrKind::Server);
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    duration_ms, 0, 0,
                                );
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    502, &picked.uid, duration_ms, Some("empty response"),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, None, 502, Some("empty response"));
                                }
                                break; // 换号
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 分级重试策略表（T2.2/F-33 v1.2，与 wb_route 保持一致）
                        match retry_plan(status, &resp_body, same_attempt, retry_after) {
                            RetryAction::RetrySame { delay_ms } => {
                                // 同账号重试：不 note_error 不冷却
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                let kind = classify_error(status, &resp_body);
                                state.pool.note_error(&picked.uid, kind);
                                *safe_lock(&state.last_error) =
                                    Some(format!("uid={} status={}", picked.uid, status));
                                state.record_usage(
                                    false, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                );
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    Some(&format!("upstream status={}", status)),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&resp_body));
                                }
                                break; // 换号（same_attempt 随新账号归零）
                            }
                            RetryAction::Fatal => {
                                // 不冷却：请求本身问题（换号无意义），终止并透传上游错误体
                                let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                                state.logger.log_request(
                                    "trae", "POST", proto.log_path(), &model, stream,
                                    status, &picked.uid, start_ts.elapsed().as_millis() as u64,
                                    Some(&msg),
                                );
                                if state.debug_enabled.load(std::sync::atomic::Ordering::Relaxed) {
                                    state.logger.log_debug(&picked.uid, &converted, Some(resp_body.as_bytes()), status, Some(&msg));
                                }
                                return Err(AggregateFail::Upstream(status, msg));
                            }
                        }
                    }
                }
            }
        }

        let duration_ms = start_ts.elapsed().as_millis() as u64;
        let diag = state.pool.diagnose();
        // 用量记账（所有账号不可用）
        state.record_usage(false, &model, "none", &key_id, false, stream, duration_ms, 0, 0);
        let diag_summary: Vec<String> = diag
            .iter()
            .map(|d| {
                let credits_str = d.credits.map(|c| format!("{:.0}", c)).unwrap_or_else(|| "N/A".to_string());
                let cd_str = if d.until > 0 { format!(",cd={}s", d.until.saturating_sub(now_ts() as i64)) } else { String::new() };
                let exp_str = d.credits_expire_at.filter(|&e| e > 0).map(|e| format!(",exp={}", e)).unwrap_or_default();
                let dis_str = if d.disabled { ",DIS" } else { "" };
                format!("{}({}:{},cr={}{}{}{})", d.name, d.uid.get(..8).unwrap_or(&d.uid), d.reason, credits_str, cd_str, exp_str, dis_str)
            })
            .collect();
        state.logger.log_request(
            "trae", "POST", proto.log_path(), &model, stream,
            503, "none", duration_ms, Some("no healthy account"),
        );
        // 写入诊断日志
        {
            if let Some(mut f) = state.logger.get_writer() {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[DEBUG] NO_HEALTHY_ACCOUNT(non-stream) tried={} pool={} reasons=[{}]",
                    tried.len(), diag.len(), diag_summary.join(", "),
                );
            }
        }
        Err(AggregateFail::NoHealthy {
            message: "no healthy account available".to_string(),
        })
    })
    .await;

    match result {
        Ok(Ok(resp)) => {
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(resp.to_string()))
                .unwrap_or_else(|_| {
                Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(Body::from("internal server error"))
                    .unwrap()
                })
        }
        Ok(Err(AggregateFail::NoHealthy { message: msg })) => {
            let response = match proto {
                Protocol::OpenAi | Protocol::OpenAiText | Protocol::Responses => {
                    openai_error(StatusCode::SERVICE_UNAVAILABLE, "no_healthy_account", &msg)
                }
                Protocol::Anthropic => anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &msg),
            };
            response
        }
        Ok(Err(AggregateFail::Incomplete { message: msg })) => match proto {
            Protocol::Anthropic => anthropic_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "incomplete_upstream",
                &msg,
            ),
            _ => openai_error(StatusCode::SERVICE_UNAVAILABLE, "incomplete_upstream", &msg),
        },
        Ok(Err(AggregateFail::Upstream(status, msg))) => {
            // Fatal：上游错误体透传（不冷却），按协议格式化并保留上游状态码
            let sc = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            match proto {
                Protocol::Anthropic => anthropic_error(sc, "api_error", &msg),
                _ => openai_error(sc, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== Upstream Request ====================

/// 上游错误体读取上限（P2 修复11）：防止上游异常回包把整响应读进内存
const MAX_UPSTREAM_ERR_BYTES: usize = 64 * 1024;

/// 限量读取上游错误体（P2 修复11）：最多 MAX_UPSTREAM_ERR_BYTES，读满截断
fn read_limited_body(response: ureq::Response) -> String {
    let mut buf = Vec::new();
    let mut limited = response
        .into_reader()
        .take(MAX_UPSTREAM_ERR_BYTES as u64);
    let _ = limited.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

fn make_upstream_request(
    jwt: &str,
    _uid: &str,
    device_id: &str,
    machine_id: &str,
    body: &[u8],
) -> Result<Box<dyn Read + Send>, (u16, String, Option<u64>)> {
    let url = format!("{}{}", AGENT_HOST, EP_LLM_CHAT);
    let referer = format!("{}{}", REFERER_BASE, EP_LLM_CHAT);
    let trace_id = format!(
        "00-{}-{}-01",
        uuid_like_id(),
        uuid_like_id()
    );
    let request_id = format!("req_{}", uuid_like_id());

    let resp = streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("accept-encoding", "gzip, deflate, br, zstd")
        .set("user-agent", "TraeClient/TTNet")
        .set("x-ide-token", jwt)
        .set("x-app-id", APP_ID)
        .set("x-app-version", "default")
        .set("x-app-version-code", IDE_VERSION_CODE)
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-device-brand", "CREFG-XX")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", device_id)
        .set("x-machine-id", machine_id)
        .set("x-os-version", "Windows 11 Home China")
        .set("request-traffic-type", "prod")
        .set("package-type", "stable_cn")
        .set("x-lgw-req-sdk-type", "3")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("x-ss-dp", "787976")
        .set("app-version", IDE_VERSION)
        .set("x-custom-trace-id", &trace_id[..16])
        .set("x-flow-traceparent", &format!("04-{}-{}-01", &trace_id[3..35], uuid_like_id()))
        .set("x-tt-trace-id", &trace_id)
        .set("x-request-id", &request_id)
        .set("referer", &referer)
        .send_bytes(body);

    match resp {
        Ok(r) => Ok(Box::new(r.into_reader())),
        Err(ureq::Error::Status(code, response)) => {
            // Retry-After（秒）解析（P1 修复1）：供分级重试表 429 退避决策；
            // header 需在 into_reader 消费响应前读取
            let retry_after = response
                .header("retry-after")
                .and_then(|v| v.trim().parse::<u64>().ok());
            let body = read_limited_body(response);
            Err((code, body, retry_after))
        }
        Err(e) => {
            let err_str = format!("{}", e);
            // 区分 DNS 解析失败 / 连接超时 / TLS 错误，提供更精准的诊断
            let detail = if err_str.contains("dns") || err_str.contains("resolve") || err_str.contains("name resolution") {
                format!("DNS解析失败（{} 无法解析），请检查网络或代理设置: {}", AGENT_HOST, e)
            } else if err_str.contains("timed out") || err_str.contains("timeout") {
                format!("连接超时（{} 10秒内未响应），请检查网络连通性: {}", AGENT_HOST, e)
            } else if err_str.contains("tls") || err_str.contains("certificate") || err_str.contains("ssl") {
                format!("TLS证书验证失败: {}", e)
            } else {
                format!("传输错误: {}", e)
            };
            Err((502, detail, None))
        }
    }
}

// ==================== Helpers ====================

/// OpenAI 错误响应格式（wb_route 复用）
pub(crate) fn openai_error(status: StatusCode, code: &str, msg: &str) -> Response {    let body = json!({
        "error": {
            "message": msg,
            "type": "api_error",
            "code": code,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

fn limit_error_response(error: LimitError) -> Response {
    let mut response = openai_error(StatusCode::TOO_MANY_REQUESTS, error.code(), error.message());
    if let Ok(value) = HeaderValue::from_str(&error.retry_after_secs().to_string()) {
        response.headers_mut().insert(header::RETRY_AFTER, value);
    }
    response
}

/// Anthropic 错误响应格式：{"type":"error","error":{"type","message"}}（wb_route 复用）
pub(crate) fn anthropic_error(status: StatusCode, err_type: &str, msg: &str) -> Response {
    let body = json!({
        "type": "error",
        "error": {
            "type": err_type,
            "message": msg,
        }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"internal error\"}}"))
                .unwrap()
        })
}

/// 流式错误统一下发：sse 层在流未开始时不透传错误（留待重试决策），
/// 由此处按客户端协议格式化错误事件并收尾
pub(crate) fn send_stream_error(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    match proto {
        Protocol::OpenAi | Protocol::OpenAiText => {
            let body = json!({
                "error": { "message": msg, "type": "api_error", "code": code }
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
        Protocol::Anthropic => {
            let err = json!({
                "type": "error",
                "error": { "type": "api_error", "message": msg },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            let body = json!({
                "type": "response.failed",
                "response": {
                    "id": format!("resp_{}", now_ts()),
                    "object": "response",
                    "status": "failed",
                    "output": [],
                    "error": {"code": code.to_string(), "message": msg},
                },
            });
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
    }
}

fn now_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 生成类似 UUID 的十六进制字符串，用于 trace-id 等请求头
fn uuid_like_id() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let nanos = now.as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}

fn safe_slice(s: &str, n: usize) -> &str {
    // P2 修复6：沿字符边界向前找 <=n 的最大可截断点。原实现非字符边界时
    // 回退返回整串（可能超长），且字节切片 &s[..n] 在多字节字符中间会 panic
    // （上游错误 JSON 常含中文）；现保证输出永不超过 n 字节且不 panic
    if n >= s.len() {
        return s;
    }
    let mut cut = 0;
    for (i, _) in s.char_indices().take_while(|(i, _)| *i <= n) {
        cut = i;
    }
    &s[..cut]
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

    use super::*;
    use aiwork_core::{CoreStore, CostPolicy, MockChatExecutor, NewUser, Principal, QuotaGrant, UserRole};
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::HeaderMap;

    use crate::api_server::{CoreBridge, CoreMode};

    struct CoreFixture {
        dir: PathBuf,
        state: Arc<ApiSharedState>,
        principal: Principal,
    }

    impl Drop for CoreFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn core_fixture(grant: i64, scopes: &[&str]) -> CoreFixture {
        let dir = std::env::temp_dir().join(format!(
            "aiwork-routes-core-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "route-user".into(),
                    name: "Route test user".into(),
                    role: UserRole::User,
                },
                "bootstrap",
            )
            .unwrap();
        let key = store
            .issue_api_key(
                "route-user",
                "route-test",
                scopes.iter().map(|scope| (*scope).to_owned()).collect::<BTreeSet<_>>(),
                "bootstrap",
            )
            .unwrap();
        store
            .upsert_cost_policy(CostPolicy {
                id: "route-chat-policy".into(),
                endpoint: "chat".into(),
                model_pattern: "mock-*".into(),
                resource_kind: "chat_request".into(),
                reserve_amount: 1,
                max_actual_amount: Some(1),
                version: 1,
                enabled: true,
            })
            .unwrap();
        if grant > 0 {
            store
                .grant(QuotaGrant {
                    user_id: "route-user".into(),
                    resource_kind: "chat_request".into(),
                    amount: grant,
                    actor_user_id: "route-user".into(),
                    reason: "route test grant".into(),
                })
                .unwrap();
        }
        let principal = Principal {
            user_id: "route-user".into(),
            key_id: key.id,
            scopes: scopes.iter().map(|scope| (*scope).to_owned()).collect(),
        };
        let state = Arc::new(ApiSharedState {
            core: Some(Arc::new(CoreBridge::new(store, CoreMode::Enforce))),
            pool: super::super::pool::ApiPool::new(),
            wb_pool: super::super::pool::ApiPool::new(),
            wb_enabled: std::sync::atomic::AtomicBool::new(false),
            wb_sanitize: std::sync::atomic::AtomicBool::new(true),
            wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
            wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
            wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
            wb_sticky: super::super::wb_sticky::StickyStore::default(),
            pool_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
            model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
            default_model: "mock-1".into(),
            data_dir: dir.clone(),
            cors_origins: String::new(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limiter: super::super::limits::RateLimiter::from_env(),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            logger: super::super::ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        });
        CoreFixture { dir, state, principal }
    }

    fn chat_body(stream: bool) -> Bytes {
        chat_body_for_model("mock-1", stream)
    }

    fn chat_body_for_model(model: &str, stream: bool) -> Bytes {
        Bytes::from(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "stream": stream,
        }).to_string())
    }

    #[tokio::test]
    async fn core_models_keep_existing_catalog_shape_with_scope() {
        let fixture = core_fixture(1, &["models:read", "chat:invoke"]);
        let response = models(
            State(fixture.state.clone()),
            Some(Extension(fixture.principal.clone())),
        ).await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn core_models_reject_missing_scope() {
        let fixture = core_fixture(1, &[]);
        let response = models(
            State(fixture.state.clone()),
            Some(Extension(fixture.principal.clone())),
        ).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn core_chat_stream_returns_phase1_not_implemented() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            HeaderMap::new(),
            chat_body(true),
        ).await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let balance = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .balance("route-user", "chat_request")
            .unwrap();
        assert_eq!(balance.held, 0);
    }

    #[tokio::test]
    async fn core_chat_requires_idempotency_key_even_with_request_id() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "trace-only".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn core_chat_reports_insufficient_quota_before_dispatch() {
        let fixture = core_fixture(0, &["chat:invoke"]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "quota-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn core_chat_rejects_missing_scope_before_preflight() {
        let fixture = core_fixture(1, &[]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "scope-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let balance = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .balance("route-user", "chat_request")
            .unwrap();
        assert_eq!(balance.held, 0);
    }

    #[tokio::test]
    async fn core_chat_rejects_missing_budget_policy_before_reservation() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "policy-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body_for_model("unpriced-model", false),
        ).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let balance = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .balance("route-user", "chat_request")
            .unwrap();
        assert_eq!(balance.available, 1);
        assert_eq!(balance.held, 0);
    }

    #[tokio::test]
    async fn core_chat_reserves_before_dispatch_and_replays_idempotency() {
        let fixture = core_fixture(2, &["chat:invoke"]);
        let executor = Arc::new(MockChatExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "idem-1".parse().unwrap());
        let first = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers.clone(),
            chat_body(false),
        ).await;
        let second = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        clear_core_test_executor();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(executor.calls().len(), 1);
        let persisted = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .preflight_chat(
                &fixture.principal,
                &fixture.principal.key_id,
                Some("idem-1"),
                &serde_json::from_slice(&chat_body(false)).unwrap(),
            )
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result.as_ref().and_then(|result| result.status), Some(200));
        assert_eq!(persisted.result.as_ref().and_then(|result| result.error_code.as_deref()), None);
        let balance = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .balance("route-user", "chat_request")
            .unwrap();
        assert_eq!(balance.available, 1);
        assert_eq!(balance.held, 0);
        assert_eq!(
            first.headers().get("x-request-id"),
            second.headers().get("x-request-id")
        );
    }

    #[tokio::test]
    async fn core_replay_of_failed_request_is_not_success() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let first = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("replay-failed"), &body)
            .unwrap();
        let reservation = first.reservation.as_ref().unwrap();
        bridge
            .settle_chat(
                &fixture.principal,
                &reservation.id,
                ChatOutcome::Failure(UpstreamError::Rejected {
                    status: 400,
                    code: Some("bad_request".into()),
                }),
            )
            .unwrap();
        let persisted = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("replay-failed"), &body)
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result.as_ref().and_then(|result| result.status), Some(400));
        assert!(persisted.result.as_ref().and_then(|result| result.error_code.as_deref()).is_some());

        let executor = Arc::new(MockChatExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "replay-failed".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        clear_core_test_executor();
        assert_ne!(response.status(), StatusCode::OK);
        assert!(executor.calls().is_empty());
        assert_eq!(response.headers().get("x-request-id").is_some(), true);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "idempotent_replay_failed");
    }

    #[tokio::test]
    async fn core_replay_of_unknown_request_is_not_success() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let first = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("replay-unknown"), &body)
            .unwrap();
        let reservation = first.reservation.as_ref().unwrap();
        bridge
            .settle_chat(
                &fixture.principal,
                &reservation.id,
                ChatOutcome::Upstream(UpstreamError::Disconnected),
            )
            .unwrap();
        let persisted = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("replay-unknown"), &body)
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result.as_ref().and_then(|result| result.status), None);
        assert_eq!(persisted.result.as_ref().and_then(|result| result.error_code.as_deref()), Some("upstream_uncertain"));

        let executor = Arc::new(MockChatExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "replay-unknown".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        clear_core_test_executor();
        assert_ne!(response.status(), StatusCode::OK);
        assert!(executor.calls().is_empty());
        assert_eq!(response.headers().get("x-request-id").is_some(), true);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "idempotent_replay_unknown");
    }

    #[test]
    fn core_503_response_settles_unknown() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let (completion, error) = sse::aggregate(
            std::io::Cursor::new("event: output\ndata: {\"response\":\"partial\"}\n\n"),
            "incomplete",
        );
        assert!(completion.is_none());
        assert_eq!(error.as_ref().map(|(code, _)| *code), Some(sse::INCOMPLETE_STREAM_ERROR_CODE));
        let preflight = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("transport-503"), &body)
            .unwrap();
        let reservation = preflight.reservation.as_ref().unwrap().clone();
        let context = CoreChatContext {
            bridge: bridge.clone(),
            principal: fixture.principal.clone(),
            request_id: preflight.request_id,
            reservation_id: reservation.id,
            reservation_amount: reservation.amount,
        };
        let response = settle_core_response(
            &context,
            Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .body(Body::from("upstream unavailable"))
                .unwrap(),
        );
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let stored = bridge.store.reservation_for_request(&context.request_id).unwrap().unwrap();
        assert_eq!(stored.state, aiwork_core::ReservationState::Unknown);
    }

    #[test]
    fn core_disconnect_settles_unknown() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let preflight = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("transport-disconnect"), &body)
            .unwrap();
        let reservation = preflight.reservation.as_ref().unwrap().clone();
        let context = CoreChatContext {
            bridge: bridge.clone(),
            principal: fixture.principal.clone(),
            request_id: preflight.request_id,
            reservation_id: reservation.id,
            reservation_amount: reservation.amount,
        };
        let response = settle_core_outcome(
            &context,
            openai_error(StatusCode::BAD_GATEWAY, "upstream_error", "disconnected"),
            ChatOutcome::Upstream(UpstreamError::Disconnected),
        );
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let stored = bridge.store.reservation_for_request(&context.request_id).unwrap().unwrap();
        assert_eq!(stored.state, aiwork_core::ReservationState::Unknown);
    }

    #[test]
    fn upstream_usage_does_not_set_core_actual_amount() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let preflight = bridge
            .preflight_chat(&fixture.principal, &fixture.principal.key_id, Some("usage-untrusted"), &body)
            .unwrap();
        let reservation = preflight.reservation.as_ref().unwrap().clone();
        let context = CoreChatContext {
            bridge: bridge.clone(),
            principal: fixture.principal.clone(),
            request_id: preflight.request_id,
            reservation_id: reservation.id,
            reservation_amount: reservation.amount,
        };
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(json!({"usage": {"total_tokens": 0}}).to_string()))
            .unwrap();
        settle_core_response(&context, response);
        let balance = bridge.balance("route-user", "chat_request").unwrap();
        assert_eq!(balance.available, 0);
        assert_eq!(balance.held, 0);
    }

    #[test]
    fn public_health_payload_contains_liveness_only() {
        let payload = public_health_payload();
        assert_eq!(payload.get("status").and_then(Value::as_str), Some("ok"));
        assert_eq!(payload.get("running").and_then(Value::as_bool), Some(true));
        for sensitive in ["pool", "active_uid", "last_error", "total_requests", "wb"] {
            assert!(payload.get(sensitive).is_none(), "public health leaked {sensitive}");
        }
    }

    #[test]
    fn public_healthz_payload_contains_liveness_only() {
        let (status, payload) = healthz_payload(true, false, false);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(payload.get("status").and_then(Value::as_str), Some("ok"));
        assert_eq!(payload.get("running").and_then(Value::as_bool), Some(true));
        for sensitive in ["pool", "solo_available", "wb_available", "total_credits"] {
            assert!(payload.get(sensitive).is_none(), "public healthz leaked {sensitive}");
        }
        let (status, payload) = healthz_payload(false, false, false);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(payload.get("status").and_then(Value::as_str), Some("unavailable"));
    }

    #[test]
    fn asset_response_security_headers_are_non_cacheable() {
        let headers = asset_security_headers();
        assert_eq!(headers[0], ("cache-control", "no-store"));
        assert_eq!(headers[1], ("referrer-policy", "no-referrer"));
        assert_eq!(headers[2], ("x-content-type-options", "nosniff"));
    }

    #[test]
    fn rate_limit_error_contains_retry_after() {
        let response = limit_error_response(LimitError::VideoSubmissions);
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("60")
        );
    }

    // ==================== P2 修复6：safe_slice 字符边界截断 ====================

    #[test]
    fn safe_slice_cuts_on_char_boundary() {
        // ASCII：n 在串内 → 精确截断
        assert_eq!(safe_slice("hello world", 5), "hello");
        // n 超长 → 原文
        assert_eq!(safe_slice("abc", 100), "abc");
        // n 等于串长 → 原文
        assert_eq!(safe_slice("abc", 3), "abc");
        // n=0 → 空串（原实现会 panic）
        assert_eq!(safe_slice("中文", 0), "");
    }

    #[test]
    fn safe_slice_never_returns_overlong_or_panics() {
        // 200 落在多字节字符中间：沿边界向前取最近可截断点，不 panic、不回退整串
        let s = format!("{}{}", "a".repeat(199), "中文中文中文");
        let out = safe_slice(&s, 200);
        assert!(out.len() <= 200, "输出不得超过 n 字节");
        assert_eq!(out.len(), 199, "应回退到最近字符边界（199 处）");
        // n=1/2 落在 3 字节「中」的中间 → 边界回退到 0
        assert_eq!(safe_slice("中文", 1), "");
        assert_eq!(safe_slice("中文", 2), "");
        assert_eq!(safe_slice("中文", 3), "中");
        // 任意 n 都不 panic 且不超长
        let body = "上游错误：{\"code\":1005,\"message\":\"套餐额度用尽\"}";
        for n in 0..=body.len() {
            let out = safe_slice(body, n);
            assert!(out.len() <= n);
            assert!(body.starts_with(out));
        }
    }
}
