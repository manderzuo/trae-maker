use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio_stream::wrappers::ReceiverStream;

use aiwork_core::{
    require_scope, ChatExecutionResult, CoreError, CoreJob, CoreJobAttempt, JobState, LeaseState,
    Principal, RequestState, UpstreamError, UpstreamLeaseGrant, VideoJobEnqueueResult,
};

use super::custom_route;
use super::api_keys::{self, KeyLimits, ResolvedKey};
use super::assets;
use super::core_bridge::{
    CancelSupport, ChatOutcome, CoreLeaseError, CoreUpstreamExecutor, LeasePreflightResult,
    LeaseUpstreamAdapter, StreamTerminalOutcome, UpstreamOutcome,
};
use super::core_video::{VideoAdapterOutcome, VideoCancelOutcome, VideoExecutionRequest};
use super::core_stream::core_stream_chat;
use super::dispatch::{self, DispatchError, TargetPool};
use super::retry::{retry_plan, RetryAction};
use super::seedance_chat::{
    append_inline_image_asset_ids, is_seedance_model, project_chat_to_video, InlineImage,
};
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
    Attribution { message: String },
}

struct CoreChatContext {
    bridge: Arc<super::CoreBridge>,
    principal: Principal,
    request_id: String,
    reservation_id: String,
    lease: Option<UpstreamLeaseGrant>,
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

fn require_legacy_capability(
    key_id: &str,
    resolved_key: Option<&ResolvedKey>,
    capability: &str,
) -> Result<KeyLimits, Response> {
    if key_id == "anonymous" {
        if matches!(capability, api_keys::CAPABILITY_VIDEO | api_keys::CAPABILITY_ASSETS) {
            return Err(openai_error(
                StatusCode::UNAUTHORIZED,
                "api_key_required",
                "此接口必须使用已启用的 API Key",
            ));
        }
        return Ok(KeyLimits::default());
    }
    let Some(policy) = resolved_key else {
        return Err(openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth_snapshot_missing",
            "authenticated API Key policy snapshot is missing",
        ));
    };
    if policy.id != key_id {
        return Err(openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "auth_snapshot_missing",
            "authenticated API Key policy snapshot does not match the request key",
        ));
    }
    if !policy.capabilities.iter().any(|value| value == capability) {
        return Err(openai_error(
            StatusCode::FORBIDDEN,
            "capability_not_allowed",
            &format!("API Key 未启用能力: {capability}"),
        ));
    }
    Ok(policy.limits.clone())
}

fn legacy_request_guard(
    state: &ApiSharedState,
    key_id: &str,
    key_limits: &KeyLimits,
) -> Result<InflightGuard, Response> {
    state
        .acquire_request(key_id, key_limits)
        .map(|permit| state.inflight_guard_with_permit(permit))
        .map_err(limit_error_response)
}

/// 获取 legacy 文字请求的 request permit，并在同一请求生命周期内持有
/// 保守 Token reservation。视频/assets 继续使用 `legacy_request_guard`，
/// 因为它们不消费每日 Token 额度。
fn legacy_text_request_guard(
    state: &ApiSharedState,
    key_id: &str,
    key_limits: &KeyLimits,
) -> Result<InflightGuard, Response> {
    let permit = state
        .acquire_request(key_id, key_limits)
        .map_err(limit_error_response)?;
    let reservation = match api_keys::reserve_token_quota(
        &state.data_dir,
        key_id,
        &super::usage::key_quota_day(),
        key_limits.daily_tokens,
    ) {
        Ok(reservation) => reservation,
        Err(api_keys::TokenQuotaError::Exceeded { limit }) => {
            drop(permit);
            return Err(super::auth::quota_exceeded(
                limit,
                api_keys::QuotaKind::Tokens,
            ));
        }
        Err(api_keys::TokenQuotaError::KeyUnavailable) => {
            drop(permit);
            return Err(super::auth::quota_exceeded(
                key_limits.daily_tokens,
                api_keys::QuotaKind::Tokens,
            ));
        }
    };
    Ok(state.inflight_guard_with_permit_and_reservation(permit, reservation))
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
        CoreError::KeyQuotaNotConfigured { .. } => openai_error(
            StatusCode::CONFLICT,
            "key_quota_not_configured",
            "Core key quota is not configured",
        ),
        CoreError::QuotaMigrationPending { .. } => openai_error(
            StatusCode::CONFLICT,
            "quota_migration_pending",
            "Core quota requires reconciliation before use",
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

pub(super) fn scheduler_endpoint_not_enabled_response() -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "scheduler_endpoint_not_enabled",
        "this endpoint is not enabled by the Core scheduler",
    )
}

pub(super) fn core_lease_error_response(error: CoreLeaseError) -> Response {
    match error {
        CoreLeaseError::EndpointNotEnabled => scheduler_endpoint_not_enabled_response(),
        CoreLeaseError::Core(error) => core_error_response(error),
        CoreLeaseError::Schedule(error) => match error {
            aiwork_core::ScheduleError::NoFreshObservation => openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_fresh_observation",
                "no fresh upstream observation is eligible",
            ),
            aiwork_core::ScheduleError::NoUpstreamCapacity
            | aiwork_core::ScheduleError::CapabilityMismatch
            | aiwork_core::ScheduleError::AccountCooling => openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_upstream_capacity",
                "no upstream capacity is eligible",
            ),
            aiwork_core::ScheduleError::MissingScope(scope) => core_scope_error(&scope),
            aiwork_core::ScheduleError::InvalidRequestIdentity => {
                openai_error(StatusCode::UNAUTHORIZED, "unauthorized", "invalid Core request identity")
            }
            aiwork_core::ScheduleError::IdempotencyConflict => openai_error(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "Idempotency-Key was already used for a different request",
            ),
            aiwork_core::ScheduleError::Core(error) => core_error_response(error),
            _ => openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "scheduler_error",
                "Core scheduler operation failed",
            ),
        },
    }
}

fn with_core_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = HeaderValue::from_str(request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn core_replay_response(preflight: &super::core_bridge::PreflightResult) -> Response {
    core_replay_response_for(
        &preflight.request_id,
        preflight.state,
        preflight.result.as_ref(),
        None,
    )
}

pub(super) fn core_lease_replay_response(preflight: &LeasePreflightResult) -> Response {
    core_replay_response_for(
        &preflight.request_id,
        preflight.state,
        preflight.result.as_ref(),
        preflight.replay_lease.as_ref().map(|lease| lease.state),
    )
}

fn core_replay_response_for(
    request_id: &str,
    state: RequestState,
    result: Option<&aiwork_core::RequestResult>,
    replay_lease_state: Option<LeaseState>,
) -> Response {
    let successful_result = result.filter(|result| {
        matches!(result.status, Some(status) if (200..300).contains(&status))
            && result.error_code.is_none()
    });
    let successful_replay = successful_result.is_some()
        || matches!(replay_lease_state, Some(LeaseState::Succeeded));
    let (status, code, message, body) = if successful_replay {
        (
            StatusCode::OK,
            "idempotent_replay",
            "request already completed; response replay is safe",
            json!({
                "id": request_id,
                "object": "chat.completion",
                "choices": [],
                "idempotent_replay": true,
            }),
        )
    } else if result.is_none()
        && !matches!(state, RequestState::Settled | RequestState::Succeeded | RequestState::Failed | RequestState::Unknown)
    {
        (
            StatusCode::CONFLICT,
            "request_in_progress",
            "request is already in progress; upstream was not called again",
            json!({"request_id": request_id}),
        )
    } else if matches!(
        result.and_then(|result| result.error_code.as_deref()),
        Some("upstream_uncertain" | "transport_unknown" | "transport_timeout" | "lease_expired")
    ) || result.is_none() {
        (
            StatusCode::CONFLICT,
            "idempotent_replay_unknown",
            "request outcome is unknown; upstream was not called again",
            json!({"request_id": request_id}),
        )
    } else {
        (
            StatusCode::CONFLICT,
            "idempotent_replay_failed",
            "request already failed; upstream was not called again",
            json!({"request_id": request_id}),
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
    with_core_request_id(response, request_id)
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

fn settle_core_lease_outcome(
    context: &CoreChatContext,
    mut response: Response,
    outcome: UpstreamOutcome,
) -> (Response, bool) {
    response.headers_mut().remove("x-aiwork-core-actual-amount");
    response.headers_mut().remove("x-aiwork-core-outcome");
    let now_ms = chrono::Utc::now().timestamp_millis();
    let Some(lease) = context.lease.as_ref() else {
        return (scheduler_endpoint_not_enabled_response(), false);
    };
    let health_category = core_health_category(&outcome);
    match context.bridge.settle_chat_lease(
        &context.principal,
        &lease.lease_id,
        outcome.lease_outcome(now_ms),
    ) {
        Ok(settlement) => {
            // The lease/quota transition is authoritative. Health persistence
            // is a separate account projection and must never release a user
            // hold if its best-effort diagnostic write fails.
            if settlement.applied {
                let _ = context.bridge.store.record_upstream_health_transition(
                    &lease.account_ref,
                    Some(&context.request_id),
                    Some(&lease.lease_id),
                    "chat_request",
                    Some(&lease.observation_id),
                    health_category,
                    now_ms,
                );
            }
            (with_core_request_id(response, &context.request_id), settlement.applied)
        }
        Err(error) => (core_lease_error_response(error), false),
    }
}

fn core_health_category(outcome: &UpstreamOutcome) -> &'static str {
    match outcome {
        UpstreamOutcome::Success { .. } => "success",
        UpstreamOutcome::TransportUnknown { reason, .. } => {
            let reason = reason.to_ascii_lowercase();
            if reason.contains("timeout") {
                "transport_timeout"
            } else if reason.contains("disconnect") {
                "transport_unknown"
            } else {
                "server"
            }
        }
        UpstreamOutcome::Rejected { status, code, .. } => {
            let code = normalize_health_code(code);
            if matches!(*status, 401) || code.contains("session") {
                "session_dead"
            } else if matches!(*status, 403) || code.contains("forbidden") {
                "forbidden"
            } else if matches!(*status, 404) || code.contains("not_found") || code.contains("notfound") {
                "not_found"
            } else if code == "hard_credit" || code.contains("hard_credit") {
                "hard_credit"
            } else if code == "plan_limit" || code.contains("plan_limit") {
                "plan_limit"
            } else if code == "soft_rate" || code.contains("soft_rate") {
                "soft_rate"
            } else if matches!(*status, 429) || code.contains("rate") {
                "soft_rate"
            } else if (500..600).contains(status) {
                "server"
            } else {
                "client"
            }
        }
    }
}

fn normalize_health_code(value: &str) -> String {
    let mut normalized = String::with_capacity(value.len() + 4);
    let mut previous_is_lower_or_digit = false;
    for character in value.chars() {
        if character == '-' || character == ' ' {
            if !normalized.ends_with('_') {
                normalized.push('_');
            }
            previous_is_lower_or_digit = false;
            continue;
        }
        if character.is_ascii_uppercase() {
            if !normalized.is_empty()
                && previous_is_lower_or_digit
                && !normalized.ends_with('_')
            {
                normalized.push('_');
            }
            normalized.push(character.to_ascii_lowercase());
            previous_is_lower_or_digit = false;
        } else {
            normalized.push(character.to_ascii_lowercase());
            previous_is_lower_or_digit = character.is_ascii_lowercase() || character.is_ascii_digit();
        }
    }
    normalized
}

fn log_core_lease_event(
    state: &ApiSharedState,
    context: &CoreChatContext,
    outcome: &UpstreamOutcome,
) {
    let Some(lease) = context.lease.as_ref() else { return; };
    state.logger.log_scheduler_event(
        "upstream.lease_settle",
        Some(&context.request_id),
        Some(&lease.lease_id),
        Some(&lease.account_ref),
        None,
        Some("chat_request"),
        Some(&lease.observation_id),
        Some(core_health_category(outcome)),
    );
}

fn settle_core_lease_response(
    context: &CoreChatContext,
    outcome: UpstreamOutcome,
) -> (Response, bool) {
    match outcome.response() {
        Some(result) if (200..300).contains(&result.status) => {
            let response = Response::builder()
                .status(StatusCode::from_u16(result.status).unwrap_or(StatusCode::OK))
                .header("content-type", "application/json")
                .body(Body::from(result.body.to_string()))
                .unwrap_or_else(|_| internal_error_response());
                settle_core_lease_outcome(context, response, outcome)
        }
        Some(result) => {
            let response = openai_error(
                StatusCode::from_u16(result.status).unwrap_or(StatusCode::BAD_GATEWAY),
                "upstream_error",
                &result.body["error"]["code"].as_str().unwrap_or("upstream_rejected"),
            );
            settle_core_lease_outcome(context, response, outcome)
        }
        None => {
            let response = openai_error(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream transport outcome is unknown",
            );
            settle_core_lease_outcome(context, response, outcome)
        }
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
    static CORE_TEST_EXECUTOR: std::cell::RefCell<Option<Arc<dyn LeaseUpstreamAdapter>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn install_core_test_executor(executor: Arc<dyn LeaseUpstreamAdapter>) {
    CORE_TEST_EXECUTOR.with(|slot| *slot.borrow_mut() = Some(executor));
}

#[cfg(test)]
fn clear_core_test_executor() {
    CORE_TEST_EXECUTOR.with(|slot| *slot.borrow_mut() = None);
}

#[cfg(test)]
fn core_test_executor() -> Option<Arc<dyn LeaseUpstreamAdapter>> {
    CORE_TEST_EXECUTOR.with(|slot| slot.borrow().clone())
}

fn core_route_executor(
    bridge: &super::CoreBridge,
) -> Result<CoreUpstreamExecutor, CoreLeaseError> {
    #[cfg(test)]
    if let Some(adapter) = core_test_executor() {
        return Ok(CoreUpstreamExecutor::from_adapter_for_account_with_provider(
            adapter,
            "mock",
            "mock-account",
            "vault://mock/account",
        ));
    }
    bridge.upstream_executor()
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

pub async fn status(
    State(state): State<Arc<ApiSharedState>>,
    principal: Option<Extension<Principal>>,
) -> Response {
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        let bridge = match state.core.as_ref() {
            Some(bridge) => bridge,
            None => return scheduler_endpoint_not_enabled_response(),
        };
        if bridge.store.authorize_admin_principal(&principal).is_err() {
            return openai_error(
                StatusCode::FORBIDDEN,
                "scheduler_admin_required",
                "scheduler status requires an administrator principal",
            );
        }
        let scheduler = match bridge.scheduler() {
            Ok(runtime) => runtime.scheduler_status_for_admin(
                &principal,
                chrono::Utc::now().timestamp_millis(),
            ),
            Err(error) => Err(error),
        };
        return match scheduler {
            Ok(scheduler) => Json(json!({
                "running": true,
                "scheduler": scheduler,
            }))
            .into_response(),
            Err(error) => openai_error(
                StatusCode::SERVICE_UNAVAILABLE,
                error.code(),
                "scheduler status is unavailable",
            ),
        };
    }

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
    .into_response()
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
    let mut data: Vec<Value> = list
        .iter()
        .filter(|m| wb_enabled || !m.sources.iter().all(|s| s.pool == "buddy"))
        .map(public_unified_model_value)
        .collect();
    // Seedance 是 Work 视频能力，不混入文字统一目录；但公共模型发现接口需要让
    // 外部客户端能够通过同一个 Base URL 发现并选择它。
    data.push(public_seedance_model_value(
        state.pool.has_selectable_for(ResourceKind::Work),
    ));
    Json(json!({ "object": "list", "data": data })).into_response()
}

fn public_unified_model_value(model: &unified_catalog::UnifiedModel) -> Value {
    json!({
        "id": model.id,
        "object": "model",
        "created": 1753600000,
        "owned_by": "unified",
        "display": model.display,
        "rate": model.rate,
        "context_length": model.context_length,
        "max_tokens": model.max_tokens,
        "supports_image": model.supports_image,
        "supported_efforts": model.efforts,
        "capabilities": ["text"],
        "endpoint": "/v1/chat/completions",
        "async": false,
        // 来源池集合：[{pool: "trae"|"buddy"|"custom", rate, enabled}]。
        // 客户端按元数据自决，勿硬编码来源池。
        "sources": model.sources,
        "manual": model.manual,
    })
}

fn public_seedance_model_value(work_available: bool) -> Value {
    let model = unified_catalog::seedance_model(work_available);
    json!({
        "id": model.id,
        "object": "model",
        "created": 1753600000,
        "owned_by": model.vendor,
        "display": model.display,
        "rate": model.rate,
        "context_length": model.context_length,
        "max_tokens": model.max_tokens,
        "supports_image": model.supports_image,
        "supported_efforts": model.efforts,
        "capabilities": ["video"],
        "endpoint": "/v1/videos/generations",
        "async": true,
        "sources": model.sources,
        "manual": model.manual,
    })
}

async fn seedance_chat_completions(
    state: Arc<ApiSharedState>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    core_attribution: Option<Extension<super::bridge_billing::CoreRequestAttribution>>,
    headers: HeaderMap,
    input: Value,
) -> Response {
    if input.get("stream").and_then(Value::as_bool).unwrap_or(false) {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "seedance_stream_unsupported",
            "Seedance Chat 兼容入口暂不支持 stream=true，请使用非流式请求",
        );
    }
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if idempotency_key.is_none() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Seedance Chat 请求必须提供 Idempotency-Key",
        );
    }
    let projection = match project_chat_to_video(&input) {
        Ok(projection) => projection,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, error.code(), error.message()),
    };
    let mut video_input = projection.video_input;
    let idempotency_key = idempotency_key.expect("validated above");
    let owner_key_id = key_id.as_ref().map(|Extension(key)| key.0.as_str());
    if let Err(error) = persist_seedance_inline_assets(
        &state,
        &mut video_input,
        &projection.inline_images,
        owner_key_id,
        principal.as_ref().map(|Extension(value)| value),
        idempotency_key,
    ) {
        return openai_error(StatusCode::BAD_REQUEST, "invalid_asset", &error);
    }
    let body = match serde_json::to_vec(&video_input) {
        Ok(body) => bytes::Bytes::from(body),
        Err(_) => return internal_error_response(),
    };
    let video_response = videos_generations_with_attribution(
        State(state),
        key_id,
        resolved_key,
        principal,
        core_attribution,
        headers,
        body,
    )
    .await;
    if !video_response.status().is_success() {
        return video_response;
    }
    wrap_seedance_video_response(video_response).await
}

fn persist_seedance_inline_assets(
    state: &ApiSharedState,
    video_input: &mut Value,
    inline_images: &[InlineImage],
    legacy_key_id: Option<&str>,
    core_principal: Option<&Principal>,
    idempotency_key: &str,
) -> Result<Vec<String>, String> {
    if inline_images.is_empty() {
        return Ok(Vec::new());
    }
    let owner = if core_enforcing(state) {
        core_principal
            .map(|principal| principal.user_id.as_str())
            .ok_or_else(|| "Core 模式缺少调用方身份".to_string())?
    } else {
        legacy_key_id
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| "参考图上传必须绑定 API Key".to_string())?
    };
    let asset_ids = inline_images
        .iter()
        .enumerate()
        .map(|(index, image)| {
            let mut seed = Vec::new();
            seed.extend_from_slice(b"seedance-inline-asset-v1\0");
            seed.extend_from_slice(state.data_dir.to_string_lossy().as_bytes());
            seed.push(0);
            seed.extend_from_slice(owner.as_bytes());
            seed.push(0);
            seed.extend_from_slice(idempotency_key.as_bytes());
            seed.push(0);
            seed.extend_from_slice(index.to_string().as_bytes());
            seed.push(0);
            seed.extend_from_slice(&image.bytes);
            format!("asset-inline-{:x}", Sha256::digest(seed))
        })
        .collect::<Vec<_>>();
    let candidate_ids = asset_ids.iter().cloned();
    let mut candidate = video_input.clone();
    append_inline_image_asset_ids(&mut candidate, candidate_ids)
        .map_err(|error| error.message().to_string())?;

    let mut persisted_ids = Vec::with_capacity(inline_images.len());
    for (index, image) in inline_images.iter().enumerate() {
        let extension = match image.mime_type.as_str() {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            _ => return Err("参考图 MIME 类型不受支持".into()),
        };
        let filename = format!("seedance-reference-{}.{}", index + 1, extension);
        if core_enforcing(state) {
            let principal = core_principal.ok_or_else(|| "Core 模式缺少调用方身份".to_string())?;
            let bridge = state
                .core
                .as_ref()
                .ok_or_else(|| "Core 素材服务未启用".to_string())?;
            if let Some(existing) = bridge
                .store
                .asset_for_user(principal, &asset_ids[index])
                .map_err(|error| format!("Core 参考图查询失败: {error}"))?
            {
                assets::read_core(
                    &state.data_dir,
                    &existing.storage_ref,
                    existing.size,
                    &existing.sha256,
                )
                .map_err(|error| format!("Core 参考图校验失败: {error}"))?;
                persisted_ids.push(existing.id);
                continue;
            }
            let (record, storage_ref) = assets::write_core_asset_with_id(
                &state.data_dir,
                &principal.user_id,
                &asset_ids[index],
                &filename,
                Some(&image.mime_type),
                &image.bytes,
            )
            .map_err(|error| format!("参考图保存失败: {error}"))?;
            let created_at_ms = i64::try_from(record.created_at.saturating_mul(1_000))
                .map_err(|_| "参考图创建时间无效".to_string())?;
            let expires_at_ms = i64::try_from(record.expires_at.saturating_mul(1_000))
                .map_err(|_| "参考图过期时间无效".to_string())?;
            let input = aiwork_core::CreateAssetInput {
                id: record.id.clone(),
                filename: record.filename.clone(),
                mime_type: record.mime_type.clone(),
                extension: record.extension.clone(),
                size: record.size as i64,
                sha256: record.sha256.clone(),
                storage_ref: storage_ref.clone(),
                content_token_digest: assets::content_token_digest(&record.public_token),
                created_at_ms,
                expires_at_ms,
            };
            if let Err(error) = bridge.store.create_asset(principal, input) {
                let _ = assets::remove_core_asset(&state.data_dir, &storage_ref);
                return Err(format!("Core 参考图登记失败: {error}"));
            }
            persisted_ids.push(record.id);
        } else {
            let owner_key_id = legacy_key_id
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| "参考图上传必须绑定 API Key".to_string())?;
            let record = assets::create_with_id(
                &state.data_dir,
                owner_key_id,
                &asset_ids[index],
                &filename,
                Some(&image.mime_type),
                &image.bytes,
            )
            .map_err(|error| format!("参考图保存失败: {error}"))?;
            persisted_ids.push(record.id);
        }
    }
    append_inline_image_asset_ids(video_input, persisted_ids.clone())
        .map_err(|error| error.message().to_string())?;
    Ok(persisted_ids)
}

async fn wrap_seedance_video_response(response: Response) -> Response {
    let status = response.status();
    let request_id = response.headers().get("x-request-id").cloned();
    let body = match axum::body::to_bytes(response.into_body(), MAX_BODY_BYTES).await {
        Ok(body) => body,
        Err(_) => return internal_error_response(),
    };
    let payload: Value = match serde_json::from_slice(&body) {
        Ok(payload) => payload,
        Err(_) => return internal_error_response(),
    };
    let task = payload.get("task").cloned().unwrap_or(payload);
    let task_id = match task.get("id").and_then(Value::as_str).filter(|id| !id.is_empty()) {
        Some(task_id) => task_id,
        None => return openai_error(StatusCode::BAD_GATEWAY, "video_task_unavailable", "视频任务响应缺少任务 ID"),
    };
    let model = task
        .get("model")
        .and_then(Value::as_str)
        .filter(|model| !model.is_empty())
        .unwrap_or("seedance");
    let task_status = task
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("queued");
    let created = task.get("created_at").and_then(Value::as_u64).unwrap_or_else(now_ts);
    let mut video_task = json!({
        "id": task_id,
        "model": model,
        "status": task_status,
        "status_endpoint": format!("/v1/videos/{task_id}"),
    });
    if task.get("content_url").is_some() {
        video_task["content_endpoint"] = json!(format!("/v1/videos/{task_id}/content"));
    }
    if let Some(updated_at) = task.get("updated_at") {
        video_task["updated_at"] = updated_at.clone();
    }
    let response_body = json!({
        "id": format!("chatcmpl-{task_id}"),
        "object": "chat.completion",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "视频任务已提交，完成后可下载"
            },
            "finish_reason": "video_async"
        }],
        "video_task": video_task,
    });
    let mut builder = Response::builder()
        .status(if status.is_success() { status } else { StatusCode::ACCEPTED })
        .header("content-type", "application/json");
    if let Some(request_id) = request_id {
        builder = builder.header("x-request-id", request_id);
    }
    builder
        .body(Body::from(response_body.to_string()))
        .unwrap_or_else(|_| internal_error_response())
}

pub async fn chat_completions(
    state: State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    chat_completions_with_attribution(state, key_id, resolved_key, principal, None, headers, body).await
}

pub async fn chat_completions_with_attribution(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    core_attribution: Option<Extension<super::bridge_billing::CoreRequestAttribution>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let core_attribution = core_attribution.map(|Extension(value)| value);
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
    if is_seedance_model(&model) {
        return seedance_chat_completions(
            state, key_id, resolved_key, principal,
            core_attribution.map(Extension), headers, peek,
        ).await;
    }
    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = if core_enforcing(&state) {
        KeyLimits::default()
    } else {
        match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_CHAT) {
            Ok(limits) => limits,
            Err(response) => return response,
        }
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        if require_scope(&principal, "chat:invoke").is_err() {
            return core_scope_error("chat:invoke");
        }
        if stream {
            let idempotency_key = headers
                .get("idempotency-key")
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned);
            return core_stream_chat(
                state.clone(),
                principal,
                key_str,
                peek,
                Protocol::OpenAi,
                idempotency_key,
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
        let bridge = match state.core.as_ref() {
            Some(bridge) => bridge.clone(),
            None => return scheduler_endpoint_not_enabled_response(),
        };
        // Keep request validation and quota errors ahead of executor readiness,
        // but do this read-only so a missing binding can never leave a hold or
        // lease behind. The lease preflight repeats the authoritative checks.
        let sanitized_body = super::payload::sanitize_scheduler_chat_body(&peek);
        let estimate = match bridge
            .store
            .estimate_cost("chat", &model, &sanitized_body)
        {
            Ok(estimate) => estimate,
            Err(error) => return core_error_response(error),
        };
        if principal.key_id != key_str {
            return core_error_response(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: key_str.clone(),
            });
        }
        // Resolve the registered executor/explicit account binding before Core
        // can create a user reservation or upstream lease. A missing runtime
        // binding is never permission to enter the legacy ApiPool path. For
        // that fail-closed branch, perform the quota check read-only so a
        // valid but unfunded request still reports quota before 501; when an
        // executor exists, the authoritative lease preflight handles quota
        // and idempotent replay together.
        let executor = match core_route_executor(&bridge) {
            Ok(executor) => executor,
            Err(error) => {
                let balance = match bridge
                    .store
                    .key_quota_balance_for_principal(&principal, &estimate.resource_kind)
                {
                    Ok(balance) => balance,
                    Err(error) => return core_error_response(error),
                };
                if balance.available < estimate.reserve_amount {
                    return core_error_response(CoreError::QuotaInsufficient {
                        available: balance.available,
                        required: estimate.reserve_amount,
                    });
                }
                return core_lease_error_response(error);
            }
        };
        let bound_account_refs = executor.bound_account_refs();
        // Preserve completed idempotent replays without consuming a new
        // execution permit. New Core requests acquire the guard before
        // preflight and keep it through synchronous execution and settlement.
        match bridge.lookup_chat_replay(&principal, &key_str, idempotency_key, &peek) {
            Ok(Some(replay)) => return core_lease_replay_response(&replay),
            Ok(None) => {}
            Err(error) => return core_lease_error_response(error),
        }
        let _request_guard = match state.core_request_guard(&principal.key_id) {
            Ok(guard) => guard,
            Err(error) => return limit_error_response(error),
        };
        let preflight = match bridge.preflight_chat_with_lease_for_accounts(
            &principal,
            &key_str,
            Some(idempotency_key),
            &peek,
            &bound_account_refs,
        ) {
            Ok(preflight) => preflight,
            Err(error) => return core_lease_error_response(error),
        };
        if preflight.execution.is_none() {
            return core_lease_replay_response(&preflight);
        }
        let lease = match preflight.lease {
            Some(lease) => lease,
            None => return scheduler_endpoint_not_enabled_response(),
        };
        let execution = match preflight.execution {
            Some(execution) => execution,
            None => return scheduler_endpoint_not_enabled_response(),
        };
        let reservation = match preflight.reservation.as_ref() {
            Some(reservation) => reservation,
            None => {
                let request_id = preflight.request_id.clone();
                let context = CoreChatContext {
                    bridge,
                    principal,
                    request_id: request_id.clone(),
                    reservation_id: request_id,
                    lease: Some(lease.clone()),
                    reservation_amount: 0,
                };
                let outcome = UpstreamOutcome::TransportUnknown {
                    reason: "lease_context_incomplete".into(),
                    upstream_request_ref: None,
                };
                let (response, applied) = settle_core_lease_response(
                    &context,
                    outcome.clone(),
                );
                if applied {
                    log_core_lease_event(&state, &context, &outcome);
                }
                return response;
            }
        };
        let context = CoreChatContext {
            bridge,
            principal,
            request_id: preflight.request_id,
            reservation_id: reservation.id.clone(),
            lease: Some(lease.clone()),
            reservation_amount: reservation.amount,
        };
        let outcome = executor.execute_nonstream_chat(&lease, execution);
        let (response, applied) = settle_core_lease_response(&context, outcome.clone());
        if applied {
            log_core_lease_event(&state, &context, &outcome);
        }
        return response;
    }

    let body_vec = serde_json::to_vec(&peek).unwrap_or_else(|_| body.to_vec());

    // 统一调度分流点（§4.1 ③~⑥）：resolve_target 决定资源池/会话池粘性/跨池回退/
    // 错误矩阵，替代原 resolve_wb_target 单向判定；默认策略下行为与改造前一致（§9.1）。
    // inflight guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = match legacy_text_request_guard(&state, &key_str, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    let response = match dispatch::resolve_target(&state, &model, &peek) {
        Err(e) => dispatch_error_response(e, Protocol::OpenAi, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考：客户端未带 reasoning_effort 时注入 high
                let explicit = peek.get("reasoning_effort").and_then(|v| v.as_str()).is_some();
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAi, key_str, resolved_key.clone(), guard)
                } else {
                    wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, resolved_key.clone(), guard).await
                }
            }
            TargetPool::Trae => {
                if stream {
                    stream_chat_with_attribution(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard, conversation_id.clone(), core_attribution.clone())
                } else {
                    aggregate_chat_with_attribution(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAi, key_str, guard, conversation_id.clone(), core_attribution.clone()).await
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
    response
}

/// Codex Responses API 端点（T4.1/F-40）：POST /v1/responses
///
/// 请求投影为 OpenAI 内部格式后复用 WB 上游既有管线（取号/重试/粘性/脱敏一份）。
/// 仅支持 WB 上游模型（Codex CLI `wire_api="responses"` 直配 base_url 的目标场景）；
/// 脱敏沿用全局 `wb_sanitize` 开关，审核命中按既有分级重试表退回重试。
pub async fn responses_api(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
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

    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = if core_enforcing(&state) {
        KeyLimits::default()
    } else {
        match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_CHAT) {
            Ok(limits) => limits,
            Err(response) => return response,
        }
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if core_enforcing(&state) {
        if !stream {
            return scheduler_endpoint_not_enabled_response();
        }
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        let idempotency_key = headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        return core_stream_chat(
            state.clone(),
            principal,
            key_str,
            chat_body,
            Protocol::Responses,
            idempotency_key,
        );
    }

    let state_clone = state.clone();
    let start_ts = std::time::Instant::now();
    // inflight guard：随执行路径持有至请求结束（§4.5）
    let guard = match legacy_text_request_guard(&state, &key_str, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };

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
                    state_clone, chat_body, route.model, stream, start_ts, key_str, resolved_key.clone(), guard,
                )
                .await;
            }
            if stream {
                wb_route::wb_stream_chat(state_clone, body_vec, route.model, start_ts, Protocol::Responses, key_str, resolved_key.clone(), guard)
            } else {
                wb_route::wb_aggregate_chat(state_clone, body_vec, route.model, stream, start_ts, Protocol::Responses, key_str, resolved_key.clone(), guard).await
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
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
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

    // Preserve Core's fail-closed behavior for the non-stream Messages
    // endpoint, including malformed/empty bodies. Only an explicit stream
    // request enters the new Core lease path below.
    if core_enforcing(&state) {
        let is_stream = serde_json::from_slice::<Value>(&body)
            .ok()
            .and_then(|value| value.get("stream").and_then(Value::as_bool))
            .unwrap_or(false);
        if !is_stream {
            return scheduler_endpoint_not_enabled_response();
        }
    }

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
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = if core_enforcing(&state) {
        KeyLimits::default()
    } else {
        match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_CHAT) {
            Ok(limits) => limits,
            Err(response) => return response,
        }
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    if core_enforcing(&state) {
        if !stream {
            return scheduler_endpoint_not_enabled_response();
        }
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        let idempotency_key = headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned);
        return core_stream_chat(
            state.clone(),
            principal,
            key_str,
            internal,
            Protocol::Anthropic,
            idempotency_key,
        );
    }

    // 统一调度分流点（§4.1）：resolve_target 决定资源池/回退/错误矩阵；
    // guard 随执行路径持有至请求结束（流式含整个后台任务）
    let guard = match legacy_text_request_guard(&state, &key_str, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    match dispatch::resolve_target(&state, &model, &internal) {
        Err(e) => dispatch_error_response(e, Protocol::Anthropic, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考：Anthropic 侧 thinking 参数视为显式请求
                let explicit = peek.get("thinking").map_or(false, |t| !t.is_null());
                let hint = effective_effort_hint(&state, r.effort_hint, explicit);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::Anthropic, key_str, resolved_key.clone(), guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::Anthropic, key_str, resolved_key.clone(), guard).await;
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
    resolved_key: Option<Extension<ResolvedKey>>,
    body: axum::body::Bytes,
) -> Response {
    if body.len() > MAX_BODY_BYTES {
        return openai_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_too_large",
            "request body exceeds 8MB limit",
        );
    }

    if core_enforcing(&state) {
        return scheduler_endpoint_not_enabled_response();
    }

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
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_CHAT) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    // 统一调度分流点（§4.1）；guard 随执行路径持有至请求结束
    let guard = match legacy_text_request_guard(&state, &key_str, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
    match dispatch::resolve_target(&state, &model, &internal) {
        Err(e) => dispatch_error_response(e, Protocol::OpenAiText, &model),
        Ok(r) => match r.pool {
            TargetPool::Buddy => {
                // T5.3 默认深度思考（text completions 无 effort 字段 → 默认思考直接生效）
                let hint = effective_effort_hint(&state, r.effort_hint, false);
                let body_vec = apply_effort_hint(body_vec, hint);
                if stream {
                    return wb_route::wb_stream_chat(state_clone, body_vec, r.model, start_ts, Protocol::OpenAiText, key_str, resolved_key.clone(), guard);
                }
                return wb_route::wb_aggregate_chat(state_clone, body_vec, r.model, stream, start_ts, Protocol::OpenAiText, key_str, resolved_key.clone(), guard).await;
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
    resolved_key: Option<Extension<ResolvedKey>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(
        state,
        key_id,
        resolved_key.map(|Extension(key)| key),
        body,
        false,
    )
    .await
}

/// /v1/images/edits 图生图（T5.4/F-63）：接受 JSON（image 为 base64/data URL）。
/// 注：OpenAI SDK 默认 multipart/form-data；本端点仅接受 JSON 变体（零新增依赖红线），
/// 客户端需将图像读为 base64 后以 JSON 提交。
pub async fn images_edits(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    body: axum::body::Bytes,
) -> Response {
    images_entry(
        state,
        key_id,
        resolved_key.map(|Extension(key)| key),
        body,
        true,
    )
    .await
}

/// 参考图/参考视频素材上传。素材只绑定当前 API Key，落盘后等待 Trae
/// 原生资源上传适配器消费；不返回本地文件路径，也不把素材写入 API 日志。
/// 为保持零新增依赖，客户端发送 JSON `{filename,mime_type,data_base64}`；
/// 也接受完整 data URL 作为 `data_base64`。
pub async fn assets_upload(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    body: axum::body::Bytes,
) -> Response {
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        return core_assets_upload(state, principal, body).await;
    }
    let owner = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = match require_legacy_capability(&owner, resolved_key.as_ref(), api_keys::CAPABILITY_ASSETS) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    if owner == "anonymous" {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "api_key_required",
            "素材上传必须使用已启用的 API Key",
        );
    }
    let parsed = match parse_asset_upload(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let filename = parsed.filename;
    let declared_mime = parsed.declared_mime;
    let bytes = parsed.bytes;
    let request_permit = match state.acquire_limit(
        &owner,
        LimitKind::AssetUpload,
        bytes.len() as u64,
        &key_limits,
    ) {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    let _guard = state.inflight_guard_with_permit(request_permit);
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
            // 短时查看地址是显式 opt-in；Seedance 原生上传不依赖该地址。
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
/// URL 中，也不会写入日志。该端点只在用户显式配置公网素材基址后提供短时查看，
/// 普通 API 客户端仍应通过 `/v1/assets` 上传并用 Key 管理素材；Seedance 输入走原生上传。
pub async fn assets_content(
    State(state): State<Arc<ApiSharedState>>,
    Path(asset_id): Path<String>,
    Query(query): Query<PublicAssetQuery>,
) -> Response {
    if core_enforcing(&state) {
        let Some(bridge) = state.core.as_ref() else {
            return scheduler_endpoint_not_enabled_response();
        };
        let digest = assets::content_token_digest(&query.token);
        let asset = match bridge.store.asset_by_content_token(
            &asset_id,
            &digest,
            chrono::Utc::now().timestamp_millis(),
        ) {
            Ok(Some(asset)) => asset,
            Ok(None) => {
                return openai_error(StatusCode::NOT_FOUND, "asset_not_found", "素材不存在、已过期或链接无效")
            }
            Err(error) => return core_error_response(error),
        };
        let bytes = match assets::read_core(
            &state.data_dir,
            &asset.storage_ref,
            asset.size,
            &asset.sha256,
        ) {
            Ok(bytes) => bytes,
            Err(_) => {
                return openai_error(StatusCode::NOT_FOUND, "asset_not_found", "素材不存在、已过期或链接无效")
            }
        };
        let mut builder = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", asset.mime_type)
            .header("content-length", bytes.len());
        for (name, value) in asset_security_headers() {
            builder = builder.header(name, value);
        }
        return builder
            .body(Body::from(bytes))
            .unwrap_or_else(|_| internal_error_response());
    }
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

struct ParsedAssetUpload {
    filename: String,
    declared_mime: Option<String>,
    bytes: Vec<u8>,
}

fn parse_asset_upload(body: &[u8]) -> Result<ParsedAssetUpload, Response> {
    let input: Value = serde_json::from_slice(body).map_err(|error| {
        openai_error(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            &format!("素材上传 JSON 无效: {error}"),
        )
    })?;
    let filename = input
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("upload")
        .to_string();
    if filename.len() > 128 {
        return Err(openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", "filename 过长"));
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
        .filter(|value| !value.is_empty())
        .ok_or_else(|| openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", "缺少 data_base64 字段"))?;
    let encoded = if let Some((header, data)) = encoded.split_once(',') {
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
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        encoded,
    )
    .map_err(|error| openai_error(StatusCode::BAD_REQUEST, "invalid_asset", &format!("data_base64 无效: {error}")))?;
    Ok(ParsedAssetUpload {
        filename,
        declared_mime,
        bytes,
    })
}

async fn core_assets_upload(
    state: Arc<ApiSharedState>,
    principal: Principal,
    body: axum::body::Bytes,
) -> Response {
    if require_scope(&principal, "assets:write").is_err() {
        return core_scope_error("assets:write");
    }
    if body.len() > MAX_BODY_BYTES {
        return openai_error(StatusCode::PAYLOAD_TOO_LARGE, "request_too_large", "request body exceeds 8MB limit");
    }
    let parsed = match parse_asset_upload(&body) {
        Ok(parsed) => parsed,
        Err(response) => return response,
    };
    let request_permit = match state.acquire_limit(
        &principal.key_id,
        LimitKind::AssetUpload,
        parsed.bytes.len() as u64,
        &KeyLimits::default(),
    ) {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    let _guard = state.inflight_guard_with_permit(request_permit);
    let (record, storage_ref) = match assets::write_core_asset(
        &state.data_dir,
        &principal.user_id,
        &parsed.filename,
        parsed.declared_mime.as_deref(),
        &parsed.bytes,
    ) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_asset", &error),
    };
    let created_at_ms = match i64::try_from(record.created_at.saturating_mul(1_000)) {
        Ok(value) => value,
        Err(_) => {
            let _ = assets::remove_core_asset(&state.data_dir, &storage_ref);
            return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", "素材时间戳无效");
        }
    };
    let expires_at_ms = match i64::try_from(record.expires_at.saturating_mul(1_000)) {
        Ok(value) => value,
        Err(_) => {
            let _ = assets::remove_core_asset(&state.data_dir, &storage_ref);
            return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "core_error", "素材过期时间无效");
        }
    };
    let storage_ref_for_cleanup = storage_ref.clone();
    let input = aiwork_core::CreateAssetInput {
        id: record.id.clone(),
        filename: record.filename.clone(),
        mime_type: record.mime_type.clone(),
        extension: record.extension.clone(),
        size: record.size as i64,
        sha256: record.sha256.clone(),
        storage_ref,
        content_token_digest: assets::content_token_digest(&record.public_token),
        created_at_ms,
        expires_at_ms,
    };
    let core_asset = match state
        .core
        .as_ref()
        .expect("Core asset route requires a Core bridge")
        .store
        .create_asset(&principal, input)
    {
        Ok(asset) => asset,
        Err(error) => {
            let _ = assets::remove_core_asset(&state.data_dir, &storage_ref_for_cleanup);
            return core_error_response(error);
        }
    };
    let mut response = json!({
        "object": "asset",
        "id": core_asset.id,
        "filename": core_asset.filename,
        "mime_type": core_asset.mime_type,
        "bytes": core_asset.size,
        "sha256": core_asset.sha256,
        "created_at": core_asset.created_at_ms / 1_000,
        "expires_at": core_asset.expires_at_ms / 1_000,
    });
    if let Ok(url) = assets::public_content_url(&state.data_dir, &record.id, &record.public_token) {
        response["content_url"] = json!(url);
    }
    Json(response).into_response()
}

fn core_video_status(state: JobState) -> &'static str {
    match state {
        JobState::Created => "created",
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::CancelRequested => "cancel_requested",
        JobState::Canceled => "canceled",
        JobState::Succeeded => "completed",
        JobState::Failed => "failed",
        JobState::Unknown => "unknown",
    }
}

fn core_video_projection(job: &CoreJob, attempt: Option<&CoreJobAttempt>, data_dir: &std::path::Path) -> Value {
    let mut payload = json!({
        "id": job.id,
        "object": "video",
        "model": job.model,
        "status": core_video_status(job.state),
        "created_at": job.created_at_ms / 1_000,
        "updated_at": job.updated_at_ms / 1_000,
    });
    if let Some(attempt) = attempt {
        payload["attempt"] = json!(attempt.attempt_no);
    }
    if let Some(error_code) = job.error_code.as_deref() {
        payload["error"] = json!({ "code": error_code });
    }
    if job.reconcile_required {
        payload["reconcile_required"] = json!(true);
    }
    let expected_ref = format!("video-store:{}", job.id);
    if job.artifact_ref.as_deref() == Some(expected_ref.as_str())
        && super::video_store::artifact_path(data_dir, &job.id)
            .map(|path| path.is_file())
            .unwrap_or(false)
    {
        if let Ok(content_url) = super::video_store::content_url(&job.id) {
            payload["content_url"] = json!(content_url);
        }
    }
    payload
}

fn core_video_bundle(
    bridge: &super::CoreBridge,
    principal: &Principal,
    job_id: &str,
) -> Result<Option<(CoreJob, Option<CoreJobAttempt>)>, CoreError> {
    let Some(job) = bridge.store.video_job_for_user(principal, job_id)? else {
        return Ok(None);
    };
    let attempt = bridge.store.video_job_attempt_for_user(principal, job_id)?;
    Ok(Some((job, attempt)))
}

fn core_video_response(
    bridge: &super::CoreBridge,
    principal: &Principal,
    job_id: &str,
    request_id: Option<&str>,
    status: StatusCode,
    wrapped: bool,
    replay: bool,
    data_dir: &std::path::Path,
) -> Response {
    let bundle = match core_video_bundle(bridge, principal, job_id) {
        Ok(Some(bundle)) => bundle,
        Ok(None) => return openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found"),
        Err(error) => return core_error_response(error),
    };
    let task = core_video_projection(&bundle.0, bundle.1.as_ref(), data_dir);
    let body = if wrapped {
        json!({ "task": task, "idempotent_replay": replay })
    } else {
        task
    };
    let mut response = Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap_or_else(|_| internal_error_response());
    if let Some(request_id) = request_id {
        response = with_core_request_id(response, request_id);
    }
    response
}

fn release_core_video_permit_if_terminal(
    bridge: &super::CoreBridge,
    principal: &Principal,
    job_id: &str,
) {
    let Ok(Some(job)) = bridge.store.video_job_for_user(principal, job_id) else {
        return;
    };
    if matches!(job.state, JobState::Succeeded | JobState::Failed | JobState::Canceled) {
        video::release_job_permit(job_id);
    }
}

async fn core_videos_generations(
    state: Arc<ApiSharedState>,
    principal: Principal,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    if require_scope(&principal, "videos:submit").is_err() {
        return core_scope_error("videos:submit");
    }
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
    let (model, _) = match video::validate_request(&input) {
        Ok(value) => value,
        Err(error) => return openai_error(StatusCode::BAD_REQUEST, "invalid_request_error", &error),
    };
    let idempotency_key = headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if idempotency_key.is_none() {
        return openai_error(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key is required in Core enforce mode",
        );
    }
    let bridge = match state.core.as_ref() {
        Some(bridge) => bridge.clone(),
        None => return scheduler_endpoint_not_enabled_response(),
    };
    // This readiness check deliberately precedes Core preflight. A missing
    // adapter must not create a request, quota hold, lease, or job row.
    let executor = match bridge.video_executor() {
        Ok(executor) => executor,
        Err(error) => return core_lease_error_response(error),
    };
    let _request_guard = match state.core_request_guard(&principal.key_id) {
        Ok(guard) => guard,
        Err(error) => return limit_error_response(error),
    };
    let video_job_permit = match state.acquire_video_job(&principal.key_id, &KeyLimits::default()) {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    let job_id = format!(
        "video-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u64>()
    );
    // Core stores only this digest.  The encrypted payload is written first so
    // a crash between persistence and enqueue cannot leave a runnable Core job
    // without the adapter input it needs after restart.
    let sanitized_input = super::payload::sanitize_scheduler_chat_body(&input);
    let input_hash = aiwork_core::canonical_json_hash(&sanitized_input);
    if let Err(error) = state.video_payloads.put(
        &job_id,
        &principal.user_id,
        &input_hash,
        &input,
    ) {
        return openai_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "video_payload_unavailable",
            &error,
        );
    }
    let enqueued = match bridge.enqueue_video_job(
        &principal,
        &principal.key_id,
        idempotency_key,
        &input,
        job_id.clone(),
    ) {
        Ok(result) => result,
        Err(error) => {
            let _ = state.video_payloads.remove(&job_id);
            return core_lease_error_response(error);
        }
    };
    let job = match enqueued {
        VideoJobEnqueueResult::Replay { job, .. } => {
            drop(video_job_permit);
            let _ = state.video_payloads.remove(&job_id);
            return core_video_response(
                &bridge,
                &principal,
                &job.id,
                Some(&job.request_id),
                StatusCode::ACCEPTED,
                true,
                true,
                &state.data_dir,
            )
        }
        VideoJobEnqueueResult::Created { job, .. } => {
            if !video::retain_job_permit(&job.id, video_job_permit) {
                return openai_error(
                    StatusCode::CONFLICT,
                    "video_limit_state_conflict",
                    "video job permit already exists",
                );
            }
            job
        }
    };

    let (job, lease) = match bridge.claim_video_job_for_worker("http-video-worker", &job.id) {
        Ok(Some(claim)) => claim,
        Ok(None) => {
            return core_video_response(
                &bridge,
                &principal,
                &job.id,
                Some(&job.request_id),
                StatusCode::ACCEPTED,
                true,
                false,
                &state.data_dir,
            )
        }
        Err(error) => return core_lease_error_response(error),
    };
    let heartbeat_failed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop_heartbeat = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heartbeat_handle = spawn_core_video_heartbeat(
        bridge.clone(),
        "http-video-worker",
        job.id.clone(),
        heartbeat_failed.clone(),
        stop_heartbeat.clone(),
    );
    let request = VideoExecutionRequest {
        job_id: job.id.clone(),
        request_id: job.request_id.clone(),
        model,
        body: input,
    };
    let lease_for_adapter = lease.clone();
    let mut outcome = match tokio::task::spawn_blocking(move || executor.submit_video(&lease_for_adapter, request)).await {
        Ok(outcome) => outcome,
        Err(_) => VideoAdapterOutcome::TransportUnknown {
            reason: "adapter_task_join_failed".into(),
            upstream_request_ref: None,
        },
    };
    stop_heartbeat.store(true, std::sync::atomic::Ordering::Release);
    heartbeat_handle.abort();
    if heartbeat_failed.load(std::sync::atomic::Ordering::Acquire) {
        outcome = VideoAdapterOutcome::TransportUnknown {
            reason: "video_heartbeat_failed".into(),
            upstream_request_ref: video_outcome_request_ref(&outcome),
        };
    }
    match &outcome {
        VideoAdapterOutcome::Accepted { upstream_request_ref } => {
            if let Err(error) = bridge.record_video_job_acceptance(
                &principal,
                &job.id,
                upstream_request_ref,
            ) {
                let _ = bridge.settle_video_job(
                    &principal,
                    &job.id,
                    &lease.lease_id,
                    VideoAdapterOutcome::TransportUnknown {
                        reason: "acceptance_persistence_failed".into(),
                        upstream_request_ref: Some(upstream_request_ref.clone()),
                    },
                );
                release_core_video_permit_if_terminal(&bridge, &principal, &job.id);
                return core_lease_error_response(error);
            }
        }
        VideoAdapterOutcome::Succeeded { .. }
        | VideoAdapterOutcome::Canceled { .. }
        | VideoAdapterOutcome::Rejected { .. }
        | VideoAdapterOutcome::TransportUnknown { .. } => {
            if let Err(error) = bridge.settle_video_job(
                &principal,
                &job.id,
                &lease.lease_id,
                outcome.clone(),
            ) {
                release_core_video_permit_if_terminal(&bridge, &principal, &job.id);
                return core_lease_error_response(error);
            }
            if video_outcome_releases_payload(&outcome) {
                let _ = state.video_payloads.remove(&job.id);
                video::release_job_permit(&job.id);
            }
        }
    }
    if let VideoAdapterOutcome::Rejected {
        status,
        accepted: false,
        code,
    } = &outcome
    {
        let response = openai_error(
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY),
            "upstream_error",
            code,
        );
        return with_core_request_id(response, &job.request_id);
    }
    core_video_response(
        &bridge,
        &principal,
        &job.id,
        Some(&job.request_id),
        StatusCode::ACCEPTED,
        true,
        false,
        &state.data_dir,
    )
}

fn spawn_core_video_heartbeat(
    bridge: Arc<super::CoreBridge>,
    worker_id: &'static str,
    job_id: String,
    heartbeat_failed: Arc<std::sync::atomic::AtomicBool>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        interval.tick().await;
        loop {
            interval.tick().await;
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                break;
            }
            if bridge
                .heartbeat_video_job_for_worker(worker_id, &job_id)
                .is_err()
            {
                heartbeat_failed.store(true, std::sync::atomic::Ordering::Release);
                break;
            }
        }
    })
}

fn video_outcome_request_ref(outcome: &VideoAdapterOutcome) -> Option<String> {
    match outcome {
        VideoAdapterOutcome::Accepted { upstream_request_ref } => Some(upstream_request_ref.clone()),
        VideoAdapterOutcome::Succeeded { upstream_request_ref, .. }
        | VideoAdapterOutcome::Canceled { upstream_request_ref }
        | VideoAdapterOutcome::TransportUnknown { upstream_request_ref, .. } => upstream_request_ref.clone(),
        VideoAdapterOutcome::Rejected { .. } => None,
    }
}

fn video_outcome_releases_payload(outcome: &VideoAdapterOutcome) -> bool {
    matches!(
        outcome,
        VideoAdapterOutcome::Succeeded { .. }
            | VideoAdapterOutcome::Canceled { .. }
            | VideoAdapterOutcome::Rejected {
                accepted: false,
                ..
            }
    )
}

/// W-02 Seedance 文生视频入口。通用或 Work 积分账号异步转发到 Trae Work CN
/// 原生 SSE 接口，客户端通过任务查询接口获取最终资源地址。
pub async fn videos_generations(
    state: State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    videos_generations_with_attribution(state, key_id, resolved_key, principal, None, headers, body).await
}

pub async fn videos_generations_with_attribution(
    State(state): State<Arc<ApiSharedState>>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    core_attribution: Option<Extension<super::bridge_billing::CoreRequestAttribution>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let core_attribution = core_attribution.map(|Extension(value)| value);
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        return core_videos_generations(state, principal, headers, body).await;
    }
    let key_str = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_VIDEO) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
    let request_permit = match state.acquire_limit(
        &key_str,
        LimitKind::VideoSubmission,
        body.len() as u64,
        &key_limits,
    ) {
        Ok(permit) => permit,
        Err(error) => return limit_error_response(error),
    };
    let guard = state.inflight_guard_with_permit(request_permit);
    let video_job_permit = match state.acquire_video_job(&key_str, &key_limits) {
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
                "没有可用的 Trae Work 账号，或通用积分与 Work 积分均不可用",
            )
        }
    };
    let task = match video::create_pending_with_idempotency(
        model.clone(),
        prompt,
        &key_str,
        scoped_idempotency_key.as_deref(),
    ) {
        video::IdempotentCreateResult::Existing(task) => {
            drop(video_job_permit);
            drop(guard);
            let detail = json!({ "task": task, "request_key": key_str, "idempotent_replay": true });
            return Response::builder()
                .status(StatusCode::ACCEPTED)
                .header("content-type", "application/json")
                .body(Body::from(detail.to_string()))
                .unwrap_or_else(|_| internal_error_response());
        }
        video::IdempotentCreateResult::Created(task) => task,
    };
    video::start_native_task(
        state.clone(),
        task.id.clone(),
        input,
        key_str.clone(),
        core_attribution,
        account,
        video_job_permit,
    );
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
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    Path(task_id): Path<String>,
) -> Response {
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        if require_scope(&principal, "videos:read").is_err() {
            return core_scope_error("videos:read");
        }
        let Some(bridge) = state.core.as_ref() else {
            return scheduler_endpoint_not_enabled_response();
        };
        return core_video_response(
            bridge,
            &principal,
            &task_id,
            None,
            StatusCode::OK,
            false,
            false,
            &state.data_dir,
        );
    }
    let owner_key_id = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = match require_legacy_capability(&owner_key_id, resolved_key.as_ref(), api_keys::CAPABILITY_VIDEO) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    let _guard = match legacy_request_guard(&state, &owner_key_id, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
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
    resolved_key: Option<Extension<ResolvedKey>>,
    principal: Option<Extension<Principal>>,
    Path(task_id): Path<String>,
) -> Response {
    if core_enforcing(&state) {
        let principal = match core_principal_or_unauthorized(principal.as_ref()) {
            Ok(principal) => principal,
            Err(response) => return response,
        };
        if require_scope(&principal, "videos:read").is_err() {
            return core_scope_error("videos:read");
        }
        let Some(bridge) = state.core.as_ref() else {
            return scheduler_endpoint_not_enabled_response();
        };
        let Some(bundle) = (match core_video_bundle(bridge, &principal, &task_id) {
            Ok(bundle) => bundle,
            Err(error) => return core_error_response(error),
        }) else {
            return openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found");
        };
        let expected_ref = format!("video-store:{task_id}");
        if bundle.0.artifact_ref.as_deref() != Some(expected_ref.as_str()) {
            return openai_error(StatusCode::NOT_FOUND, "video_not_found", "video artifact not found");
        }
        let path = match super::video_store::artifact_path(&state.data_dir, &task_id) {
            Ok(path) => path,
            Err(_) => return openai_error(StatusCode::NOT_FOUND, "video_not_found", "video artifact not found"),
        };
        let Some(metadata) = std::fs::metadata(&path).ok().filter(|metadata| metadata.is_file()) else {
            return openai_error(StatusCode::NOT_FOUND, "video_not_found", "video artifact not found");
        };
        let size = metadata.len();
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
        return Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "video/mp4")
            .header("content-length", size)
            .header("cache-control", "private, max-age=3600")
            .body(Body::from_stream(ReceiverStream::new(rx)))
            .unwrap_or_else(|_| internal_error_response());
    }
    let owner_key_id = key_id
        .map(|Extension(k)| k.0)
        .unwrap_or_else(|| "anonymous".to_string());
    let resolved_key = resolved_key.map(|Extension(key)| key);
    let key_limits = match require_legacy_capability(&owner_key_id, resolved_key.as_ref(), api_keys::CAPABILITY_VIDEO) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    let _guard = match legacy_request_guard(&state, &owner_key_id, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
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

/// Request cancellation of a Core video job. A client request only records
/// intent; the quota hold is released only after the adapter confirms cancel.
pub async fn video_cancel(
    State(state): State<Arc<ApiSharedState>>,
    principal: Option<Extension<Principal>>,
    Path(task_id): Path<String>,
) -> Response {
    if !core_enforcing(&state) {
        return scheduler_endpoint_not_enabled_response();
    }
    let principal = match core_principal_or_unauthorized(principal.as_ref()) {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if require_scope(&principal, "videos:cancel").is_err() {
        return core_scope_error("videos:cancel");
    }
    let Some(bridge) = state.core.as_ref() else {
        return scheduler_endpoint_not_enabled_response();
    };
    let job = match bridge.request_video_cancel(&principal, &task_id) {
        Ok(job) => job,
        Err(CoreLeaseError::Core(CoreError::RequestNotFound { .. })) => {
            return openai_error(StatusCode::NOT_FOUND, "task_not_found", "video task not found")
        }
        Err(error) => return core_lease_error_response(error),
    };
    if matches!(
        job.state,
        JobState::Canceled | JobState::Succeeded | JobState::Failed | JobState::Unknown
    ) {
        return core_video_response(
            bridge,
            &principal,
            &task_id,
            None,
            StatusCode::OK,
            false,
            false,
            &state.data_dir,
        );
    }
    let lease = match bridge.video_job_lease(&principal, &task_id) {
        Ok(Some(lease)) => lease,
        Ok(None) => {
            return core_video_response(
                bridge,
                &principal,
                &task_id,
                None,
                StatusCode::ACCEPTED,
                false,
                false,
                &state.data_dir,
            )
        }
        Err(error) => return core_lease_error_response(error),
    };
    let outcome = match bridge.video_executor() {
        Ok(executor) => {
            match executor.grant_for_lease(&lease) {
                Some(grant) => {
                    let outcome = tokio::task::spawn_blocking(move || executor.cancel_video(&grant)).await;
                    match outcome {
                        Ok(outcome) => outcome,
                        Err(_) => VideoCancelOutcome::Unknown {
                            reason: "cancel_adapter_task_join_failed".into(),
                            upstream_request_ref: lease.upstream_request_ref.clone(),
                        },
                    }
                }
                None => VideoCancelOutcome::Unknown {
                    reason: "cancel_lease_binding_missing".into(),
                    upstream_request_ref: lease.upstream_request_ref.clone(),
                },
            }
        }
        Err(_) => VideoCancelOutcome::Unknown {
            reason: "cancel_adapter_unavailable".into(),
            upstream_request_ref: lease.upstream_request_ref.clone(),
        },
    };
    let settlement_outcome = match outcome {
        VideoCancelOutcome::Confirmed {
            upstream_request_ref,
        } => VideoAdapterOutcome::Canceled {
            upstream_request_ref,
        },
        VideoCancelOutcome::Unsupported => VideoAdapterOutcome::TransportUnknown {
            reason: "cancel_unsupported".into(),
            upstream_request_ref: lease.upstream_request_ref.clone(),
        },
        VideoCancelOutcome::Unknown {
            reason,
            upstream_request_ref,
        } => VideoAdapterOutcome::TransportUnknown {
            reason,
            upstream_request_ref,
        },
    };
    let confirmed_cancel = matches!(&settlement_outcome, VideoAdapterOutcome::Canceled { .. });
    if let Err(error) = bridge.settle_video_job(
        &principal,
        &task_id,
        &lease.id,
        settlement_outcome,
    ) {
        return core_lease_error_response(error);
    }
    if confirmed_cancel {
        video::release_job_permit(&task_id);
    }
    core_video_response(
        bridge,
        &principal,
        &task_id,
        None,
        StatusCode::ACCEPTED,
        false,
        false,
        &state.data_dir,
    )
}

async fn images_entry(
    state: Arc<ApiSharedState>,
    key_id: Option<Extension<KeyId>>,
    resolved_key: Option<ResolvedKey>,
    body: axum::body::Bytes,
    is_edit: bool,
) -> Response {
    if core_enforcing(&state) {
        return scheduler_endpoint_not_enabled_response();
    }
    let key_str = key_id.map(|Extension(k)| k.0).unwrap_or_else(|| "anonymous".to_string());
    let key_limits = match require_legacy_capability(&key_str, resolved_key.as_ref(), api_keys::CAPABILITY_CHAT) {
        Ok(limits) => limits,
        Err(response) => return response,
    };
    state
        .total_requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // inflight guard（§4.5）：async fn 全程 inline await，作用域即请求生命周期
    let _guard = match legacy_request_guard(&state, &key_str, &key_limits) {
        Ok(guard) => guard,
        Err(response) => return response,
    };
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
        let key_constraints = resolved_key.as_ref();
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
    stream_chat_with_attribution(state, body_vec, model, stream, start_ts, proto, key_id, guard, conversation_id, None)
}

fn stream_chat_with_attribution(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard, conversation_id: Option<String>, core_attribution: Option<super::bridge_billing::CoreRequestAttribution>) -> Response {
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
        let mut guard = guard;
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
            let converted = match bind_core_usage_session(converted, core_attribution.as_ref(), &picked.uid) {
                Ok(body) => body,
                Err(_) => {
                    let message = "Core 请求无法建立独立的上游用量会话，已阻止未记录的上游调用";
                    send_stream_error(&tx, proto, 503, message);
                    return;
                }
            };
            if super::bridge_billing::BridgeBillingStore::record_core_upstream_attempt_from_payload(
                &state.data_dir,
                core_attribution.as_ref(),
                &picked.uid,
                &serde_json::from_slice::<Value>(&converted).unwrap_or(Value::Null),
            ).is_err() {
                let message = "Core 请求与上游会话归因暂不可用，已阻止未记录的上游调用";
                state.logger.log_request(
                    "trae", "POST", proto.log_path(), &model, stream,
                    503, &picked.uid, start_ts.elapsed().as_millis() as u64, Some(message),
                );
                send_stream_error(&tx, proto, 503, message);
                return;
            }

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
                        // 用量记账（流式结束即落盘）；首字节前失败可换号，
                        // 因而只有已经向客户端发送内容的错误才是本次终态。
                        let (pt, ct) = up_usage.as_ref().map(extract_tokens).unwrap_or((0, 0));
                        let terminal = error_info.is_none() || sent_any;
                        state.record_usage_with_guard(
                            &mut guard,
                            false,
                            &model,
                            &picked.uid,
                            &key_id,
                            error_info.is_none(),
                            true,
                            duration_ms,
                            pt,
                            ct,
                            terminal && error_info.is_none(),
                        );
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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    false,
                                    true,
                                    start_ts.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    false,
                                );
                                return;
                            }
                        }
                    }
                }
            }
        }

        // 所有账号不可用
        let duration_ms = start_ts.elapsed().as_millis() as u64;
        state.record_usage_with_guard(
            &mut guard,
            false,
            &model,
            "none",
            &key_id,
            false,
            true,
            duration_ms,
            0,
            0,
            false,
        );
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
    aggregate_chat_with_attribution(state, body_vec, model, stream, start_ts, proto, key_id, guard, conversation_id, None).await
}

async fn aggregate_chat_with_attribution(state: Arc<ApiSharedState>, body_vec: Vec<u8>, model: String, stream: bool, start_ts: std::time::Instant, proto: Protocol, key_id: String, guard: InflightGuard, conversation_id: Option<String>, core_attribution: Option<super::bridge_billing::CoreRequestAttribution>) -> Response {
    let result = tokio::task::spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）
        let mut guard = guard;
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
            let converted = match bind_core_usage_session(converted, core_attribution.as_ref(), &picked.uid) {
                Ok(body) => body,
                Err(_) => {
                    return Err(AggregateFail::Attribution {
                        message: "Core 请求无法建立独立的上游用量会话，已阻止未记录的上游调用".into(),
                    });
                }
            };
            if super::bridge_billing::BridgeBillingStore::record_core_upstream_attempt_from_payload(
                &state.data_dir,
                core_attribution.as_ref(),
                &picked.uid,
                &serde_json::from_slice::<Value>(&converted).unwrap_or(Value::Null),
            ).is_err() {
                return Err(AggregateFail::Attribution {
                    message: "Core 请求与上游会话归因暂不可用，已阻止未记录的上游调用".into(),
                });
            }

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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    true,
                                    stream,
                                    duration_ms,
                                    pt,
                                    ct,
                                    true,
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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    false,
                                    stream,
                                    duration_ms,
                                    0,
                                    0,
                                    false,
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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    false,
                                    stream,
                                    duration_ms,
                                    0,
                                    0,
                                    false,
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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    false,
                                    stream,
                                    start_ts.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    false,
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
                                state.record_usage_with_guard(
                                    &mut guard,
                                    false,
                                    &model,
                                    &picked.uid,
                                    &key_id,
                                    false,
                                    stream,
                                    start_ts.elapsed().as_millis() as u64,
                                    0,
                                    0,
                                    false,
                                );
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
        state.record_usage_with_guard(
            &mut guard,
            false,
            &model,
            "none",
            &key_id,
            false,
            stream,
            duration_ms,
            0,
            0,
            false,
        );
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
        Ok(Err(AggregateFail::Attribution { message })) => match proto {
            Protocol::Anthropic => anthropic_error(StatusCode::SERVICE_UNAVAILABLE, "api_error", &message),
            _ => openai_error(StatusCode::SERVICE_UNAVAILABLE, "core_attribution_unavailable", &message),
        },
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

pub(crate) fn make_upstream_request(
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

pub(super) fn limit_error_response(error: LimitError) -> Response {
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
    use aiwork_core::{CoreStore, CostPolicy, KeyQuotaGrant, NewUser, Principal, UserRole};
    use axum::body::Bytes;
    use axum::extract::State;
    use axum::http::HeaderMap;

    use crate::api_server::{CoreBridge, CoreMode};

    #[test]
    fn core_session_binding_separates_requests_without_changing_conversation_context() {
        let body = serde_json::to_vec(&json!({
            "conversation_id": "shared-conversation",
            "session_id": "conversation-session",
            "messages": [{"role":"user","content":"hello"}]
        })).unwrap();
        let first = super::bind_core_usage_session(
            body.clone(),
            Some(&super::super::bridge_billing::CoreRequestAttribution {
                request_id: "core-req-1".into(),
                core_key_id: "key-1".into(),
                one_shot_test: false,
            }),
            "uid-a",
        ).unwrap();
        let first_retry = super::bind_core_usage_session(
            body.clone(),
            Some(&super::super::bridge_billing::CoreRequestAttribution {
                request_id: "core-req-1".into(),
                core_key_id: "key-1".into(),
                one_shot_test: false,
            }),
            "uid-a",
        ).unwrap();
        let second = super::bind_core_usage_session(
            body.clone(),
            Some(&super::super::bridge_billing::CoreRequestAttribution {
                request_id: "core-req-2".into(),
                core_key_id: "key-2".into(),
                one_shot_test: false,
            }),
            "uid-a",
        ).unwrap();
        let ordinary = super::bind_core_usage_session(body.clone(), None, "uid-a").unwrap();
        let first: Value = serde_json::from_slice(&first).unwrap();
        let first_retry: Value = serde_json::from_slice(&first_retry).unwrap();
        let second: Value = serde_json::from_slice(&second).unwrap();
        let ordinary: Value = serde_json::from_slice(&ordinary).unwrap();

        assert_eq!(first["session_id"], first_retry["session_id"]);
        assert_ne!(first["session_id"], second["session_id"]);
        assert_eq!(first["conversation_id"], "shared-conversation");
        assert_eq!(first["messages"], body_value(&body)["messages"]);
        assert_eq!(ordinary["session_id"], "conversation-session");
    }

    fn body_value(body: &[u8]) -> Value {
        serde_json::from_slice(body).unwrap()
    }

    struct CoreFixture {
        dir: PathBuf,
        state: Arc<ApiSharedState>,
        principal: Principal,
        admin: Principal,
    }

    impl Drop for CoreFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn ensure_zero_key_budget(dir: &std::path::Path, user_id: &str, api_key_id: &str, resource_kind: &str) {
        let connection = rusqlite::Connection::open(
            dir.join("data").join(aiwork_core::CORE_DB_FILE),
        )
        .unwrap();
        connection
            .execute(
                "INSERT INTO quota_budget_accounts
                 (id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state, created_at_ms, updated_at_ms)
                 VALUES (?1, 'key', ?2, ?3, ?4, 1, 1, 'ready', 0, 0)",
                rusqlite::params![
                    format!("budget-zero-{}", rand::random::<u64>()),
                    user_id,
                    api_key_id,
                    resource_kind,
                ],
            )
            .unwrap();
    }

    fn core_fixture(grant: i64, scopes: &[&str]) -> CoreFixture {
        let root = PathBuf::from(r"D:\gpt");
        fs::create_dir_all(&root).unwrap();
        let dir = root.join(format!(
            "aiwork-routes-core-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_bootstrap_admin(
                NewUser {
                    id: "admin".into(),
                    name: "Admin".into(),
                    role: UserRole::Admin,
                },
                "bootstrap",
            )
            .unwrap();
        let admin_key = store
            .issue_api_key("admin", "admin", BTreeSet::new(), "bootstrap")
            .unwrap();
        let admin = Principal {
            user_id: "admin".into(),
            key_id: admin_key.id,
            scopes: BTreeSet::new(),
        };
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
                .key_quota_grant_as_admin(&admin, KeyQuotaGrant {
                    api_key_id: key.id.clone(),
                    resource_kind: "chat_request".into(),
                    amount: grant,
                    actor_user_id: "admin".into(),
                    reason: "route test grant".into(),
                })
                .unwrap();
        } else {
            ensure_zero_key_budget(&dir, "route-user", &key.id, "chat_request");
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
            video_payloads: super::super::video_payload::VideoPayloadStore::new(&dir),
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
        CoreFixture { dir, state, principal, admin }
    }

    fn key_balance(fixture: &CoreFixture, resource_kind: &str) -> aiwork_core::QuotaBudgetBalance {
        fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .store
            .key_quota_balance_as_admin(
                &fixture.admin,
                &fixture.principal.key_id,
                resource_kind,
            )
            .unwrap()
    }

    fn chat_body(stream: bool) -> Bytes {
        chat_body_for_model("mock-1", stream)
    }

    fn phase2_fixture(grant: i64) -> CoreFixture {
        let mut fixture = core_fixture(grant, &["chat:invoke"]);
        let store = fixture.state.core.as_ref().unwrap().store.clone();
        let mut account = aiwork_core::RegisterUpstreamAccount::new("mock-account".into(), "mock".into(), "vault://mock/account".into());
        account.capabilities.insert("chat".into());
        store.upsert_upstream_account(account, &fixture.admin).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        store.append_upstream_observation(aiwork_core::UpstreamObservation::new(
            "mock-observation".into(), "mock-account".into(), "chat_request".into(), Some(100), 1,
            "reader".into(), aiwork_core::ObservationStatus::Fresh, now, now + 600_000, json!({}),
        )).unwrap();
        let runtime = super::super::scheduler::SchedulerRuntime::new(store.clone(), fixture.dir.clone(),
            super::super::scheduler::SchedulerMode::Enforce, Default::default(), Default::default(), now).unwrap();
        Arc::get_mut(&mut fixture.state).unwrap().core = Some(Arc::new(CoreBridge::new(store, CoreMode::Enforce)
            .with_scheduler(Arc::new(runtime)).unwrap()));
        fixture
    }

    fn phase3_video_fixture(
        grant: i64,
        submit_outcomes: Vec<VideoAdapterOutcome>,
        cancel_outcomes: Vec<VideoCancelOutcome>,
    ) -> (CoreFixture, Arc<super::super::core_video::MockVideoAdapter>) {
        let mut fixture = core_fixture(
            0,
            &["videos:submit", "videos:read", "videos:cancel"],
        );
        let store = fixture.state.core.as_ref().unwrap().store.clone();
        store
            .upsert_cost_policy(CostPolicy {
                id: "route-video-policy".into(),
                endpoint: "videos".into(),
                model_pattern: "mock-video".into(),
                resource_kind: "video_job".into(),
                reserve_amount: 1,
                max_actual_amount: Some(1),
                version: 1,
                enabled: true,
            })
            .unwrap();
        if grant > 0 {
            store
                .key_quota_grant_as_admin(&fixture.admin, KeyQuotaGrant {
                    api_key_id: fixture.principal.key_id.clone(),
                    resource_kind: "video_job".into(),
                    amount: grant,
                    actor_user_id: "admin".into(),
                    reason: "video route test grant".into(),
                })
                .unwrap();
        } else {
            ensure_zero_key_budget(&fixture.dir, "route-user", &fixture.principal.key_id, "video_job");
        }
        let mut account = aiwork_core::RegisterUpstreamAccount::new(
            "mock-video-account".into(),
            "mock-video".into(),
            "vault://mock/video".into(),
        );
        account.capabilities.insert("video".into());
        store.upsert_upstream_account(account, &fixture.admin).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        store
            .append_upstream_observation(aiwork_core::UpstreamObservation::new(
                "mock-video-observation".into(),
                "mock-video-account".into(),
                "video_job".into(),
                Some(100),
                1,
                "reader".into(),
                aiwork_core::ObservationStatus::Fresh,
                now,
                now + 600_000,
                json!({}),
            ))
            .unwrap();
        let runtime = super::super::scheduler::SchedulerRuntime::new(
            store.clone(),
            fixture.dir.clone(),
            super::super::scheduler::SchedulerMode::Enforce,
            Default::default(),
            Default::default(),
            now,
        )
        .unwrap();
        let adapter = Arc::new(super::super::core_video::MockVideoAdapter::new(
            submit_outcomes,
            cancel_outcomes,
        ));
        let executor = super::super::core_video::CoreVideoExecutor::new()
            .with_provider("mock-video", adapter.clone())
            .for_account(
                "mock-video-account",
                "mock-video",
                "vault://mock/video",
            );
        Arc::get_mut(&mut fixture.state).unwrap().core = Some(Arc::new(
            CoreBridge::new(store, CoreMode::Enforce)
                .with_video_executor(executor)
                .with_scheduler(Arc::new(runtime))
                .unwrap(),
        ));
        (fixture, adapter)
    }

    fn legacy_fixture_with_capabilities(
        capabilities: &[&str],
    ) -> (Arc<ApiSharedState>, PathBuf, String) {
        let root = PathBuf::from(r"D:\gpt");
        fs::create_dir_all(&root).unwrap();
        let dir = root.join(format!(
            "aiwork-routes-legacy-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        let key_id = "legacy-route-key".to_string();
        let key = super::super::api_keys::ApiKeyEntry {
            id: key_id.clone(),
            name: "route test".into(),
            key: format!("fixture-route-{}", rand::random::<u64>()),
            enabled: true,
            daily_limit: 0,
            created_at: 0,
            used_date: String::new(),
            used_today: 0,
            allowed_accounts: Vec::new(),
            schedule_mode: String::new(),
            dedicated_account: String::new(),
            daily_stats: Vec::new(),
            token_reservations: Vec::new(),
            limits: super::super::api_keys::KeyLimits::default(),
            capabilities: capabilities.iter().map(|value| (*value).to_string()).collect(),
        };
        super::super::api_keys::save(
            &dir,
            &super::super::api_keys::ApiKeysFile {
                keys: vec![key],
                auth_disabled: false,
            },
        );
        let state = Arc::new(ApiSharedState {
            core: None,
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
            video_payloads: super::super::video_payload::VideoPayloadStore::new(&dir),
            cors_origins: String::new(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limiter: super::super::limits::RateLimiter::with_config(
                super::super::limits::LimitConfig {
                    max_inflight: 8,
                    max_video_jobs: 8,
                    asset_uploads_per_minute: 8,
                    asset_bytes_per_hour: 1024 * 1024,
                    video_submissions_per_minute: 8,
                },
            ),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            logger: super::super::ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        });
        (state, dir, key_id)
    }

    fn legacy_snapshot(
        state: &ApiSharedState,
        key_id: &str,
    ) -> Option<Extension<ResolvedKey>> {
        Some(Extension(
            api_keys::constraints_for(&state.data_dir, key_id)
                .expect("legacy fixture must provide an auth snapshot"),
        ))
    }

    #[tokio::test]
    async fn video_without_usable_account_explains_both_credit_types() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_VIDEO]);
        let response = videos_generations(
            State(state.clone()),
            Some(Extension(KeyId(key_id.clone()))),
            legacy_snapshot(&state, &key_id),
            None,
            HeaderMap::new(),
            Bytes::from(json!({"model": "seedance", "prompt": "hello"}).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "no_work_credits");
        let message = body["error"]["message"].as_str().unwrap();
        assert!(message.contains("通用积分"), "{message}");
        assert!(message.contains("Work 积分"), "{message}");
        fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn disabled_capability_returns_403_without_consuming_usage() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[]);
        let body = Bytes::from(
            json!({
                "model": "mock-1",
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        );

        let chat = chat_completions(
            State(state.clone()),
            Some(Extension(KeyId(key_id.clone()))),
            legacy_snapshot(&state, &key_id),
            None,
            HeaderMap::new(),
            body.clone(),
        )
        .await;
        let chat_status = chat.status();
        let chat_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(chat.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(chat_status, StatusCode::FORBIDDEN);
        assert_eq!(chat_body["error"]["code"], "capability_not_allowed");

        let video = videos_generations(
            State(state.clone()),
            Some(Extension(KeyId(key_id.clone()))),
            legacy_snapshot(&state, &key_id),
            None,
            HeaderMap::new(),
            Bytes::from(json!({"model": "seedance", "prompt": "hello"}).to_string()),
        )
        .await;
        let video_status = video.status();
        let video_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(video.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(video_status, StatusCode::FORBIDDEN);
        assert_eq!(video_body["error"]["code"], "capability_not_allowed");

        let assets = assets_upload(
            State(state.clone()),
            Some(Extension(KeyId(key_id.clone()))),
            legacy_snapshot(&state, &key_id),
            None,
            Bytes::from("{}"),
        )
        .await;
        let assets_status = assets.status();
        let assets_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(assets.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(assets_status, StatusCode::FORBIDDEN);
        assert_eq!(assets_body["error"]["code"], "capability_not_allowed");

        assert_eq!(
            state
                .total_requests
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let usage = state.usage.lock().unwrap();
        assert!(usage.days.is_empty());
        assert!(usage.wb_days.is_empty());
        assert!(usage.custom_days.is_empty());
        drop(usage);
        let permit = state
            .acquire_request(&key_id, &super::super::api_keys::KeyLimits::default())
            .expect("capability rejection must not consume request permit");
        drop(permit);
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn core_enforce_chat_uses_core_lease_and_mock_executor_once() {
        let fixture = phase2_fixture(2);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase2-success".parse().unwrap());
        for _ in 0..2 {
            let response = chat_completions(State(fixture.state.clone()),
                Some(Extension(KeyId(fixture.principal.key_id.clone()))),
                None,
                Some(Extension(fixture.principal.clone())), headers.clone(), chat_body(false)).await;
            assert_eq!(response.status(), StatusCode::OK);
        }
        clear_core_test_executor();
        assert_eq!(executor.calls().len(), 1);
        let db = rusqlite::Connection::open_with_flags(fixture.dir.join("data/core.sqlite3"), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let (count, state, account): (i64, String, String) = db.query_row(
            "SELECT count(*), state, account_ref FROM upstream_leases", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!((count, state.as_str(), account.as_str()), (1, "succeeded", "mock-account"));
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!((balance.available, balance.held), (1, 0));
    }

    #[tokio::test]
    async fn core_replay_conflict_is_stable_and_does_not_dispatch_again() {
        let fixture = phase2_fixture(2);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase2-conflict".parse().unwrap());
        let first = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers.clone(),
            chat_body(false),
        )
        .await;
        let changed = Bytes::from(
            json!({
                "model": "mock-1",
                "messages": [{"role": "user", "content": "changed"}],
                "stream": false,
            })
            .to_string(),
        );
        let conflict = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            changed,
        )
        .await;
        clear_core_test_executor();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(conflict.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "idempotency_conflict");
        assert_eq!(executor.calls().len(), 1);
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!((balance.available, balance.held), (1, 0));
    }

    #[tokio::test]
    async fn core_request_body_account_and_user_fields_cannot_override_lease() {
        let fixture = phase2_fixture(1);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase2-identity".parse().unwrap());
        let body = json!({"model":"mock-1", "messages":[], "user_id":"victim", "account_ref":"attacker",
            "credentials_ref":"vault://attacker", "provider":"attacker", "allowed_accounts":["attacker"], "dedicated_account":"attacker"});
        let response = chat_completions(State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())), headers, Bytes::from(body.to_string())).await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::OK);
        let calls = executor.calls();
        assert_eq!(calls.len(), 1);
        for field in ["user_id", "account_ref", "credentials_ref", "provider", "allowed_accounts", "dedicated_account"] {
            assert!(calls[0].body.get(field).is_none(), "untrusted field passed to adapter: {field}");
        }
    }

    #[tokio::test]
    async fn core_no_fresh_observation_never_dispatches_or_holds_user_quota() {
        let fixture = phase2_fixture(1);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let store = &fixture.state.core.as_ref().unwrap().store;
        let now = chrono::Utc::now().timestamp_millis() + 1;
        store.append_upstream_observation(aiwork_core::UpstreamObservation::new(
            "stale-observation".into(), "mock-account".into(), "chat_request".into(), Some(100), 1,
            "json_cache".into(), aiwork_core::ObservationStatus::Stale, now, now, json!({}),
        )).unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "stale".parse().unwrap());
        let response = chat_completions(State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())), headers, chat_body(false)).await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let payload: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(payload["error"]["code"], "no_fresh_observation");
        assert_eq!(executor.calls().len(), 0);
        assert_eq!(key_balance(&fixture, "chat_request").held, 0);
        assert!(store.list_recoverable_leases().unwrap().is_empty());
    }

    #[tokio::test]
    async fn core_no_upstream_capacity_never_dispatches_or_holds_user_quota() {
        let fixture = phase2_fixture(1);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let store = &fixture.state.core.as_ref().unwrap().store;
        let now = chrono::Utc::now().timestamp_millis();
        store
            .append_upstream_observation(aiwork_core::UpstreamObservation::new(
                "no-capacity-observation".into(),
                "mock-account".into(),
                "chat_request".into(),
                Some(0),
                1,
                "reader".into(),
                aiwork_core::ObservationStatus::Fresh,
                now,
                now + 600_000,
                json!({}),
            ))
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "no-capacity".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "no_upstream_capacity");
        assert!(executor.calls().is_empty());
        assert_eq!(key_balance(&fixture, "chat_request").held, 0);
        assert!(store.list_recoverable_leases().unwrap().is_empty());
    }

    fn chat_body_for_model(model: &str, stream: bool) -> Bytes {
        Bytes::from(json!({
            "model": model,
            "messages": [{"role": "user", "content": "hello"}],
            "stream": stream,
        }).to_string())
    }

    #[tokio::test]
    async fn core_enforce_chat_does_not_fallback_to_legacy_pool_when_scheduler_rejects() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "no-runtime".parse().unwrap());
        let response = chat_completions(State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())), headers, chat_body(false)).await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        assert_eq!(executor.calls().len(), 0);
        assert_eq!(key_balance(&fixture, "chat_request").held, 0);
    }

    #[tokio::test]
    async fn core_missing_registered_executor_rejects_before_lease_acquire() {
        let fixture = phase2_fixture(1);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "missing-binding".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(key_balance(&fixture, "chat_request").held, 0);
        assert!(store.list_recoverable_leases().unwrap().is_empty());
    }

    #[tokio::test]
    async fn core_transport_unknown_settles_lease_without_held_or_active_residue() {
        let fixture = phase2_fixture(1);
        let executor = Arc::new(
            super::super::core_bridge::core_executor::MockUpstreamExecutor::with_outcome(
                UpstreamOutcome::TransportUnknown {
                    reason: "transport_timeout".into(),
                    upstream_request_ref: None,
                },
            ),
        );
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "transport-unknown".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(executor.calls().len(), 1);
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(key_balance(&fixture, "chat_request").held, 1);
        let leases = store.list_recoverable_leases().unwrap();
        assert_eq!(leases.len(), 1);
        assert!(leases
            .iter()
            .all(|lease| lease.state == aiwork_core::LeaseState::Unknown));
    }

    #[test]
    fn repeated_core_lease_settlement_does_not_repeat_health_or_audit() {
        let fixture = phase2_fixture(1);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let preflight = bridge
            .preflight_chat_with_lease_for_accounts(
                &fixture.principal,
                &fixture.principal.key_id,
                Some("duplicate-health"),
                &body,
                &["mock-account".into()],
            )
            .unwrap();
        let lease = preflight.lease.clone().unwrap();
        let context = CoreChatContext {
            bridge: bridge.clone(),
            principal: fixture.principal.clone(),
            request_id: preflight.request_id,
            reservation_id: preflight.reservation.unwrap().id,
            lease: Some(lease),
            reservation_amount: 1,
        };
        let outcome = UpstreamOutcome::Rejected {
            status: 429,
            code: "SoftRate".into(),
            accepted: false,
        };
        let _ = settle_core_lease_outcome(&context, Response::new(Body::empty()), outcome.clone());
        let store = &context.bridge.store;
        let after_first = store.count_rows("audit_events").unwrap();
        let db = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        let errors_after_first: i64 = db
            .query_row(
                "SELECT consecutive_errors FROM upstream_accounts WHERE id = 'mock-account'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let _ = settle_core_lease_outcome(&context, Response::new(Body::empty()), outcome);
        assert_eq!(store.count_rows("audit_events").unwrap(), after_first);
        let errors_after_replay: i64 = db
            .query_row(
                "SELECT consecutive_errors FROM upstream_accounts WHERE id = 'mock-account'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(errors_after_replay, errors_after_first);
    }

    #[tokio::test]
    async fn core_unintegrated_stream_or_protocol_returns_scheduler_endpoint_not_enabled() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let mut stream_headers = HeaderMap::new();
        stream_headers.insert("idempotency-key", "legacy-stream-test".parse().unwrap());
        let response = chat_completions(State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())), stream_headers, chat_body(true)).await;
        let payload: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let response = responses_api(State(fixture.state.clone()), None, None, None, HeaderMap::new(), Bytes::from("{}")).await;
        let payload: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let response = messages(
            State(fixture.state.clone()),
            None,
            None,
            None,
            HeaderMap::new(),
            Bytes::from("{}"),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let response = completions(State(fixture.state.clone()), None, None, Bytes::from("{}")).await;
        let payload: Value = serde_json::from_slice(&axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
    }

    #[tokio::test]
    async fn core_asset_route_requires_scope_while_video_routes_remain_fail_closed() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let response = assets_upload(
            State(fixture.state.clone()),
            None,
            None,
            Some(Extension(fixture.principal.clone())),
            Bytes::from("{}"),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "insufficient_scope");

        let response = assets_content(
            State(fixture.state.clone()),
            Path("unintegrated-asset".into()),
            Query(PublicAssetQuery { token: "token".into() }),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "asset_not_found");

        let response = videos_generations(
            State(fixture.state.clone()),
            None,
            None,
            Some(Extension(fixture.principal.clone())),
            HeaderMap::new(),
            Bytes::from("{}"),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "insufficient_scope");

        let response = video_task(
            State(fixture.state.clone()),
            None,
            None,
            Some(Extension(fixture.principal.clone())),
            Path("unintegrated-video".into()),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "insufficient_scope");

        let response = video_content(
            State(fixture.state.clone()),
            None,
            None,
            Some(Extension(fixture.principal.clone())),
            Path("unintegrated-video".into()),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "insufficient_scope");
    }

    #[tokio::test]
    async fn core_asset_upload_persists_user_owner_and_token_protected_content() {
        use base64::Engine as _;

        let fixture = core_fixture(0, &["assets:write", "assets:read"]);
        super::super::gateway_settings::save(
            &fixture.dir,
            super::super::gateway_settings::GatewaySettings {
                port: 7864,
                default_model: "mock-1".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: "https://assets.example.test/v1".into(),
                core_mode: "enforce".into(),
                scheduler_mode: "enforce".into(),
                limit_defaults: Default::default(),
                updated_at: 0,
            },
        )
        .unwrap();
        let png = b"\x89PNG\r\n\x1a\ncore-route-asset";
        let body = json!({
            "filename": "..\\private.png",
            "mime_type": "image/png",
            "data_base64": base64::engine::general_purpose::STANDARD.encode(png),
            "user_id": "attacker-controlled-value"
        });
        let response = assets_upload(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            Bytes::from(body.to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        let asset_id = payload["id"].as_str().unwrap().to_owned();
        let content_url = payload["content_url"].as_str().unwrap();
        let token = content_url.rsplit_once("token=").unwrap().1.to_owned();
        let db = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        let (owner, storage_ref): (String, String) = db
            .query_row(
                "SELECT user_id, storage_ref FROM assets WHERE id = ?1",
                [&asset_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(owner, "route-user");
        assert_eq!(storage_ref, format!("assets/{asset_id}.png"));
        assert!(!fixture.dir.join("data/assets.json").exists());

        let response = assets_content(
            State(fixture.state.clone()),
            Path(asset_id.clone()),
            Query(PublicAssetQuery { token }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .as_ref(),
            png.as_slice()
        );

        let response = assets_content(
            State(fixture.state.clone()),
            Path(asset_id),
            Query(PublicAssetQuery { token: "wrong".into() }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn core_health_category_normalizes_credit_and_rate_error_labels() {
        for (code, expected) in [
            ("HardCredit", "hard_credit"),
            ("hard_credit", "hard_credit"),
            ("hard-credit", "hard_credit"),
            ("PlanLimit", "plan_limit"),
            ("plan_limit", "plan_limit"),
            ("SoftRate", "soft_rate"),
            ("soft_rate", "soft_rate"),
        ] {
            let outcome = UpstreamOutcome::Rejected {
                status: 400,
                code: code.into(),
                accepted: false,
            };
            assert_eq!(core_health_category(&outcome), expected, "label {code}");
        }
    }

    #[tokio::test]
    async fn core_images_routes_return_scheduler_endpoint_not_enabled() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let response = images_generations(
            State(fixture.state.clone()),
            None,
            None,
            Bytes::from("{}"),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");

        let response = images_edits(
            State(fixture.state.clone()),
            None,
            None,
            Bytes::from("{}"),
        )
        .await;
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
    }

    #[tokio::test]
    async fn core_models_keep_existing_catalog_shape_with_scope() {
        let fixture = core_fixture(1, &["models:read", "chat:invoke"]);
        let response = models(
            State(fixture.state.clone()),
            Some(Extension(fixture.principal.clone())),
        ).await;
        assert_eq!(response.status(), StatusCode::OK);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let seedance = payload["data"]
            .as_array()
            .and_then(|models| models.iter().find(|model| model["id"] == "seedance"))
            .expect("/v1/models should expose seedance");
        assert_eq!(seedance["capabilities"], serde_json::json!(["video"]));
        assert_eq!(seedance["endpoint"], "/v1/videos/generations");
        assert_eq!(seedance["async"], true);
        assert_eq!(seedance["sources"][0]["enabled"], false);
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
    async fn core_chat_stream_without_registered_adapter_returns_501() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase1-stream-test".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        ).await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let balance = key_balance(&fixture, "chat_request");
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
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn core_chat_reports_insufficient_quota_before_dispatch() {
        let fixture = phase2_fixture(0);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "quota-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn core_chat_key_quota_rejects_before_mock_dispatch() {
        let fixture = phase2_fixture(1);
        let connection = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
            connection
            .execute(
                "DELETE FROM quota_ledger WHERE budget_account_id IN (
                    SELECT id FROM quota_budget_accounts WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = 'chat_request'
                )",
                [&fixture.principal.key_id],
            )
            .unwrap();
        connection
            .execute(
                "DELETE FROM quota_budget_accounts WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = 'chat_request'",
                [&fixture.principal.key_id],
            )
            .unwrap();
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "key-quota-missing".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;
        clear_core_test_executor();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(payload["error"]["code"], "key_quota_not_configured");
        assert!(executor.calls().is_empty());
        assert_eq!(fixture.state.core.as_ref().unwrap().store.count_rows("quota_reservations").unwrap(), 0);
    }

    #[tokio::test]
    async fn core_chat_rejects_missing_scope_before_preflight() {
        let fixture = core_fixture(1, &[]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "scope-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!(balance.held, 0);
    }

    #[tokio::test]
    async fn core_chat_rejects_missing_budget_policy_before_reservation() {
        let fixture = phase2_fixture(1);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "policy-1".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body_for_model("unpriced-model", false),
        ).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!(balance.available, 1);
        assert_eq!(balance.held, 0);
    }

    #[tokio::test]
    async fn core_chat_reserves_before_dispatch_and_replays_idempotency() {
        let fixture = phase2_fixture(2);
        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "idem-1".parse().unwrap());
        let first = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers.clone(),
            chat_body(false),
        ).await;
        let second = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        ).await;
        clear_core_test_executor();
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(second.status(), StatusCode::OK);
        assert_eq!(executor.calls().len(), 1);
        let bound_accounts = vec!["mock-account".to_owned()];
        let persisted = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .preflight_chat_with_lease_for_accounts(
                &fixture.principal,
                &fixture.principal.key_id,
                Some("idem-1"),
                &serde_json::from_slice(&chat_body(false)).unwrap(),
                &bound_accounts,
            )
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result, None);
        assert_eq!(
            persisted.replay_lease.as_ref().map(|lease| lease.state),
            Some(LeaseState::Succeeded)
        );
        assert!(persisted.execution.is_none());
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!(balance.available, 1);
        assert_eq!(balance.held, 0);
        let first_request_id = first.headers().get("x-request-id").cloned();
        let second_request_id = second.headers().get("x-request-id").cloned();
        assert_eq!(
            first_request_id,
            second_request_id
        );
        let replay_payload: Value = serde_json::from_slice(
            &axum::body::to_bytes(second.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        assert_eq!(replay_payload["idempotent_replay"], true);
        assert_eq!(replay_payload["choices"], json!([]));
    }

    #[tokio::test]
    async fn core_replay_of_failed_request_is_not_success() {
        let fixture = phase2_fixture(1);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let bound_accounts = vec!["mock-account".to_owned()];
        let first = bridge
            .preflight_chat_with_lease_for_accounts(&fixture.principal, &fixture.principal.key_id, Some("replay-failed"), &body, &bound_accounts)
            .unwrap();
        let lease = first.lease.as_ref().unwrap();
        bridge
            .settle_chat_lease(
                &fixture.principal,
                &lease.lease_id,
                UpstreamOutcome::Rejected {
                    status: 400,
                    code: "bad_request".into(),
                    accepted: false,
                }
                .lease_outcome(chrono::Utc::now().timestamp_millis()),
            )
            .unwrap();
        let persisted = bridge
            .preflight_chat_with_lease_for_accounts(&fixture.principal, &fixture.principal.key_id, Some("replay-failed"), &body, &bound_accounts)
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result.as_ref().and_then(|result| result.status), Some(400));
        assert!(persisted.result.as_ref().and_then(|result| result.error_code.as_deref()).is_some());

        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "replay-failed".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
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
        let fixture = phase2_fixture(1);
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let body: Value = serde_json::from_slice(&chat_body(false)).unwrap();
        let bound_accounts = vec!["mock-account".to_owned()];
        let first = bridge
            .preflight_chat_with_lease_for_accounts(&fixture.principal, &fixture.principal.key_id, Some("replay-unknown"), &body, &bound_accounts)
            .unwrap();
        let lease = first.lease.as_ref().unwrap();
        bridge
            .settle_chat_lease(
                &fixture.principal,
                &lease.lease_id,
                UpstreamOutcome::TransportUnknown {
                    reason: "transport_unknown".into(),
                    upstream_request_ref: None,
                }
                .lease_outcome(chrono::Utc::now().timestamp_millis()),
            )
            .unwrap();
        let persisted = bridge
            .preflight_chat_with_lease_for_accounts(&fixture.principal, &fixture.principal.key_id, Some("replay-unknown"), &body, &bound_accounts)
            .unwrap();
        assert_ne!(persisted.request_id, "");
        assert_eq!(persisted.result.as_ref().and_then(|result| result.status), None);
        assert_eq!(persisted.result.as_ref().and_then(|result| result.error_code.as_deref()), Some("transport_unknown"));

        let executor = Arc::new(super::super::core_bridge::core_executor::MockUpstreamExecutor::ok());
        install_core_test_executor(executor.clone());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "replay-unknown".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
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
            lease: None,
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
            lease: None,
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
            lease: None,
            reservation_amount: reservation.amount,
        };
        let response = Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "application/json")
            .body(Body::from(json!({"usage": {"total_tokens": 0}}).to_string()))
            .unwrap();
        settle_core_response(&context, response);
        let balance = key_balance(&fixture, "chat_request");
        assert_eq!(balance.available, 0);
        assert_eq!(balance.held, 1);
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

    fn phase3_stream_fixture(
        outcome: StreamTerminalOutcome,
        cancel_support: CancelSupport,
        register_stream: bool,
    ) -> (CoreFixture, Arc<super::super::core_bridge::core_executor::MockStreamAdapter>) {
        let mut fixture = core_fixture(2, &["chat:invoke"]);
        let store = fixture.state.core.as_ref().unwrap().store.clone();
        let mut account = aiwork_core::RegisterUpstreamAccount::new(
            "mock-account".into(),
            "mock".into(),
            "vault://mock/account".into(),
        );
        account.capabilities.insert("chat".into());
        store
            .upsert_upstream_account(account, &fixture.admin)
            .unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        store
            .append_upstream_observation(aiwork_core::UpstreamObservation::new(
                "mock-observation".into(),
                "mock-account".into(),
                "chat_request".into(),
                Some(100),
                1,
                "reader".into(),
                aiwork_core::ObservationStatus::Fresh,
                now,
                now + 600_000,
                json!({}),
            ))
            .unwrap();
        let runtime = super::super::scheduler::SchedulerRuntime::new(
            store.clone(),
            fixture.dir.clone(),
            super::super::scheduler::SchedulerMode::Enforce,
            Default::default(),
            Default::default(),
            now,
        )
        .unwrap();
        let stream_mock = Arc::new(
            super::super::core_bridge::core_executor::MockStreamAdapter::with_outcome(outcome)
                .with_cancel_support(cancel_support),
        );
        let nonstream_mock = Arc::new(
            super::super::core_bridge::core_executor::MockUpstreamExecutor::ok(),
        );
        let mut executor = CoreUpstreamExecutor::new().with_provider("mock", nonstream_mock);
        if register_stream {
            executor = executor
                .with_stream_provider("mock", stream_mock.clone())
                .for_account("mock-account", "mock", "vault://mock/account");
        }
        let bridge = CoreBridge::new(store, CoreMode::Enforce)
            .with_upstream_executor(executor)
            .with_scheduler(Arc::new(runtime))
            .unwrap();
        Arc::get_mut(&mut fixture.state).unwrap().core = Some(Arc::new(bridge));
        (fixture, stream_mock)
    }

    async fn response_body_text(response: Response) -> String {
        String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn core_stream_openai_mock_emits_done_and_settles_success() {
        let (fixture, mock) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-openai".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = response_body_text(response).await;
        assert!(body.contains("data:"));
        assert!(body.contains("hello"));
        assert!(body.contains("data: [DONE]"));
        assert_eq!(mock.calls().len(), 1);
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 1);
        let lease_state: String = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3"))
            .unwrap()
            .query_row("SELECT state FROM upstream_leases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(lease_state, "succeeded");
        assert_eq!(key_balance(&fixture, "chat_request").held, 1);
    }

    #[tokio::test]
    async fn core_stream_and_video_repeated_settlement_keeps_one_event_group() {
        let (stream_fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: Some(1),
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        let mut stream_headers = HeaderMap::new();
        stream_headers.insert("idempotency-key", "event-group-stream".parse().unwrap());
        let stream_response = chat_completions(
            State(stream_fixture.state.clone()),
            Some(Extension(KeyId(stream_fixture.principal.key_id.clone()))),
            None,
            Some(Extension(stream_fixture.principal.clone())),
            stream_headers,
            chat_body(true),
        )
        .await;
        assert_eq!(stream_response.status(), StatusCode::OK);
        let _ = response_body_text(stream_response).await;
        let stream_bridge = stream_fixture.state.core.as_ref().unwrap().clone();
        let stream_body: Value = serde_json::from_slice(&chat_body(true)).unwrap();
        let stream_replay = stream_bridge
            .lookup_chat_replay(
                &stream_fixture.principal,
                &stream_fixture.principal.key_id,
                "event-group-stream",
                &stream_body,
            )
            .unwrap()
            .unwrap();
        let stream_lease_id = stream_replay.replay_lease.as_ref().unwrap().id.clone();
        let repeated_stream = stream_bridge
            .settle_chat_lease(
                &stream_fixture.principal,
                &stream_lease_id,
                UpstreamOutcome::Success {
                    body: json!({"choices": []}),
                    actual_units: Some(1),
                    upstream_request_ref: None,
                }
                .lease_outcome(chrono::Utc::now().timestamp_millis()),
            )
            .unwrap();
        assert!(!repeated_stream.applied);
        let stream_db = rusqlite::Connection::open(stream_fixture.dir.join("data/core.sqlite3")).unwrap();
        let stream_group: String = stream_db
            .query_row("SELECT event_group_id FROM quota_reservations WHERE resource_kind = 'chat_request'", [], |row| row.get(0))
            .unwrap();
        let stream_events: i64 = stream_db
            .query_row("SELECT count(*) FROM quota_ledger WHERE event_group_id = ?1", [&stream_group], |row| row.get(0))
            .unwrap();
        assert_eq!(stream_events, 2);

        let (video_fixture, _) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::Accepted {
                upstream_request_ref: "event-group-video".into(),
            }],
            vec![VideoCancelOutcome::Confirmed {
                upstream_request_ref: Some("event-group-video".into()),
            }],
        );
        let mut video_headers = HeaderMap::new();
        video_headers.insert("idempotency-key", "event-group-video".parse().unwrap());
        let submitted = videos_generations(
            State(video_fixture.state.clone()),
            Some(Extension(KeyId(video_fixture.principal.key_id.clone()))),
            None,
            Some(Extension(video_fixture.principal.clone())),
            video_headers,
            Bytes::from(json!({"model":"mock-video","prompt":"event group"}).to_string()),
        )
        .await;
        let submitted_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(submitted.into_body(), usize::MAX).await.unwrap(),
        )
        .unwrap();
        let video_job_id = submitted_body["task"]["id"].as_str().unwrap().to_owned();
        let canceled = video_cancel(
            State(video_fixture.state.clone()),
            Some(Extension(video_fixture.principal.clone())),
            Path(video_job_id.clone()),
        )
        .await;
        assert_eq!(canceled.status(), StatusCode::ACCEPTED);
        let video_store = &video_fixture.state.core.as_ref().unwrap().store;
        let video_job = video_store
            .video_job_for_user(&video_fixture.principal, &video_job_id)
            .unwrap()
            .unwrap();
        let video_lease = video_store
            .upstream_lease_for_request(&video_job.request_id, "video_job")
            .unwrap()
            .unwrap();
        let repeated_video = video_fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .settle_video_job(
                &video_fixture.principal,
                &video_job_id,
                &video_lease.id,
                VideoAdapterOutcome::Canceled {
                    upstream_request_ref: Some("event-group-video".into()),
                },
            )
            .unwrap();
        assert!(!repeated_video.applied);
        let video_db = rusqlite::Connection::open(video_fixture.dir.join("data/core.sqlite3")).unwrap();
        let video_group: String = video_db
            .query_row("SELECT event_group_id FROM quota_reservations WHERE resource_kind = 'video_job'", [], |row| row.get(0))
            .unwrap();
        let video_events: i64 = video_db
            .query_row("SELECT count(*) FROM quota_ledger WHERE event_group_id = ?1", [&video_group], |row| row.get(0))
            .unwrap();
        assert_eq!(video_events, 2);
    }

    #[tokio::test]
    async fn core_stream_anthropic_and_responses_emit_protocol_terminal_events() {
        let (anthropic_fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        let mut anthropic_headers = HeaderMap::new();
        anthropic_headers.insert("idempotency-key", "phase3-anthropic".parse().unwrap());
        let anthropic = messages(
            State(anthropic_fixture.state.clone()),
            Some(Extension(KeyId(anthropic_fixture.principal.key_id.clone()))),
            None,
            Some(Extension(anthropic_fixture.principal.clone())),
            anthropic_headers,
            Bytes::from(
                json!({
                    "model": "mock-1",
                    "messages": [{"role": "user", "content": "hello"}],
                    "stream": true
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(anthropic.status(), StatusCode::OK);
        let anthropic_body = response_body_text(anthropic).await;
        assert!(anthropic_body.contains("event: message_start"));
        assert!(anthropic_body.contains("event: content_block_delta"));
        assert!(anthropic_body.contains("event: message_stop"));
        assert!(anthropic_body.find("message_start").unwrap() < anthropic_body.find("message_stop").unwrap());

        let (responses_fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        let mut responses_headers = HeaderMap::new();
        responses_headers.insert("idempotency-key", "phase3-responses".parse().unwrap());
        let responses = responses_api(
            State(responses_fixture.state.clone()),
            Some(Extension(KeyId(responses_fixture.principal.key_id.clone()))),
            None,
            Some(Extension(responses_fixture.principal.clone())),
            responses_headers,
            Bytes::from(
                json!({"model": "mock-1", "input": "hello", "stream": true}).to_string(),
            ),
        )
        .await;
        assert_eq!(responses.status(), StatusCode::OK);
        let responses_body = response_body_text(responses).await;
        assert!(responses_body.contains("event: response.output_text.delta"));
        assert!(responses_body.contains("event: response.completed"));
        assert!(responses_body.find("response.output_text.delta").unwrap() < responses_body.find("response.completed").unwrap());
    }

    #[tokio::test]
    async fn closed_client_channel_requests_cancel_and_unknown_without_release() {
        let (fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-disconnect".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        drop(response);

        let store = &fixture.state.core.as_ref().unwrap().store;
        let mut lease_state = String::new();
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            lease_state = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3"))
                .unwrap()
                .query_row("SELECT state FROM upstream_leases", [], |row| row.get(0))
                .unwrap_or_default();
            if lease_state == "unknown" {
                break;
            }
        }
        assert_eq!(lease_state, "unknown");
        let request_id: String = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3"))
            .unwrap()
            .query_row("SELECT request_id FROM upstream_leases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Unknown);
        assert_eq!(store.reservation_for_request(&request_id).unwrap().unwrap().state, aiwork_core::ReservationState::Unknown);
        assert_eq!(key_balance(&fixture, "chat_request").held, 1);
    }

    #[tokio::test]
    async fn core_stream_without_registered_adapter_returns_501_before_lease() {
        let (fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            false,
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-no-stream-adapter".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "scheduler_endpoint_not_enabled");
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("requests").unwrap(), 0);
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 0);
    }

    #[tokio::test]
    async fn core_video_without_adapter_returns_501_before_any_persistent_write() {
        let fixture = core_fixture(
            0,
            &["videos:submit", "videos:read", "videos:cancel"],
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "video-no-adapter".parse().unwrap());
        let response = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            Bytes::from(json!({"model":"mock-video","prompt":"hello"}).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "scheduler_endpoint_not_enabled");
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("requests").unwrap(), 0);
        assert_eq!(store.count_rows("quota_reservations").unwrap(), 0);
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 0);
        assert_eq!(store.count_rows("jobs").unwrap(), 0);
        assert_eq!(store.count_rows("job_attempts").unwrap(), 0);
    }

    #[tokio::test]
    async fn core_video_submission_is_persistent_running_and_idempotent() {
        let (fixture, adapter) = phase3_video_fixture(
            2,
            vec![VideoAdapterOutcome::Accepted {
                upstream_request_ref: "upstream-video-1".into(),
            }],
            Vec::new(),
        );
        let body = Bytes::from(json!({"model":"mock-video","prompt":"hello"}).to_string());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "video-idempotent".parse().unwrap());
        let first = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers.clone(),
            body.clone(),
        )
        .await;
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        let first_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(first.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(first_body["task"]["status"], "running");

        let replay = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            body,
        )
        .await;
        assert_eq!(replay.status(), StatusCode::ACCEPTED);
        let replay_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(replay.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(replay_body["idempotent_replay"], true);
        assert_eq!(replay_body["task"]["status"], "running");
        assert_eq!(adapter.calls(), vec![replay_body["task"]["id"].as_str().unwrap().to_string()]);

        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("requests").unwrap(), 1);
        assert_eq!(store.count_rows("quota_reservations").unwrap(), 1);
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 1);
        assert_eq!(store.count_rows("jobs").unwrap(), 1);
        assert_eq!(store.count_rows("job_attempts").unwrap(), 1);
        assert_eq!(key_balance(&fixture, "video_job").held, 1);
        let job_id = replay_body["task"]["id"].as_str().unwrap();
        let job = store
            .video_job_for_user(&fixture.principal, job_id)
            .unwrap()
            .unwrap();
        assert_eq!(job.state, aiwork_core::JobState::Running);
        let attempt = store
            .video_job_attempt_for_user(&fixture.principal, job_id)
            .unwrap()
            .unwrap();
        assert_eq!(attempt.upstream_request_ref.as_deref(), Some("upstream-video-1"));
    }

    #[tokio::test]
    async fn core_video_confirmed_cancel_releases_hold_once() {
        let (fixture, _) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::Accepted {
                upstream_request_ref: "upstream-video-cancel".into(),
            }],
            vec![VideoCancelOutcome::Confirmed {
                upstream_request_ref: Some("upstream-video-cancel".into()),
            }],
        );
        let body = Bytes::from(json!({"model":"mock-video","prompt":"cancel me"}).to_string());
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "video-cancel".parse().unwrap());
        let submitted = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            body,
        )
        .await;
        let submitted_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(submitted.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        let job_id = submitted_body["task"]["id"].as_str().unwrap().to_owned();

        let canceled = video_cancel(
            State(fixture.state.clone()),
            Some(Extension(fixture.principal.clone())),
            Path(job_id.clone()),
        )
        .await;
        let canceled_status = canceled.status();
        let canceled_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(canceled.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(canceled_status, StatusCode::ACCEPTED);
        assert_eq!(canceled_body["status"], "canceled");
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(key_balance(&fixture, "video_job").held, 0);
        assert_eq!(key_balance(&fixture, "video_job").available, 1);
        assert_eq!(
            store.video_job_for_user(&fixture.principal, &job_id).unwrap().unwrap().state,
            aiwork_core::JobState::Canceled
        );
        assert_eq!(
            store
                .count_rows("quota_ledger")
                .unwrap(),
            3,
            "grant, reservation hold, plus one cancel release ledger entry"
        );
    }

    #[tokio::test]
    async fn core_video_explicit_rejection_releases_without_success() {
        let (fixture, adapter) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::Rejected {
                status: 400,
                code: "invalid_request".into(),
                accepted: false,
            }],
            Vec::new(),
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "video-rejected".parse().unwrap());
        let response = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            Bytes::from(json!({"model":"mock-video","prompt":"reject me"}).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "upstream_error");
        assert_eq!(adapter.calls().len(), 1);
        let job_id = adapter.calls()[0].clone();
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(key_balance(&fixture, "video_job").held, 0);
        assert_eq!(key_balance(&fixture, "video_job").available, 1);
        assert_eq!(
            store.video_job_for_user(&fixture.principal, &job_id).unwrap().unwrap().state,
            aiwork_core::JobState::Failed
        );
    }

    #[tokio::test]
    async fn core_video_unknown_submission_keeps_hold_for_reconciliation() {
        let (fixture, _) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::TransportUnknown {
                reason: "transport_timeout".into(),
                upstream_request_ref: None,
            }],
            Vec::new(),
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "video-unknown".parse().unwrap());
        let response = videos_generations(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            Bytes::from(json!({"model":"mock-video","prompt":"uncertain"}).to_string()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["task"]["status"], "unknown");
        assert_eq!(body["task"]["reconcile_required"], true);
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(key_balance(&fixture, "video_job").held, 1);
        assert_eq!(
            store.video_job_for_user(&fixture.principal, body["task"]["id"].as_str().unwrap()).unwrap().unwrap().state,
            aiwork_core::JobState::Unknown
        );
    }

    #[test]
    fn durable_mock_worker_claims_persisted_payload_and_settles_once() {
        let (fixture, adapter) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::Succeeded {
                actual_units: Some(1),
                upstream_request_ref: Some("worker-upstream".into()),
                output_ref: Some("jobs/worker-output".into()),
                artifact_ref: None,
            }],
            Vec::new(),
        );
        let body = json!({"model": "mock-video", "prompt": "worker payload"});
        let job_id = "worker-persisted-job";
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let input_hash =
            aiwork_core::canonical_json_hash(&super::super::payload::sanitize_scheduler_chat_body(&body));
        fixture
            .state
            .video_payloads
            .put(job_id, &fixture.principal.user_id, &input_hash, &body)
            .unwrap();
        bridge
            .enqueue_video_job(
                &fixture.principal,
                &fixture.principal.key_id,
                Some("worker-persisted-idempotency"),
                &body,
                job_id.into(),
            )
            .unwrap();

        let worker = super::super::video_worker::VideoQueueWorker::new(
            fixture.state.clone(),
            "mock-worker",
        );
        let step = worker.run_once().unwrap();
        assert_eq!(
            step,
            super::super::video_worker::VideoWorkerStep::Settled {
                job_id: job_id.into(),
                state: "succeeded",
            }
        );
        assert_eq!(adapter.calls(), vec![job_id.to_string()]);
        let store = &bridge.store;
        assert_eq!(
            store.video_job_for_user(&fixture.principal, job_id).unwrap().unwrap().state,
            aiwork_core::JobState::Succeeded
        );
        assert_eq!(key_balance(&fixture, "video_job").held, 0);
        assert_eq!(
            fixture
                .state
                .video_payloads
                .get(job_id, &fixture.principal.user_id, &input_hash)
                .unwrap(),
            None
        );
    }

    #[test]
    fn durable_worker_missing_payload_becomes_unknown_without_releasing_hold() {
        let (fixture, adapter) = phase3_video_fixture(
            1,
            vec![VideoAdapterOutcome::Succeeded {
                actual_units: Some(1),
                upstream_request_ref: Some("should-not-run".into()),
                output_ref: Some("jobs/should-not-run".into()),
                artifact_ref: None,
            }],
            Vec::new(),
        );
        let body = json!({"model": "mock-video", "prompt": "not persisted"});
        let bridge = fixture.state.core.as_ref().unwrap().clone();
        let result = bridge
            .enqueue_video_job(
                &fixture.principal,
                &fixture.principal.key_id,
                Some("worker-missing-payload"),
                &body,
                "worker-missing-payload-job".into(),
            )
            .unwrap();
        let job_id = match result {
            VideoJobEnqueueResult::Created { job, .. } => job.id,
            VideoJobEnqueueResult::Replay { .. } => panic!("unexpected replay"),
        };

        let worker = super::super::video_worker::VideoQueueWorker::new(
            fixture.state.clone(),
            "mock-worker-missing-payload",
        );
        assert_eq!(
            worker.run_once().unwrap(),
            super::super::video_worker::VideoWorkerStep::Unknown {
                job_id: job_id.clone(),
                reason: "video_payload_missing".into(),
            }
        );
        assert!(adapter.calls().is_empty());
        assert_eq!(key_balance(&fixture, "video_job").held, 1);
        assert_eq!(
            bridge
                .store
                .video_job_for_user(&fixture.principal, &job_id)
                .unwrap()
                .unwrap()
                .state,
            aiwork_core::JobState::Unknown
        );
    }

    #[test]
    fn heartbeat_failure_preserves_only_a_safe_upstream_reference() {
        assert_eq!(
            video_outcome_request_ref(&VideoAdapterOutcome::Accepted {
                upstream_request_ref: "upstream-1".into(),
            }),
            Some("upstream-1".into())
        );
        assert_eq!(
            video_outcome_request_ref(&VideoAdapterOutcome::Succeeded {
                actual_units: Some(1),
                upstream_request_ref: Some("upstream-2".into()),
                output_ref: Some("jobs/output".into()),
                artifact_ref: None,
            }),
            Some("upstream-2".into())
        );
        assert_eq!(
            video_outcome_request_ref(&VideoAdapterOutcome::Rejected {
                status: 502,
                code: "upstream".into(),
                accepted: false,
            }),
            None
        );
    }

    #[test]
    fn public_text_model_value_advertises_text_route_metadata() {
        let model = unified_catalog::UnifiedModel {
            id: "deepseek-v4-flash".into(),
            display: "DeepSeek V4 Flash".into(),
            vendor: "Trae".into(),
            rate: Some(1.0),
            efforts: vec!["medium".into()],
            context_length: Some(131_072),
            max_tokens: Some(8_192),
            supports_image: Some(false),
            sources: vec![unified_catalog::UnifiedSource {
                pool: "trae",
                rate: Some(1.0),
                enabled: true,
            }],
            manual: false,
        };

        let value = public_unified_model_value(&model);
        assert_eq!(value["capabilities"], serde_json::json!(["text"]));
        assert_eq!(value["endpoint"], "/v1/chat/completions");
        assert_eq!(value["async"], false);
        assert_eq!(value["id"], "deepseek-v4-flash");
    }

    #[test]
    fn public_seedance_value_advertises_video_route_and_availability() {
        let value = public_seedance_model_value(false);
        assert_eq!(value["id"], "seedance");
        assert_eq!(value["capabilities"], serde_json::json!(["video"]));
        assert_eq!(value["endpoint"], "/v1/videos/generations");
        assert_eq!(value["async"], true);
        assert_eq!(value["sources"][0]["pool"], "trae_work");
        assert_eq!(value["sources"][0]["enabled"], false);

        let available = public_seedance_model_value(true);
        assert_eq!(available["sources"][0]["enabled"], true);
    }

    #[test]
    fn legacy_handlers_use_the_persisted_policy_without_defaulting_authenticated_keys() {
        let limits = KeyLimits {
            max_inflight: Some(2),
            max_video_jobs: None,
            asset_uploads_per_minute: None,
            asset_bytes_per_hour: None,
            video_submissions_per_minute: None,
            daily_requests: 3,
            daily_tokens: 7,
        };
        let (_state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_CHAT]);
        let mut file = api_keys::load(&dir);
        file.keys[0].limits = limits.clone();
        api_keys::save(&dir, &file);

        assert_eq!(
            require_legacy_capability(
                &key_id,
                Some(&api_keys::constraints_for(&dir, &key_id).unwrap()),
                api_keys::CAPABILITY_CHAT,
            )
            .unwrap(),
            limits
        );
        let missing = require_legacy_capability(
            &key_id,
            Some(&api_keys::constraints_for(&dir, &key_id).unwrap()),
            api_keys::CAPABILITY_VIDEO,
        )
        .unwrap_err();
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            require_legacy_capability(
                "anonymous",
                None,
                api_keys::CAPABILITY_CHAT,
            )
            .unwrap(),
            KeyLimits::default()
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn anonymous_legacy_video_capability_requires_an_api_key() {
        let response = require_legacy_capability(
            "anonymous",
            None,
            api_keys::CAPABILITY_VIDEO,
        )
        .expect_err("anonymous callers must not access legacy video endpoints");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn legacy_handlers_use_auth_snapshot_and_fail_closed_when_snapshot_is_missing() {
        let (_state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_CHAT]);
        let mut file = api_keys::load(&dir);
        file.keys[0].limits = KeyLimits {
            max_inflight: Some(2),
            daily_requests: 3,
            daily_tokens: 7,
            ..KeyLimits::default()
        };
        api_keys::save(&dir, &file);
        let snapshot = api_keys::constraints_for(&dir, &key_id).unwrap();

        let mut changed = api_keys::load(&dir);
        changed.keys[0].limits = KeyLimits {
            max_inflight: Some(99),
            daily_requests: 99,
            daily_tokens: 99,
            ..KeyLimits::default()
        };
        api_keys::save(&dir, &changed);

        assert_eq!(
            require_legacy_capability(
                &key_id,
                Some(&snapshot),
                api_keys::CAPABILITY_CHAT,
            )
                .unwrap(),
            snapshot.limits,
            "legacy policy must use the snapshot captured during authentication"
        );
        let missing = require_legacy_capability(&key_id, None, api_keys::CAPABILITY_CHAT)
            .expect_err("authenticated requests must fail closed without a snapshot");
        assert_eq!(missing.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let missing_body = axum::body::to_bytes(missing.into_body(), usize::MAX)
        .await
        .expect("missing snapshot body should be readable");
        let missing_body: Value = serde_json::from_slice(&missing_body).unwrap();
        assert_eq!(missing_body["error"]["code"], "auth_snapshot_missing");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn chat_completions_counts_one_authenticated_request_without_duplicate_increment() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_CHAT]);
        let body = Bytes::from(
            json!({
                "model": "mock-1",
                "messages": [{"role": "user", "content": "hello"}]
            })
            .to_string(),
        );

        let _response = chat_completions(
            State(state.clone()),
            Some(Extension(KeyId(key_id))),
            legacy_snapshot(&state, "legacy-route-key"),
            None,
            HeaderMap::new(),
            body,
        )
        .await;

        assert_eq!(
            state
                .total_requests
                .load(std::sync::atomic::Ordering::Relaxed),
            1
        );
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn legacy_text_quota_rejection_releases_the_request_permit() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_CHAT]);
        let limits = KeyLimits {
            max_inflight: Some(2),
            daily_tokens: 10,
            ..KeyLimits::default()
        };
        let first = legacy_text_request_guard(&state, &key_id, &limits)
            .expect("first text request should reserve the token budget");
        let rejected = match legacy_text_request_guard(&state, &key_id, &limits) {
            Ok(_) => panic!("the second concurrent request must hit the token quota"),
            Err(response) => response,
        };
        assert_eq!(rejected.status(), StatusCode::TOO_MANY_REQUESTS);
        let rejected_body = axum::body::to_bytes(rejected.into_body(), usize::MAX)
            .await
            .expect("quota response body should be readable");
        let rejected_body: Value = serde_json::from_slice(&rejected_body).unwrap();
        assert_eq!(rejected_body["error"]["type"], "quota_exceeded");
        assert_eq!(rejected_body["error"]["code"], "quota_exceeded");
        assert_eq!(rejected_body["error"]["legacy_code"], "daily_quota_exceeded");
        assert_eq!(rejected_body["error"]["param"], "daily_tokens");

        let permit_after_rejection = state
            .acquire_request(&key_id, &limits)
            .expect("failed reservation must release the request permit");
        drop(permit_after_rejection);
        drop(first);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn legacy_text_guard_skips_reservation_for_anonymous_and_unlimited_keys() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_CHAT]);
        let finite_limits = KeyLimits {
            max_inflight: Some(2),
            daily_tokens: 10,
            ..KeyLimits::default()
        };
        let anonymous_guard = legacy_text_request_guard(&state, "anonymous", &finite_limits)
            .expect("anonymous requests should not create token reservations");
        drop(anonymous_guard);

        let unlimited_limits = KeyLimits {
            max_inflight: Some(2),
            daily_tokens: 0,
            ..KeyLimits::default()
        };
        let unlimited_guard = legacy_text_request_guard(&state, &key_id, &unlimited_limits)
            .expect("daily_tokens=0 should not create token reservations");
        drop(unlimited_guard);

        assert!(api_keys::load(&dir).keys[0].token_reservations.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn core_nonstream_rejects_before_execution_when_request_limiter_is_full() {
        let (mut fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        Arc::get_mut(&mut fixture.state)
            .expect("core fixture state should be uniquely owned")
            .limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 1,
                max_video_jobs: 4,
                asset_uploads_per_minute: 8,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 8,
            },
        );
        let held = fixture
            .state
            .acquire_request(&fixture.principal.key_id, &KeyLimits::default())
            .expect("test should fill the Core request limiter");
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "core-nonstream-limiter-full".parse().unwrap());

        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(false),
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "concurrency_limit");
        assert_eq!(
            fixture
                .state
                .core
                .as_ref()
                .unwrap()
                .store
                .count_rows("requests")
                .unwrap(),
            0
        );
        drop(held);
    }

    #[tokio::test]
    async fn core_stream_rejects_before_preflight_when_request_limiter_is_full() {
        let (mut fixture, _) = phase3_stream_fixture(
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            },
            CancelSupport::Unsupported,
            true,
        );
        Arc::get_mut(&mut fixture.state)
            .expect("core fixture state should be uniquely owned")
            .limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 1,
                max_video_jobs: 4,
                asset_uploads_per_minute: 8,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 8,
            },
        );
        let held = fixture
            .state
            .acquire_request(&fixture.principal.key_id, &KeyLimits::default())
            .expect("test should fill the Core request limiter");
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "core-stream-limiter-full".parse().unwrap());

        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "concurrency_limit");
        assert_eq!(
            fixture
                .state
                .core
                .as_ref()
                .unwrap()
                .store
                .count_rows("requests")
                .unwrap(),
            0
        );
        drop(held);
    }

    #[tokio::test]
    async fn seedance_chat_requires_video_scope_in_core_mode() {
        let fixture = core_fixture(1, &["chat:invoke"]);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "seedance-scope-test".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            Bytes::from(
                json!({
                    "model": "seedance",
                    "messages": [{"role": "user", "content": "海边日落"}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["error"]["code"], "insufficient_scope");
    }

    #[tokio::test]
    async fn seedance_chat_rejects_streaming_and_missing_idempotency() {
        let fixture = core_fixture(1, &["videos:submit"]);
        let mut stream_headers = HeaderMap::new();
        stream_headers.insert("idempotency-key", "seedance-stream-test".parse().unwrap());
        let stream_response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            stream_headers,
            Bytes::from(
                json!({
                    "model": "seedance",
                    "stream": true,
                    "messages": [{"role": "user", "content": "一只猫"}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(stream_response.status(), StatusCode::BAD_REQUEST);
        let stream_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(stream_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(stream_body["error"]["code"], "seedance_stream_unsupported");

        let missing_key_response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            HeaderMap::new(),
            Bytes::from(
                json!({
                    "model": "seedance",
                    "messages": [{"role": "user", "content": "一只狗"}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(missing_key_response.status(), StatusCode::BAD_REQUEST);
        let missing_key_body: Value = serde_json::from_slice(
            &axum::body::to_bytes(missing_key_response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(missing_key_body["error"]["code"], "idempotency_key_required");
    }

    #[tokio::test]
    async fn seedance_chat_submits_through_core_video_adapter_and_returns_chat_envelope() {
        let (fixture, _adapter) = phase3_video_fixture(
            2,
            vec![VideoAdapterOutcome::Accepted {
                upstream_request_ref: "mock-seedance-task".into(),
            }],
            vec![],
        );
        fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .store
            .upsert_cost_policy(CostPolicy {
                id: "route-seedance-chat-policy".into(),
                endpoint: "videos".into(),
                model_pattern: "seedance".into(),
                resource_kind: "video_job".into(),
                reserve_amount: 1,
                max_actual_amount: Some(1),
                version: 1,
                enabled: true,
            })
            .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "seedance-chat-success".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            None,
            Some(Extension(fixture.principal.clone())),
            headers,
            Bytes::from(
                json!({
                    "model": " Seedance ",
                    "messages": [{"role": "user", "content": "一只猫在月光下奔跑"}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body: Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(body["object"], "chat.completion");
        assert_eq!(body["model"], "seedance");
        assert_eq!(body["choices"][0]["finish_reason"], "video_async");
        assert_eq!(body["video_task"]["model"], "seedance");
        assert_eq!(body["video_task"]["status"], "running");
    }

    #[test]
    fn seedance_inline_asset_is_owned_by_the_legacy_api_key() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_VIDEO]);
        let projection = super::super::seedance_chat::project_chat_to_video(&json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "让画面动起来"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,iVBORw0KGgo="
                }}
            ]}]
        }))
        .unwrap();
        let mut video_input = projection.video_input.clone();
        let asset_ids = persist_seedance_inline_assets(
            &state,
            &mut video_input,
            &projection.inline_images,
            Some(&key_id),
            None,
            "legacy-inline-test",
        )
        .unwrap();
        assert_eq!(asset_ids.len(), 1);
        assert_eq!(video_input["image_asset_ids"][0], asset_ids[0]);
        assert!(super::super::assets::read_owned(&dir, &key_id, &asset_ids[0]).is_ok());
        assert!(super::super::assets::read_owned(&dir, "another-key", &asset_ids[0]).is_err());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn seedance_inline_asset_is_owned_by_the_core_user() {
        let fixture = core_fixture(1, &["videos:submit"]);
        let projection = super::super::seedance_chat::project_chat_to_video(&json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "让画面动起来"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,iVBORw0KGgo="
                }}
            ]}]
        }))
        .unwrap();
        let mut video_input = projection.video_input.clone();
        let asset_ids = persist_seedance_inline_assets(
            &fixture.state,
            &mut video_input,
            &projection.inline_images,
            None,
            Some(&fixture.principal),
            "core-inline-test",
        )
        .unwrap();
        let asset = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .store
            .asset_for_user(&fixture.principal, &asset_ids[0])
            .unwrap();
        assert!(asset.is_some());
        assert!(fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .store
            .asset_for_user(&fixture.admin, &asset_ids[0])
            .unwrap()
            .is_none());
    }

    #[test]
    fn repeated_seedance_inline_asset_persistence_reuses_the_same_asset() {
        let (state, dir, key_id) = legacy_fixture_with_capabilities(&[api_keys::CAPABILITY_VIDEO]);
        let projection = super::super::seedance_chat::project_chat_to_video(&json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "重复请求"},
                {"type": "image_url", "image_url": {
                    "url": "data:image/png;base64,iVBORw0KGgo="
                }}
            ]}]
        }))
        .unwrap();
        let mut first = projection.video_input.clone();
        let first_ids = persist_seedance_inline_assets(
            &state,
            &mut first,
            &projection.inline_images,
            Some(&key_id),
            None,
            "same-idempotency-key",
        )
        .unwrap();
        let mut second = projection.video_input.clone();
        let second_ids = persist_seedance_inline_assets(
            &state,
            &mut second,
            &projection.inline_images,
            Some(&key_id),
            None,
            "same-idempotency-key",
        )
        .unwrap();
        assert_eq!(first_ids, second_ids);
        assert_eq!(first["image_asset_ids"], second["image_asset_ids"]);
        let index: Value = crate::fs_utils::read_json(&dir.join("data/assets.json"));
        assert_eq!(index["assets"].as_array().unwrap().len(), 1);
        let _ = fs::remove_dir_all(dir);
    }
}

fn bind_core_usage_session(
    body: Vec<u8>,
    attribution: Option<&super::bridge_billing::CoreRequestAttribution>,
    account_ref: &str,
) -> Result<Vec<u8>, String> {
    let Some(attribution) = attribution else {
        return Ok(body);
    };
    let session_id = super::bridge_billing::core_usage_session_id(&attribution.request_id, account_ref);
    super::payload::bind_upstream_session_id(&body, &session_id)
}
