//! AI Work → 星链维度分流系统的脱敏桥接接口。
//! 这里刻意只返回能力目录、执行聚合和来源新鲜度，不暴露账号池行、UID、JWT、Cookie
//! 或单账号积分；桥接 Key 由 auth middleware 在业务路由进入前校验。

use std::{collections::HashSet, sync::Arc};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use aiwork_core::{CreditAmount, UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS};

use super::{unified_catalog, ApiSharedState};

#[derive(Clone, Debug, Serialize)]
pub struct BridgeStatusResponse {
    pub service: &'static str,
    pub ready: bool,
    pub bridge_only: bool,
    pub default_model: String,
    pub capabilities: Vec<&'static str>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BridgeModelView {
    pub id: String,
    pub display: String,
    pub vendor: String,
    pub supports_image: Option<bool>,
    pub capabilities: Vec<&'static str>,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BridgeModelsResponse {
    pub object: &'static str,
    pub data: Vec<BridgeModelView>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamCreditAggregate {
    /// 通用积分；可用于文字请求，也可用于视频 Work。
    pub general: Option<String>,
    /// Work 积分；用于视频 Work。
    pub work: Option<String>,
    /// 视频可用积分，即通用积分与 Work 积分的聚合值。
    pub video_available: Option<String>,
    /// 兼容旧客户端的聚合字段，等于 `video_available`。
    pub value: Option<String>,
    pub source: &'static str,
    pub fresh: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<&'static str>,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BridgeSummaryResponse {
    /// 账号明细不属于桥接协议；null 明确表示“服务端不对外提供账号计数”。
    pub active_accounts: Option<u64>,
    pub active_models: usize,
    pub current_inflight: u64,
    pub total_requests: u64,
    pub queued_jobs: u64,
    pub running_jobs: u64,
    pub reconciliation_jobs: u64,
    pub upstream_credits: UpstreamCreditAggregate,
    pub updated_at: i64,
}

impl BridgeSummaryResponse {
    pub(crate) fn from_values_with_credits<I>(
        active_models: usize,
        current_inflight: u64,
        total_requests: u64,
        snapshots: I,
    ) -> Self
    where
        I: IntoIterator<Item = (bool, Option<f64>, Option<f64>, Option<i64>)>,
    {
        let mut active_accounts = 0_u64;
        let mut general_total: Option<CreditAmount> = None;
        let mut work_total: Option<CreditAmount> = None;
        let mut general_count = 0_u64;
        let mut work_count = 0_u64;
        let mut unified_total: Option<CreditAmount> = None;
        let mut video_count = 0_u64;
        let mut has_active_account = false;
        let mut incomplete = false;
        let mut invalid = false;
        let mut stale = false;
        let mut oldest_observed_at_ms: Option<i64> = None;
        let now = Utc::now().timestamp_millis();

        for (active, general_value, work_value, observed_at_ms) in snapshots {
            if !active { continue; }
            has_active_account = true;
            if general_value.is_some() || work_value.is_some() { active_accounts += 1; }
            if general_value.is_none() && work_value.is_none() {
                incomplete = true;
            }

            let general = match general_value {
                Some(value) => match exact_credit_amount(value) {
                    Some(amount) => Some(amount),
                    None => { invalid = true; None }
                },
                None => None,
            };
            let work = match work_value {
                Some(value) => match exact_credit_amount(value) {
                    Some(amount) => Some(amount),
                    None => { invalid = true; None }
                },
                None => None,
            };

            if let Some(amount) = general {
                if !accumulate_credit(&mut general_total, amount) { invalid = true; }
                general_count += 1;
            }
            if let Some(amount) = work {
                if !accumulate_credit(&mut work_total, amount) { invalid = true; }
                work_count += 1;
            }

            let account_total = match (general, work) {
                (Some(general), Some(work)) => match general.checked_add(work) {
                    Some(total) => Some(total),
                    None => { invalid = true; None }
                },
                (Some(general), None) => Some(general),
                (None, Some(work)) => Some(work),
                (None, None) => None,
            };
            if let Some(amount) = account_total {
                if !accumulate_credit(&mut unified_total, amount) { invalid = true; }
                video_count += 1;
            }

            match observed_at_ms {
                Some(observed_at) if observed_at > 0 => {
                    oldest_observed_at_ms = Some(
                        oldest_observed_at_ms.map_or(observed_at, |oldest| oldest.min(observed_at)),
                    );
                    let age_ms = now.saturating_sub(observed_at);
                    if age_ms < 0 || age_ms > UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS {
                        stale = true;
                    }
                }
                _ => stale = true,
            }
        }

        let has_snapshot = has_active_account && !incomplete && !invalid && !stale;
        let general = (!invalid && general_count > 0)
            .then(|| general_total.map(|amount| amount.to_string()))
            .flatten();
        let work = (!invalid && work_count > 0)
            .then(|| work_total.map(|amount| amount.to_string()))
            .flatten();
        let video_available = (!invalid && video_count > 0)
            .then(|| unified_total.map(|amount| amount.to_string()))
            .flatten();
        let error_code = if has_snapshot {
            None
        } else if !has_active_account {
            Some("upstream_aggregate_unavailable")
        } else if invalid {
            Some("upstream_balance_invalid")
        } else if incomplete {
            Some("upstream_balance_incomplete")
        } else {
            Some("upstream_balance_stale")
        };
        Self {
            active_accounts: (active_accounts > 0).then_some(active_accounts),
            active_models,
            current_inflight,
            total_requests,
            queued_jobs: 0,
            running_jobs: 0,
            reconciliation_jobs: 0,
            upstream_credits: UpstreamCreditAggregate {
                general,
                work,
                video_available: video_available.clone(),
                value: video_available,
                source: "aiwork-upstream-aggregate",
                fresh: has_snapshot,
                error_code,
                updated_at: oldest_observed_at_ms.unwrap_or(0),
            },
            updated_at: oldest_observed_at_ms.unwrap_or(now),
        }
    }

}

fn exact_credit_amount(value: f64) -> Option<CreditAmount> {
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    CreditAmount::parse(&value.to_string(), "credits").ok()
}

fn accumulate_credit(total: &mut Option<CreditAmount>, amount: CreditAmount) -> bool {
    match *total {
        Some(current) => match current.checked_add(amount) {
            Some(sum) => { *total = Some(sum); true }
            None => false,
        },
        None => { *total = Some(amount); true }
    }
}

pub async fn status(State(state): State<Arc<ApiSharedState>>) -> Json<BridgeStatusResponse> {
    Json(BridgeStatusResponse {
        service: "ai-work-bridge",
        ready: true,
        bridge_only: super::api_keys::bridge_only(&state.data_dir),
        default_model: state.default_model.clone(),
        capabilities: vec!["chat", "video", "assets"],
        updated_at: Utc::now().timestamp_millis(),
    })
}

pub async fn models(State(state): State<Arc<ApiSharedState>>) -> Json<BridgeModelsResponse> {
    let models = unified_catalog::unified_models(
        &state.data_dir,
        state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed),
        state.pool.has_selectable(),
        state.wb_pool.has_selectable(),
    );
    let data = models
        .into_iter()
        .map(|model| {
            let is_seedance = model.id.eq_ignore_ascii_case("seedance");
            BridgeModelView {
            id: model.id,
            display: model.display,
            vendor: model.vendor,
            supports_image: model.supports_image,
            capabilities: if is_seedance {
                vec!["video", "assets"]
            } else {
                vec!["chat"]
            },
            enabled: model.sources.iter().any(|source| source.enabled),
        }
        })
        .collect();
    Json(BridgeModelsResponse {
        object: "list",
        data,
        updated_at: Utc::now().timestamp_millis(),
    })
}

pub async fn summary(State(state): State<Arc<ApiSharedState>>) -> Json<BridgeSummaryResponse> {
    let active_models = unified_catalog::unified_models(
        &state.data_dir,
        state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed),
        state.pool.has_selectable(),
        state.wb_pool.has_selectable(),
    )
    .into_iter()
    .filter(|model| model.sources.iter().any(|source| source.enabled))
    .count();
    let mut snapshots = state.pool.bridge_credit_snapshots();
    if state.wb_enabled.load(std::sync::atomic::Ordering::Relaxed) {
        snapshots.extend(state.wb_pool.bridge_credit_snapshots());
    }
    Json(BridgeSummaryResponse::from_values_with_credits(
        active_models,
        state.inflight.load(std::sync::atomic::Ordering::Relaxed),
        state.total_requests.load(std::sync::atomic::Ordering::Relaxed),
        snapshots,
    ))
}

/// TRAE's official session-usage API reports session-aggregated fiat cost and
/// returns `-` for custom models; it is not a per-request credit ceiling.
/// https://docs.trae.cn/enterprise_query-usage-details-by-session-id
pub async fn quote(
    Json(request): Json<super::bridge_billing::BridgeQuoteRequest>,
) -> Response {
    if !super::bridge_billing::valid_request_id(&request.request_id)
        || request.endpoint.trim().is_empty()
        || request.endpoint.len() > 128
        || request.model.trim().is_empty()
        || request.model.len() > 128
        || request.request_fingerprint.trim().is_empty()
        || request.request_fingerprint.len() > 256
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "code": "invalid_quote_request",
                    "message": "request_id, endpoint, model and request_fingerprint are required",
                }
            })),
        )
            .into_response();
    }

    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(super::bridge_billing::quote_unavailable(&request.request_id)),
    )
        .into_response()
}

/// Returns a persisted request-scoped receipt; absence is `unknown`, never zero.
pub async fn billing(
    State(state): State<Arc<ApiSharedState>>,
    Path(request_id): Path<String>,
) -> Response {
    if !super::bridge_billing::valid_request_id(&request_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {
                    "code": "invalid_request_id",
                    "message": "request_id is invalid",
                }
            })),
        )
            .into_response();
    }

    let store = match super::bridge_billing::BridgeBillingStore::open(&state.data_dir) {
        Ok(store) => store,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": {
                        "code": "billing_store_unavailable",
                        "message": "billing records are temporarily unavailable",
                    }
                })),
            )
                .into_response();
        }
    };
    match store.get_receipt(&request_id) {
        Ok(Some(receipt)) => Json(receipt).into_response(),
        Ok(None) => Json(super::bridge_billing::unknown_receipt(request_id)).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": {
                    "code": "billing_store_unavailable",
                    "message": "billing records are temporarily unavailable",
                }
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizeVideoBillingRequest {
    task_ref: String,
}

/// Core calls this only after it has observed a terminal video task state.
/// The one-shot test path promotes usage only from the unique session that was
/// created for that request and Key; until the read-only poller sees the row,
/// the response stays unknown and the Core reservation remains held.
pub async fn finalize_video_billing(
    State(state): State<Arc<ApiSharedState>>,
    Path(request_id): Path<String>,
    Json(input): Json<FinalizeVideoBillingRequest>,
) -> Response {
    let task_ref = input.task_ref.trim().to_string();
    if !super::bridge_billing::valid_request_id(&request_id)
        || task_ref.is_empty()
        || task_ref.len() > 256
        || task_ref.chars().any(char::is_control)
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":{"code":"invalid_video_billing_finalization","message":"request_id or task_ref is invalid"}})),
        ).into_response();
    }

    let data_dir = state.data_dir.clone();
    let request_for_lookup = request_id.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        let store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        Ok::<_, String>((
            store.get_receipt(&request_for_lookup)?,
            store.one_shot_session_for_request(&request_for_lookup)?,
        ))
    }).await;
    let (existing, session_lookup) = match snapshot {
        Ok(Ok(snapshot)) => snapshot,
        _ => return bridge_billing_unavailable(),
    };
    if let Some(receipt) = existing.filter(|receipt| matches!(
        receipt.status,
        super::bridge_billing::BillingReceiptStatus::Final
    )) {
        if receipt.task_ref.as_deref() == Some(task_ref.as_str()) {
            return Json(receipt).into_response();
        }
        if receipt.task_ref.is_some() {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":{"code":"billing_task_ref_conflict","message":"final billing task reference does not match"}})),
            ).into_response();
        }
    }

    let (core_key_id, account_ref, session_id) = match session_lookup {
        super::bridge_billing::OneShotSessionLookup::Unique {
            core_key_id, account_ref, session_id, ..
        } => (core_key_id, account_ref, session_id),
        super::bridge_billing::OneShotSessionLookup::Unauthorized
        | super::bridge_billing::OneShotSessionLookup::Missing
        | super::bridge_billing::OneShotSessionLookup::Ambiguous => {
            return Json(super::bridge_billing::unknown_receipt(request_id)).into_response();
        }
    };

    let candidate = match crate::commands::usage_history::core_usage_receipt_candidate(
        &state.data_dir,
        &account_ref,
        &session_id,
        &request_id,
        &core_key_id,
    ) {
        Ok(Some(candidate)) => candidate,
        Ok(None) => return Json(super::bridge_billing::unknown_receipt(request_id)).into_response(),
        Err(_) => return bridge_billing_unavailable(),
    };
    let receipt = super::bridge_billing::BillingReceipt {
        request_id: request_id.clone(),
        status: super::bridge_billing::BillingReceiptStatus::Final,
        actual_credits: Some(candidate.credits),
        unit: Some("credits".into()),
        source_ref: Some(format!("trae-usage-session:{}", candidate.session_id)),
        task_ref: Some(task_ref),
        observed_at_ms: candidate.observed_at_ms,
    };
    let data_dir = state.data_dir.clone();
    let receipt_for_write = receipt.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        store.record_one_shot_session_receipt(
            &receipt_for_write,
            &account_ref,
            &session_id,
            &core_key_id,
        )?;
        store.get_receipt(&receipt_for_write.request_id)
    }).await;
    match result {
        Ok(Ok(Some(receipt))) => Json(receipt).into_response(),
        _ => bridge_billing_unavailable(),
    }
}

/// Confirm a one-shot text helper only from the unique, attributed upstream
/// usage session. Unlike video finalization, no video task reference is valid.
pub async fn finalize_chat_billing(
    State(state): State<Arc<ApiSharedState>>,
    Path(request_id): Path<String>,
) -> Response {
    if !super::bridge_billing::valid_request_id(&request_id) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":{"code":"invalid_request_id"}})),
        ).into_response();
    }
    let data_dir = state.data_dir.clone();
    let request_for_lookup = request_id.clone();
    let snapshot = tokio::task::spawn_blocking(move || {
        let store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        Ok::<_, String>((
            store.get_receipt(&request_for_lookup)?,
            store.one_shot_session_for_request(&request_for_lookup)?,
        ))
    }).await;
    let (existing, session_lookup) = match snapshot {
        Ok(Ok(snapshot)) => snapshot,
        _ => return bridge_billing_unavailable(),
    };
    if let Some(receipt) = existing.filter(|receipt| matches!(
        receipt.status,
        super::bridge_billing::BillingReceiptStatus::Final
    )) {
        if receipt.task_ref.is_none() {
            return Json(receipt).into_response();
        }
        return (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":{"code":"billing_task_ref_conflict"}})),
        ).into_response();
    }
    let (core_key_id, account_ref, session_id, associated_at_ms) = match session_lookup {
        super::bridge_billing::OneShotSessionLookup::Unique {
            core_key_id, account_ref, session_id, associated_at_ms,
        } => (core_key_id, account_ref, session_id, associated_at_ms),
        _ => return Json(super::bridge_billing::unknown_receipt(request_id)).into_response(),
    };
    let mut candidate = match crate::commands::usage_history::core_usage_receipt_candidate(
        &state.data_dir, &account_ref, &session_id, &request_id, &core_key_id,
    ) {
        Ok(candidate) => candidate,
        Err(_) => return bridge_billing_unavailable(),
    };
    if candidate.is_none() {
        let account_refs = HashSet::from([account_ref.clone()]);
        let credentials = state.pool.usage_credentials_for(&account_refs);
        if !credentials.is_empty() {
            for attempt in 0..3 {
                if attempt > 0 {
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
                let data_dir = state.data_dir.clone();
                let pending = vec![(account_ref.clone(), associated_at_ms)];
                let credentials = credentials.clone();
                let refreshed = tokio::task::spawn_blocking(move || {
                    crate::commands::usage_history::refresh_pending_core_usage(
                        &data_dir, &pending, &credentials,
                    )
                }).await;
                if !matches!(refreshed, Ok(Ok(_))) {
                    break;
                }
                candidate = match crate::commands::usage_history::core_usage_receipt_candidate(
                    &state.data_dir, &account_ref, &session_id, &request_id, &core_key_id,
                ) {
                    Ok(candidate) => candidate,
                    Err(_) => return bridge_billing_unavailable(),
                };
                if candidate.is_some() {
                    break;
                }
            }
        }
    }
    let Some(candidate) = candidate else {
        return Json(super::bridge_billing::unknown_receipt(request_id)).into_response();
    };
    let receipt = super::bridge_billing::BillingReceipt {
        request_id: request_id.clone(),
        status: super::bridge_billing::BillingReceiptStatus::Final,
        actual_credits: Some(candidate.credits),
        unit: Some("credits".into()),
        source_ref: Some(format!("trae-usage-session:{}", candidate.session_id)),
        task_ref: None,
        observed_at_ms: candidate.observed_at_ms,
    };
    let data_dir = state.data_dir.clone();
    let receipt_for_write = receipt.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        store.record_one_shot_chat_receipt(
            &receipt_for_write, &account_ref, &session_id, &core_key_id,
        )?;
        store.get_receipt(&receipt_for_write.request_id)
    }).await;
    match result {
        Ok(Ok(Some(receipt))) => Json(receipt).into_response(),
        _ => bridge_billing_unavailable(),
    }
}

fn bridge_billing_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error":{"code":"billing_store_unavailable","message":"billing records are temporarily unavailable"}})),
    ).into_response()
}

/// Replaces AI Work's redacted mirror of Core API Keys. This route is only
/// reachable through the dedicated bridge credential middleware.
pub(super) async fn replace_core_key_registry(
    State(state): State<Arc<ApiSharedState>>,
    Json(snapshot): Json<super::bridge_billing::CoreKeyRegistrySnapshot>,
) -> Response {
    let data_dir = state.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        let count = snapshot.keys.len();
        let version = snapshot.version;
        let update = store.replace_core_key_registry(&snapshot)?;
        Ok::<_, String>((version, count, update))
    }).await;
    match result {
        Ok(Ok((version, count, update))) => Json(serde_json::json!({
            "status": match update {
                super::bridge_billing::RegistryUpdate::Applied => "applied",
                super::bridge_billing::RegistryUpdate::Unchanged => "unchanged",
            },
            "version": version,
            "key_count": count,
        })).into_response(),
        Ok(Err(error)) if error.contains("stale") || error.contains("version was reused") => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({"error":{"code":"core_key_registry_conflict","message":"Core Key registry snapshot is stale or conflicts with the stored version"}})),
        ).into_response(),
        Ok(Err(error)) if error.contains("invalid") || error.contains("outside the allowed bounds") || error.contains("duplicate metadata") => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error":{"code":"invalid_core_key_registry","message":"Core Key registry snapshot could not be applied"}})),
        ).into_response(),
        Ok(Err(_)) | Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":{"code":"core_key_registry_unavailable","message":"Core Key registry synchronization is temporarily unavailable"}})),
        ).into_response(),
    }
}

/// Redacted per-Key usage view. Only verified request-scoped credit receipts
/// contribute to `verified_credits`; unknown requests remain pending.
pub(super) async fn core_key_usage(State(state): State<Arc<ApiSharedState>>) -> Response {
    let data_dir = state.data_dir.clone();
    let result = tokio::task::spawn_blocking(move || {
        let store = super::bridge_billing::BridgeBillingStore::open(&data_dir)?;
        store.core_key_usage()
    }).await;
    match result {
        Ok(Ok(keys)) => Json(serde_json::json!({
            "keys": keys,
            "updated_at_ms": Utc::now().timestamp_millis(),
        })).into_response(),
        Ok(Err(_)) | Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"error":{"code":"core_key_usage_unavailable","message":"Core Key usage is temporarily unavailable"}})),
        ).into_response(),
    }
}


#[cfg(test)]
mod tests {
    use super::BridgeSummaryResponse;

    #[test]
    fn bridge_summary_contains_aggregates_but_no_pool_rows() {
        let observed_at = chrono::Utc::now().timestamp_millis();
        let body = serde_json::to_value(BridgeSummaryResponse::from_values_with_credits(
            2,
            0,
            4,
            [
                (true, Some(12.0), Some(8.0), Some(observed_at)),
                (true, Some(3.0), None, Some(observed_at)),
                (false, Some(99.0), Some(99.0), Some(observed_at)),
            ],
        )).unwrap();
        assert_eq!(body["active_accounts"], 2);
        assert_eq!(body["active_models"], 2);
        assert_eq!(body["upstream_credits"]["general"], "15.000000");
        assert_eq!(body["upstream_credits"]["work"], "8.000000");
        assert_eq!(body["upstream_credits"]["video_available"], "23.000000");
        assert_eq!(body["upstream_credits"]["value"], "23.000000");
        assert_eq!(body["upstream_credits"]["fresh"], true);
        assert_eq!(body["upstream_credits"]["updated_at"], observed_at);
        assert!(body.get("accounts").is_none());
        assert!(body.get("uids").is_none());
        assert!(body.get("credits_by_account").is_none());
        assert!(!body.to_string().contains("upstream_aggregate_unavailable"));
    }

    #[test]
    fn bridge_summary_marks_stale_missing_and_imprecise_snapshots_unusable() {
        let now = chrono::Utc::now().timestamp_millis();
        let stale = BridgeSummaryResponse::from_values_with_credits(
            1,
            0,
            0,
            [(true, Some(10.0), Some(0.0), Some(now - 300_001))],
        );
        assert!(!stale.upstream_credits.fresh);
        assert_eq!(stale.upstream_credits.error_code, Some("upstream_balance_stale"));

        let missing = BridgeSummaryResponse::from_values_with_credits(
            1,
            0,
            0,
            [(true, None, None, None)],
        );
        assert!(!missing.upstream_credits.fresh);
        assert_eq!(missing.upstream_credits.error_code, Some("upstream_balance_incomplete"));

        let imprecise = BridgeSummaryResponse::from_values_with_credits(
            1,
            0,
            0,
            [(true, Some(10.1234567), Some(0.0), Some(now))],
        );
        assert!(!imprecise.upstream_credits.fresh);
        assert_eq!(imprecise.upstream_credits.error_code, Some("upstream_balance_invalid"));
    }
}
