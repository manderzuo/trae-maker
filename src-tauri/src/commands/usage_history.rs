//! Trae Work 积分消耗历史（`POST /trae/api/v1/pay/query_user_usage_group_by_session`）。
//!
//! 此前积分看板的「消耗」口径是 credits_daily 快照的余额差值推算（含签到获得等噪声）；
//! 本模块改为直连接口拉取会话级用量（credits_float / model_name / token 明细），按本地
//! 自然日聚合落盘 data/usage_history.json，供积分趋势图查询展示。
//!
//! 增量语义（避免重复计数）：
//! - 首次手动完整同步（包括已有后台短期缓存）：全量拉取近一年（FULL_PULL_DAYS）；
//! - 后续拉取（fresh=true）：从「上次查询上界所在本地日的 00:00」起重拉；仅当查询起点
//!   覆盖整日且查询上界不早于已成功上界时才替换日快照，同范围重查可纳入迟到账单；
//!   更早的历史保持不动；
//! - fresh=false：纯缓存读取，零网络。
//!
//! 请求形态（2026-09-13 代理日志实测）：
//! `{"start_time":<unix秒>,"end_time":<unix秒>,"page_size":N,"page_num":1,"usage_type":[7]}`
//! 响应：`{"total":<会话总数>,"user_usage_group_by_sessions":[{usage_time, credits_float,
//! model_name, extra_info:{input_token,output_token,cache_read_token}, ...}]}`
//!
//! 凭证红线：JWT 仅进请求头（复用 ide_query_post），不进日志/返回值。

use chrono::TimeZone;
use aiwork_core::CreditAmount;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use tauri::State;

use crate::state::AppState;

/// 首次全量拉取窗口（天）
const FULL_PULL_DAYS: i64 = 365;
/// 拉取分块窗口（天）：对齐官方控制台请求粒度，过大区间会被参数校验拒绝（400/9004）
const CHUNK_DAYS: i64 = 30;
/// 单账号单块分页安全上限（防 total 异常导致死循环；50 页 × 20 = 1000 会话/块）
const MAX_PAGES: u32 = 50;
/// 单页大小（对齐官方控制台实测值 20）
const PAGE_SIZE: u32 = 20;
/// 用量类型 7 = Cloud-IDE 会话积分消耗（实测口径）
const USAGE_TYPE: i64 = 7;

const USAGE_URL: &str = "https://api.trae.cn/trae/api/v1/pay/query_user_usage_group_by_session";

/// 单日聚合（date → 消耗合计 / 会话数 / 模型分布 / token 明细）
#[derive(Serialize, serde::Deserialize, Clone, Default)]
pub struct UsageDayStat {
    pub date: String,
    pub credits: f64,
    pub sessions: u64,
    pub models: BTreeMap<String, f64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
}

#[derive(Serialize, Clone, Default)]
pub struct UsageHistoryAccount {
    pub user_id: String,
    pub name: String,
    pub ok: bool,
    /// 本次增量拉取失败但已沿用缓存时的说明；无缓存时为失败原因
    pub error: Option<String>,
    /// 按日期升序
    pub daily: Vec<UsageDayStat>,
}

#[derive(Serialize, Clone)]
pub struct UsageHistoryResult {
    pub fetched_at: i64,
    /// true = 纯缓存读取（未发起网络请求）
    pub cached: bool,
    pub accounts: Vec<UsageHistoryAccount>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CachedAccount {
    name: String,
    /// 上次拉取的 end_time（Unix 秒）——增量起点 = 该时刻所在本地日的 00:00
    last_fetch_end_ts: Option<i64>,
    /// Set only after a successful manual 365-day sync; absent in older caches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manual_full_sync_at: Option<i64>,
    daily: BTreeMap<String, UsageDayStat>,
    /// Per-session candidates retained internally for exact request attribution.
    /// This field is intentionally not included in `UsageHistoryAccount` responses.
    #[serde(default)]
    session_usage: BTreeMap<String, CachedSessionUsage>,
    /// Actual source reads for each returned row, not account/global cache reads.
    #[serde(default)]
    session_observations: BTreeMap<String, SessionObservation>,
}

#[derive(serde::Serialize,serde::Deserialize,Clone,PartialEq,Eq)]
struct SessionObservation {
    read_started_at_ms:i64,
    completed_at_ms:i64,
    query_end_ms:i64,
    #[serde(default)]
    complete:bool,
    #[serde(default)]
    row:Option<CachedSessionUsage>,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Debug, Default, PartialEq, Eq)]
struct CachedSessionUsage {
    session_id: String,
    usage_time: i64,
    date: String,
    model_name: String,
    /// Preserve the source decimal text; never use this candidate as a final receipt by itself.
    credits_float: Option<String>,
    #[serde(default)]
    ambiguous: bool,
    /// Only a local candidate link produced by the authenticated Core session map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    core_key_id: Option<String>,
    #[serde(default)]
    core_attribution_ambiguous: bool,
}

type CoreUsageSessionMatches =
    HashMap<(String, String), crate::api_server::bridge_billing::CoreUsageSessionMatch>;

pub(crate) struct CoreUsageReceiptCandidate {
    pub session_id: String,
    pub credits: CreditAmount,
    pub observed_at_ms: i64,
}

#[derive(serde::Deserialize, serde::Serialize, Clone, Default)]
struct CacheFile {
    fetched_at: Option<i64>,
    accounts: BTreeMap<String, CachedAccount>,
}

#[derive(Default)]
struct FetchedAccountUsage {
    daily: BTreeMap<String, UsageDayStat>,
    sessions: BTreeMap<String, CachedSessionUsage>,
    source_complete: bool,
}

fn cache_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join("data").join("usage_history.json")
}

pub(crate) struct BudgetUsageReceiptEvidence {
    pub session_id: String,
    pub credits: CreditAmount,
    pub read_started_at_ms: i64,
    pub observed_at_ms: i64,
    pub query_end_ms: i64,
}

#[derive(Clone,Debug,serde::Serialize,serde::Deserialize)]
pub(crate) struct BudgetUsageSourceConflict {
    pub source_ref:String,
    pub evidence_hash:String,
    pub observed_at_ms:i64,
    pub reason:String,
}
pub(crate) fn budget_usage_source_conflict(data_dir:&std::path::Path,account_ref:&str,session_id:&str,finished_at_ms:i64)->Result<Option<BudgetUsageSourceConflict>,String> {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine};
    let cache=read_usage_cache_for_merge(&data_dir.join("data").join("usage_history.json"))?;
    let Some(account)=cache.accounts.get(account_ref) else {return Ok(None)};
    let Some(source)=account.session_observations.get(session_id) else {return Ok(None)};
    let Some(row)=source.row.as_ref() else {return Ok(None)};
    if !source.complete || row.session_id!=session_id || !row.ambiguous || finished_at_ms<=0 || source.read_started_at_ms<finished_at_ms || source.completed_at_ms<source.read_started_at_ms {return Ok(None);}
    let hash=URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&serde_json::json!({"account_ref":account_ref,"session_id":session_id,
        "reason":"conflicting_source_rows","usage_time":row.usage_time,"model":row.model_name,"credits":row.credits_float})));
    Ok(Some(BudgetUsageSourceConflict {source_ref:format!("trae-usage-session:{session_id}"),evidence_hash:hash,
        observed_at_ms:source.completed_at_ms,reason:"conflicting_source_rows".into()}))
}

pub(crate) fn budget_usage_receipt_evidence(
    data_dir: &std::path::Path, account_ref: &str, session_id: &str,
    request_id: &str, core_key_id: &str, finished_at_ms: i64,
) -> Result<Option<BudgetUsageReceiptEvidence>, String> {
    let cache=read_usage_cache_for_merge(&data_dir.join("data").join("usage_history.json"))?;
    let Some(account)=cache.accounts.get(account_ref) else {return Ok(None)};
    let Some(source)=account.session_observations.get(session_id) else {return Ok(None)};
    let Some(row)=source.row.as_ref() else {return Ok(None)};
    if !source.complete || row.session_id!=session_id || finished_at_ms<=0 || source.read_started_at_ms<finished_at_ms || source.completed_at_ms<source.read_started_at_ms
        || source.query_end_ms<finished_at_ms || row.usage_time<=0 || row.ambiguous || row.core_attribution_ambiguous
        || row.core_request_id.as_deref()!=Some(request_id) || row.core_key_id.as_deref()!=Some(core_key_id) {return Ok(None);}
    let Some(text)=row.credits_float.as_deref() else {return Ok(None)};
    let Ok(credits)=CreditAmount::parse(text,"credits") else {return Ok(None)};
    Ok(Some(BudgetUsageReceiptEvidence {session_id:row.session_id.clone(),credits,read_started_at_ms:source.read_started_at_ms,
        observed_at_ms:source.completed_at_ms,query_end_ms:source.query_end_ms}))
}

fn account_refresh_lock(data_dir: &std::path::Path, uid: &str) -> std::sync::Arc<std::sync::Mutex<()>> {
    type LockMap = HashMap<(std::path::PathBuf, String), std::sync::Weak<std::sync::Mutex<()>>>;
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<LockMap>> = std::sync::OnceLock::new();
    let locks = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let data_key = std::fs::canonicalize(data_dir).unwrap_or_else(|_| data_dir.to_path_buf());
    let mut locks = locks.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = (data_key, uid.to_string());
    if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
        return lock;
    }
    let lock = std::sync::Arc::new(std::sync::Mutex::new(()));
    locks.insert(key, std::sync::Arc::downgrade(&lock));
    lock
}

fn usage_cache_merge_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

fn read_usage_cache_for_merge(path: &std::path::Path) -> Result<CacheFile, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(CacheFile::default()),
        Err(_) => return Err("usage history cache read failed".into()),
    };
    serde_json::from_slice(&bytes).map_err(|_| "usage history cache is invalid".into())
}

fn account_summary(name: String, uid: String, daily: &BTreeMap<String, UsageDayStat>) -> UsageHistoryAccount {
    // 防御：date 一律从映射键回填（旧缓存条目的 date 字段可能为空串）
    let daily = daily
        .iter()
        .map(|(k, v)| {
            let mut d = v.clone();
            d.date = k.clone();
            d
        })
        .collect();
    UsageHistoryAccount {
        user_id: uid,
        name,
        ok: true,
        error: None,
        daily,
    }
}

/// Unix 秒 → 本地自然日（YYYY-MM-DD）
fn local_date_of(ts: i64) -> Option<String> {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string())
}

/// 本地自然日 → 当日 00:00 的 Unix 秒（本地时区；无效日期回退 None）
fn local_midnight_ts(date: &str) -> Option<i64> {
    let d = chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    match chrono::Local
        .from_local_datetime(&d.and_hms_opt(0, 0, 0)?)
    {
        chrono::LocalResult::Single(dt) => Some(dt.timestamp()),
        chrono::LocalResult::Ambiguous(dt, _) => Some(dt.timestamp()),
        chrono::LocalResult::None => None,
    }
}

/// A daily snapshot can replace an existing value only if this query starts at
/// the local day's beginning and does not precede the successful query high-water.
/// Equal bounds are allowed: a later serialized read may observe delayed upstream rows.
fn query_can_replace_daily(
    date: &str,
    query_start_ts: i64,
    query_end_ts: i64,
    previous_query_end_ts: Option<i64>,
) -> bool {
    if previous_query_end_ts.is_some_and(|previous_end| query_end_ts < previous_end) {
        return false;
    }
    let Some(day_start_ts) = local_midnight_ts(date) else {
        return false;
    };
    query_start_ts <= day_start_ts
}

/// 官网控制台（Web 端）形态请求：对齐 2026-09-13 代理抓包的成功请求——
/// 浏览器 UA + origin/referer www.trae.cn + sec-fetch cors/same-site，
/// **不带** IDE 客户端指纹头（x-market-*/x-device-id 等；该接口按 Web 路由校验，
/// 客户端头 + 大分页/大区间组合返回 400 code=9004 参数错误）。
fn web_usage_post(agent: &ureq::Agent, jwt: &str, body: serde_json::Value) -> Result<Value, String> {
    let auth = if jwt.starts_with("Cloud-IDE-JWT ") {
        jwt.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    };
    let resp = agent
        .post(USAGE_URL)
        .set("accept", "application/json, text/plain, */*")
        .set("accept-language", "zh-CN,zh;q=0.9")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("user-agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36")
        .set("origin", "https://www.trae.cn")
        .set("referer", "https://www.trae.cn/")
        .set("sec-fetch-dest", "empty")
        .set("sec-fetch-mode", "cors")
        .set("sec-fetch-site", "same-site")
        .send_json(body)
        .map_err(|e| match e {
            ureq::Error::Status(code, resp) => {
                let body = resp.into_string().unwrap_or_default();
                let snippet: String = body.chars().take(200).collect();
                format!("API 请求失败: status code {code}，响应: {snippet}")
            }
            other => format!("API 请求失败: {other}"),
        })?;
    resp.into_json().map_err(|e| format!("解析响应失败: {e}"))
}

/// 单账号拉取 [start_ts, end_ts] 区间并按本地日聚合。
/// 大区间按 30 天分块（对齐官方控制台请求窗口；chunk 间边界无缝不重叠）。
/// 失败返回 Err（调用方沿用缓存）。
fn fetch_account_usage(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
) -> Result<FetchedAccountUsage, String> {
    let agent = crate::commands::accounts::pay_status_agent();
    fetch_account_usage_with_pages(start_ts, end_ts, || false, |start, end, page| {
        web_usage_post(
            &agent,
            jwt,
            json!({
                "start_time": start,
                "end_time": end,
                "page_size": PAGE_SIZE,
                "page_num": page,
                "usage_type": [USAGE_TYPE],
            }),
        )
    })
}

fn fetch_account_usage_with_stop(
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
    stop: &std::sync::atomic::AtomicBool,
) -> Result<FetchedAccountUsage, String> {
    let agent = crate::commands::accounts::pay_status_agent();
    fetch_account_usage_with_pages(
        start_ts,
        end_ts,
        || stop.load(std::sync::atomic::Ordering::Acquire),
        |start, end, page| {
            web_usage_post(
                &agent,
                jwt,
                json!({
                    "start_time": start,
                    "end_time": end,
                    "page_size": PAGE_SIZE,
                    "page_num": page,
                    "usage_type": [USAGE_TYPE],
                }),
            )
        },
    )
}

fn fetch_account_usage_with_pages<C, F>(
    start_ts: i64,
    end_ts: i64,
    cancelled: C,
    mut request_page: F,
) -> Result<FetchedAccountUsage, String>
where
    C: Fn() -> bool,
    F: FnMut(i64, i64, u32) -> Result<Value, String>,
{
    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut sessions = BTreeMap::new();
    let mut chunk_end = end_ts;
    let mut source_complete=true;
    loop {
        if cancelled() {
            return Err("usage refresh cancelled".into());
        }
        let chunk_start = (chunk_end - CHUNK_DAYS * 86400 + 1).max(start_ts);
        source_complete &= fetch_chunk_with_pages(
            chunk_start,
            chunk_end,
            &cancelled,
            &mut request_page,
            &mut agg,
            &mut sessions,
        )?;
        if chunk_start <= start_ts {
            break;
        }
        chunk_end = chunk_start - 1;
    }
    if cancelled() {
        return Err("usage refresh cancelled".into());
    }
    Ok(FetchedAccountUsage { daily: agg, sessions, source_complete })
}

/// Return actual credits from one exact, uniquely attributed upstream session.
/// This is only a candidate; the caller must additionally verify the unique
/// authenticated Core request/session association. For video, the Core
/// caller must also verify that its task reached a terminal state.
pub(crate) fn core_usage_receipt_candidate(
    data_dir: &std::path::Path,
    account_ref: &str,
    session_id: &str,
    request_id: &str,
    core_key_id: &str,
) -> Result<Option<CoreUsageReceiptCandidate>, String> {
    let cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
    let Some(session) = cache.accounts.get(account_ref)
        .and_then(|account| account.session_usage.get(session_id)) else {
        return Ok(None);
    };
    if session.ambiguous
        || session.core_attribution_ambiguous
        || session.core_request_id.as_deref() != Some(request_id)
        || session.core_key_id.as_deref() != Some(core_key_id)
    {
        return Ok(None);
    }
    let Some(credits_text) = session.credits_float.as_deref() else {
        return Ok(None);
    };
    let Ok(credits) = CreditAmount::parse(credits_text, "credits") else {
        return Ok(None);
    };
    let observed_at_ms = chrono::Utc::now().timestamp_millis();
    if observed_at_ms <= 0 || session.usage_time <= 0 {
        return Ok(None);
    }
    Ok(Some(CoreUsageReceiptCandidate {
        session_id: session.session_id.clone(),
        credits,
        observed_at_ms,
    }))
}

/// Periodically refresh usage only for accounts that have an outstanding,
/// uniquely attributable Core session attempt. This path never issues a
/// generation request; it only calls the existing read-only usage-history API.
#[allow(dead_code)] // Preserve the existing non-cancellable crate interface; background workers use the stop-aware sibling.
pub(crate) fn refresh_pending_core_usage(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
) -> Result<usize, String> {
    let never_stop = std::sync::atomic::AtomicBool::new(false);
    refresh_pending_core_usage_with_stop(data_dir, pending_accounts, credentials, &never_stop)
}

pub(crate) fn refresh_pending_core_usage_with_stop(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    stop: &std::sync::atomic::AtomicBool,
) -> Result<usize, String> {
    refresh_pending_core_usage_with_stop_and_clock(
        data_dir,
        pending_accounts,
        credentials,
        || chrono::Local::now().timestamp(),
        stop,
        |_, jwt, start, end, stop| fetch_account_usage_with_stop(jwt, start, end, stop),
    )
}

fn refresh_pending_core_usage_with<F>(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    now_ts: i64,
    fetch: F,
) -> Result<usize, String>
where
    F: FnMut(&str, &str, i64, i64) -> Result<FetchedAccountUsage, String>,
{
    refresh_pending_core_usage_with_clock(
        data_dir,
        pending_accounts,
        credentials,
        || now_ts,
        fetch,
    )
}

fn refresh_pending_core_usage_with_clock<C, F>(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    clock: C,
    mut fetch: F,
) -> Result<usize, String>
where
    C: Fn() -> i64,
    F: FnMut(&str, &str, i64, i64) -> Result<FetchedAccountUsage, String>,
{
    let never_stop = std::sync::atomic::AtomicBool::new(false);
    refresh_pending_core_usage_with_stop_and_clock(
        data_dir,
        pending_accounts,
        credentials,
        clock,
        &never_stop,
        |uid, jwt, start, end, _| fetch(uid, jwt, start, end),
    )
}

fn refresh_pending_core_usage_with_stop_and_clock<C, F>(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    clock: C,
    stop: &std::sync::atomic::AtomicBool,
    fetch: F,
) -> Result<usize, String>
where
    C: Fn() -> i64,
    F: FnMut(&str, &str, i64, i64, &std::sync::atomic::AtomicBool)
        -> Result<FetchedAccountUsage, String>,
{
    refresh_pending_core_usage_with_stop_and_clock_and_matcher(
        data_dir,
        pending_accounts,
        credentials,
        clock,
        stop,
        fetch,
        crate::api_server::bridge_billing::match_core_usage_sessions,
    )
}

fn refresh_pending_core_usage_with_stop_and_clock_and_matcher<C, F, M>(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    clock: C,
    stop: &std::sync::atomic::AtomicBool,
    mut fetch: F,
    matcher: M,
) -> Result<usize, String>
where
    C: Fn() -> i64,
    F: FnMut(&str, &str, i64, i64, &std::sync::atomic::AtomicBool)
        -> Result<FetchedAccountUsage, String>,
    M: Fn(&std::path::Path, &[(String, String)]) -> Result<CoreUsageSessionMatches, String>,
{
    if pending_accounts.is_empty() {
        return Ok(0);
    }
    let mut refreshed = 0usize;
    let mut failed = false;

    for (uid, attempt_at_ms) in pending_accounts {
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return Err("pending Core usage refresh stopped".into());
        }
        let Some((_, name, jwt)) = credentials.iter().find(|(account_uid, _, jwt)| account_uid == uid && !jwt.trim().is_empty()) else {
            continue;
        };
        let account_lock = account_refresh_lock(data_dir, uid);
        let _account_guard = account_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if stop.load(std::sync::atomic::Ordering::Acquire) {
            return Err("pending Core usage refresh stopped".into());
        }
        // Capture the read window only after acquiring the per-account lock.
        let now_ts = clock();
        let poll_floor = now_ts.saturating_sub(7 * 86400);
        let floor_date = local_date_of(poll_floor).unwrap_or_default();
        let attempt_date = local_date_of(attempt_at_ms.div_euclid(1000)).unwrap_or_else(|| floor_date.clone());
        let cache_path = data_dir.join("data").join("usage_history.json");
        let (cached_date, mut attribution_session_ids) = {
            let _cache_guard = usage_cache_merge_lock()
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let cache = match read_usage_cache_for_merge(&cache_path) {
                Ok(cache) => cache,
                Err(_) => {
                    failed = true;
                    continue;
                }
            };
            let account = cache.accounts.get(uid);
            let cached_date = account
                .and_then(|account| account.last_fetch_end_ts)
                .and_then(local_date_of);
            let session_ids = account
                .map(|account| account.session_usage.keys().cloned().collect::<HashSet<_>>())
                .unwrap_or_default();
            (cached_date, session_ids)
        };
        let requested_date = cached_date.map_or(attempt_date.clone(), |date| date.min(attempt_date));
        let start_date = requested_date.max(floor_date.clone());
        let start_ts = local_midnight_ts(&start_date).unwrap_or(poll_floor);

        // The local Windows clock may lag the upstream clock by several minutes.
        // Only widen this read-only query window; attribution still requires an
        // exact Core request, key, account and upstream session match.
        let query_end_ts = now_ts.saturating_add(5 * 60);
        match fetch(uid, jwt, start_ts, query_end_ts, stop) {
            Ok(fetched) => {
                if stop.load(std::sync::atomic::Ordering::Acquire) {
                    return Err("pending Core usage refresh stopped".into());
                }
                let completed_at_ts = clock();
                let end_date = local_date_of(query_end_ts).unwrap_or_else(|| start_date.clone());
                attribution_session_ids.extend(fetched.sessions.keys().cloned());
                let attribution_pairs: Vec<(String, String)> = attribution_session_ids.into_iter()
                    .map(|session_id| (uid.clone(), session_id))
                    .collect();
                let attribution = if attribution_pairs.is_empty() {
                    None
                } else {
                    matcher(data_dir, &attribution_pairs)
                        .ok()
                        .map(|matches| (attribution_pairs, matches))
                };
                let _cache_guard = usage_cache_merge_lock()
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if stop.load(std::sync::atomic::Ordering::Acquire) {
                    return Err("pending Core usage refresh stopped".into());
                }
                let mut cache = match read_usage_cache_for_merge(&cache_path) {
                    Ok(cache) => cache,
                    Err(_) => {
                        failed = true;
                        continue;
                    }
                };
                merge_fetched_account(
                    &mut cache,
                    uid,
                    name,
                    &start_date,
                    &end_date,
                    start_ts,
                    query_end_ts,
                    completed_at_ts,
                    now_ts,
                    fetched,
                );
                if let Some((pairs, matches)) = attribution {
                    apply_core_usage_matches(&mut cache, &pairs, &matches);
                }
                if let Some(parent) = cache_path.parent() {
                    if std::fs::create_dir_all(parent).is_err() {
                        failed = true;
                        continue;
                    }
                }
                if stop.load(std::sync::atomic::Ordering::Acquire) {
                    return Err("pending Core usage refresh stopped".into());
                }
                match crate::fs_utils::write_json(&cache_path, &cache) {
                    Ok(()) => refreshed += 1,
                    Err(_) => failed = true,
                }
            }
            Err(_) if stop.load(std::sync::atomic::Ordering::Acquire) => {
                return Err("pending Core usage refresh stopped".into());
            }
            Err(_) => failed = true,
        }
    }

    if stop.load(std::sync::atomic::Ordering::Acquire) {
        return Err("pending Core usage refresh stopped".into());
    }
    if failed && refreshed == 0 {
        return Err("pending Core usage source query failed".into());
    }
    Ok(refreshed)
}

fn merge_fetched_account(
    cache: &mut CacheFile,
    uid: &str,
    name: &str,
    start_date: &str,
    end_date: &str,
    query_start_ts: i64,
    query_end_ts: i64,
    completed_at_ts: i64,
    read_started_at_ts: i64,
    fetched: FetchedAccountUsage,
) {
    let entry = cache.accounts.entry(uid.to_string()).or_default();
    entry.name = name.to_string();
    let previous_query_end_ts = entry.last_fetch_end_ts;
    entry.daily.retain(|date, _| {
        date.as_str() < start_date
            || date.as_str() > end_date
            || !query_can_replace_daily(
                date,
                query_start_ts,
                query_end_ts,
                previous_query_end_ts,
            )
    });
    for (date, mut stat) in fetched.daily {
        if date.as_str() >= start_date
            && date.as_str() <= end_date
            && !query_can_replace_daily(
                &date,
                query_start_ts,
                query_end_ts,
                previous_query_end_ts,
            )
            && entry.daily.contains_key(&date)
        {
            continue;
        }
        stat.date = date.clone();
        entry.daily.insert(date, stat);
    }
    // Session rows are evidence, not a replaceable aggregate snapshot. Keep
    // rows absent from this response and let merge_session_usage mark conflicts.
    for (_, session) in fetched.sessions {
        if let (Some(started),Some(completed),Some(end))=(read_started_at_ts.checked_mul(1000),completed_at_ts.checked_mul(1000),query_end_ts.checked_mul(1000)) {
            if fetched.source_complete && started>0 && completed>=started && end>=started {
                let source=SessionObservation {read_started_at_ms:started,completed_at_ms:completed,query_end_ms:end,complete:true,row:Some(session.clone())};
                let newer=entry.session_observations.get(&session.session_id).map_or(true,|old|started>=old.read_started_at_ms && completed>=old.completed_at_ms);
                if newer {entry.session_observations.insert(session.session_id.clone(),source);}
            }
        }
        merge_session_usage(&mut entry.session_usage, session);
    }
    entry.last_fetch_end_ts = Some(entry.last_fetch_end_ts.unwrap_or(i64::MIN).max(query_end_ts));
    cache.fetched_at = Some(cache.fetched_at.unwrap_or(i64::MIN).max(completed_at_ts));
}

fn ingest_usage_row(
    row: &Value,
    daily: &mut BTreeMap<String, UsageDayStat>,
    sessions: &mut BTreeMap<String, CachedSessionUsage>,
) {
    let ts = row.get("usage_time").and_then(Value::as_i64).unwrap_or(0);
    if ts <= 0 {
        return;
    }
    if let Some(session) = parse_session_usage_row(row) {
        merge_session_usage(sessions, session);
    }
    let Some(date) = local_date_of(ts) else {
        return;
    };
    let credits = row
        .get("credits_float")
        .and_then(Value::as_f64)
        .or_else(|| row.get("amount_float").and_then(Value::as_f64))
        .unwrap_or(0.0);
    let model = row
        .get("model_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string();
    let day = daily.entry(date.clone()).or_default();
    day.date = date;
    day.credits += credits;
    day.sessions += 1;
    *day.models.entry(model).or_insert(0.0) += credits;
    if let Some(extra) = row.get("extra_info") {
        day.input_tokens += extra.get("input_token").and_then(Value::as_u64).unwrap_or(0);
        day.output_tokens += extra.get("output_token").and_then(Value::as_u64).unwrap_or(0);
        day.cache_read_tokens += extra
            .get("cache_read_token")
            .and_then(Value::as_u64)
            .unwrap_or(0);
    }
}

/// 单分块（≤30 天）分页拉取并聚合进 agg；每次实际 HTTP 发起前检查停止信号。
fn fetch_chunk_with_pages<C, F>(
    start_ts: i64,
    end_ts: i64,
    cancelled: &C,
    request_page: &mut F,
    agg: &mut BTreeMap<String, UsageDayStat>,
    sessions: &mut BTreeMap<String, CachedSessionUsage>,
) -> Result<bool, String>
where
    C: Fn() -> bool,
    F: FnMut(i64, i64, u32) -> Result<Value, String>,
{
    let mut page: u32 = 1;
    let mut got: usize = 0;
    let mut total: Option<usize> = None;
    let mut complete=true;

    loop {
        if cancelled() {
            return Err("usage refresh cancelled".into());
        }
        let resp = request_page(start_ts, end_ts, page)?;
        let page_total=resp.get("total").and_then(Value::as_u64).and_then(|value|usize::try_from(value).ok());
        if page_total.is_none() || (total.is_some() && page_total!=total) {complete=false;}
        if total.is_none() {total=page_total;}
        if resp.get("user_usage_group_by_sessions").and_then(Value::as_array).is_none() {complete=false;}
        let arr = resp
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            return Ok(complete && total==Some(got));
        }
        for row in &arr {
            ingest_usage_row(row, agg, sessions);
        }
        got=got.checked_add(arr.len()).ok_or("usage pagination count overflow")?;
        let total_n = total.unwrap_or(0);
        // 终止条件：已取满 total / 本页不满页大小（服务端截断页）/ 超过安全页数上限
        if got >= total_n || (arr.len() as u32) < PAGE_SIZE || page >= MAX_PAGES {
            return Ok(complete && total==Some(got));
        }
        page += 1;
    }
}

/// 拉取全部账号的积分消耗历史（按本地日聚合），落盘缓存供查询展示。
/// fresh=false：纯缓存读取（零网络）；fresh=true：增量拉取（无缓存账号全量近一年，
/// 已有账号从上次拉取日 00:00 起重拉并替换该日期及之后的聚合）。
/// async 派发：逐账号串行分页网络请求（每请求最长 60s），同步命令会冻住 UI。
#[tauri::command(async)]
pub fn usage_history_fetch(
    state: State<AppState>,
    fresh: Option<bool>,
) -> Result<UsageHistoryResult, String> {
    let fresh = fresh.unwrap_or(false);
    let accounts = crate::vault::load_accounts(&state);
    let mut cache: CacheFile = {
        let _cache_guard = usage_cache_merge_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::fs_utils::read_json(&cache_path(&state))
    };

    // 纯缓存读取（零网络；尚未拉取过的账号如实提示）
    if !fresh {
        let mut out = Vec::new();
        for a in &accounts.accounts {
            let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
                continue;
            };
            match cache.accounts.get(&uid) {
                Some(c) => out.push(account_summary(c.name.clone(), uid, &c.daily)),
                None => out.push(UsageHistoryAccount {
                    user_id: uid,
                    name: a.name.clone(),
                    ok: false,
                    error: Some("尚未拉取消耗明细，点击「更新消耗明细」拉取".into()),
                    ..Default::default()
                }),
            }
        }
        return Ok(UsageHistoryResult {
            fetched_at: cache.fetched_at.unwrap_or(0),
            cached: true,
            accounts: out,
        });
    }

    // Refresh each account through the same account guard and write-time cache
    // merge used by the background Core poller.
    let mut errors: BTreeMap<String, String> = BTreeMap::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        if a.jwt.trim().is_empty() {
            // 占位账号（无 JWT）：保留既有缓存，不发起请求
            continue;
        }
        if let Err(error) = refresh_manual_account_with(
            &state.data_dir,
            &uid,
            &name,
            &a.jwt,
            fetch_account_usage,
        ) {
            // 拉取失败：保留旧缓存，错误在结果中注明
            errors.insert(uid, error);
        }
    }

    cache = {
        let _cache_guard = usage_cache_merge_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::fs_utils::read_json(&cache_path(&state))
    };

    let mut out = Vec::new();
    for a in &accounts.accounts {
        let Some(uid) = a.user_id.clone().filter(|u| !u.is_empty()) else {
            continue;
        };
        let name = a.name.clone();
        match cache.accounts.get(&uid) {
            Some(c) => {
                let mut acc = account_summary(c.name.clone(), uid.clone(), &c.daily);
                if let Some(e) = errors.get(&uid) {
                    acc.error = Some(format!("本次更新失败（展示已有缓存）：{e}"));
                }
                out.push(acc);
            }
            None => out.push(UsageHistoryAccount {
                error: errors.get(&uid).cloned(),
                user_id: uid,
                name,
                ok: false,
                ..Default::default()
            }),
        }
    }
    Ok(UsageHistoryResult {
        fetched_at: cache.fetched_at.unwrap_or(0),
        cached: false,
        accounts: out,
    })
}

fn refresh_manual_account_with<F>(
    data_dir: &std::path::Path,
    uid: &str,
    name: &str,
    jwt: &str,
    fetch: F,
) -> Result<(), String>
where
    F: FnMut(&str, i64, i64) -> Result<FetchedAccountUsage, String>,
{
    refresh_manual_account_with_clock(
        data_dir,
        uid,
        name,
        jwt,
        || chrono::Local::now().timestamp(),
        fetch,
    )
}

fn refresh_manual_account_with_clock<C, F>(
    data_dir: &std::path::Path,
    uid: &str,
    name: &str,
    jwt: &str,
    clock: C,
    fetch: F,
) -> Result<(), String>
where
    C: Fn() -> i64,
    F: FnMut(&str, i64, i64) -> Result<FetchedAccountUsage, String>,
{
    refresh_manual_account_with_clock_and_matcher(
        data_dir,
        uid,
        name,
        jwt,
        clock,
        fetch,
        crate::api_server::bridge_billing::match_core_usage_sessions,
    )
}

fn refresh_manual_account_with_clock_and_matcher<C, F, M>(
    data_dir: &std::path::Path,
    uid: &str,
    name: &str,
    jwt: &str,
    clock: C,
    mut fetch: F,
    matcher: M,
) -> Result<(), String>
where
    C: Fn() -> i64,
    F: FnMut(&str, i64, i64) -> Result<FetchedAccountUsage, String>,
    M: Fn(&std::path::Path, &[(String, String)]) -> Result<CoreUsageSessionMatches, String>,
{
    let account_lock = account_refresh_lock(data_dir, uid);
    let _account_guard = account_lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    // A queued refresh may have waited behind the same account's background
    // poll. Capture its time and cache window only after taking the account lock.
    let now_ts = clock();
    let cache_path = data_dir.join("data").join("usage_history.json");
    let (start_ts, start_date, full_sync, mut attribution_session_ids) = {
        let _cache_guard = usage_cache_merge_lock()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let cache = read_usage_cache_for_merge(&cache_path)?;
        let account = cache.accounts.get(uid);
        let full_sync = account.and_then(|account| account.manual_full_sync_at).is_none();
        let session_ids = account
            .map(|account| account.session_usage.keys().cloned().collect::<HashSet<_>>())
            .unwrap_or_default();
        if full_sync {
            let start = now_ts - FULL_PULL_DAYS * 86400;
            (start, local_date_of(start).unwrap_or_default(), true, session_ids)
        } else {
            match account.and_then(|account| account.last_fetch_end_ts) {
                Some(last_end) => {
                    let date = match (local_date_of(last_end), local_date_of(now_ts)) {
                        // Background polls may query five minutes ahead. If
                        // that crosses midnight, a manual refresh before
                        // midnight must still cover the current day instead
                        // of producing a start later than its end.
                        (Some(last_date), Some(now_date)) if last_date > now_date => now_date,
                        (Some(last_date), _) => last_date,
                        (_, Some(now_date)) => now_date,
                        _ => String::new(),
                    };
                    let start = local_midnight_ts(&date).unwrap_or(now_ts - 86400);
                    (start, date, false, session_ids)
                }
                None => {
                    let start = now_ts - FULL_PULL_DAYS * 86400;
                    (start, local_date_of(start).unwrap_or_default(), true, session_ids)
                }
            }
        }
    };
    let query_end_ts = now_ts;
    let fetched = fetch(jwt, start_ts, query_end_ts)?;
    let completed_at_ts = clock();
    let end_date = local_date_of(query_end_ts).unwrap_or_else(|| start_date.clone());
    attribution_session_ids.extend(fetched.sessions.keys().cloned());
    let attribution_pairs: Vec<(String, String)> = attribution_session_ids.into_iter()
        .map(|session_id| (uid.to_string(), session_id))
        .collect();
    let attribution = if attribution_pairs.is_empty() {
        None
    } else {
        matcher(data_dir, &attribution_pairs)
            .ok()
            .map(|matches| (attribution_pairs, matches))
    };

    let _cache_guard = usage_cache_merge_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut cache = read_usage_cache_for_merge(&cache_path)?;
    merge_fetched_account(
        &mut cache,
        uid,
        name,
        &start_date,
        &end_date,
        start_ts,
        query_end_ts,
        completed_at_ts,
        now_ts,
        fetched,
    );
    if full_sync {
        let account = cache.accounts.get_mut(uid).expect("merged account exists");
        account.manual_full_sync_at = Some(
            account.manual_full_sync_at.unwrap_or(i64::MIN).max(completed_at_ts),
        );
    }
    if let Some((pairs, matches)) = attribution {
        apply_core_usage_matches(&mut cache, &pairs, &matches);
    }
    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|_| "usage history cache directory unavailable".to_string())?;
    }
    crate::fs_utils::write_json(&cache_path, &cache)
        .map_err(|_| "usage history cache write failed".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回归：聚合产物的 date 字段必须写入（此前仅存于映射键，响应 daily.date 恒为空串，
    /// 导致前端区间匹配全部失败、消耗折线/模型柱状图不显示）
    #[test]
    fn aggregated_day_stat_carries_date() {
        let ts = 1_789_009_361i64; // 2026-09-10（+8）
        let Some(dt) = chrono::DateTime::from_timestamp(ts, 0) else {
            panic!("timestamp out of range");
        };
        let date = dt.with_timezone(&chrono::Local).format("%Y-%m-%d").to_string();
        let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
        let e = agg.entry(date.clone()).or_default();
        e.date = date.clone();
        e.credits += 1.5;
        // account_summary 从映射键回填 date（含旧缓存 date 字段为空的条目）
        let mut legacy = BTreeMap::new();
        legacy.insert("2026-09-11".to_string(), UsageDayStat::default());
        let acc = account_summary("n".into(), "u".into(), &legacy);
        assert_eq!(acc.daily.len(), 1);
        assert_eq!(acc.daily[0].date, "2026-09-11");
        assert_eq!(agg[&date].date, date);
    }

    #[test]
    fn session_usage_parser_preserves_upstream_session_and_decimal_credit() {
        let row = json!({
            "session_id": "trae-session-42",
            "usage_time": 1_789_009_361i64,
            "credits_float": 1.25,
            "model_name": "seedance",
        });

        let parsed = parse_session_usage_row(&row).expect("stable session row");
        assert_eq!(parsed.session_id, "trae-session-42");
        assert_eq!(parsed.credits_float.as_deref(), Some("1.25"));
        assert_eq!(parsed.model_name, "seedance");
    }

    #[test]
    fn session_usage_parser_does_not_invent_an_id_for_anonymous_rows() {
        let row = json!({
            "usage_time": 1_789_009_361i64,
            "credits_float": 1.25,
            "model_name": "seedance",
        });

        assert!(parse_session_usage_row(&row).is_none());
    }

    #[test]
    fn cached_account_round_trips_exact_session_usage_candidates() {
        let parsed = parse_session_usage_row(&json!({
            "session_id": "session-persist-1",
            "usage_time": 1_789_009_361i64,
            "credits_float": "0.125000",
            "model_name": "seedance",
        }))
        .unwrap();
        let mut account = CachedAccount::default();
        account.session_usage.insert(parsed.session_id.clone(), parsed);
        let mut cache = CacheFile::default();
        cache.accounts.insert("internal-account".into(), account);

        let saved = serde_json::to_vec(&cache).unwrap();
        let reopened: CacheFile = serde_json::from_slice(&saved).unwrap();
        let restored = &reopened.accounts["internal-account"].session_usage["session-persist-1"];
        assert_eq!(restored.credits_float.as_deref(), Some("0.125000"));
        assert!(!restored.ambiguous);
    }

    #[test]
    fn repeated_session_rows_are_idempotent_but_conflicting_amounts_stay_ambiguous() {
        let first = parse_session_usage_row(&json!({
            "session_id": "session-dup-1",
            "usage_time": 1_789_009_361i64,
            "credits_float": "0.125000",
        }))
        .unwrap();
        let same = first.clone();
        let mut rows = BTreeMap::new();
        merge_session_usage(&mut rows, first);
        merge_session_usage(&mut rows, same);
        assert!(!rows["session-dup-1"].ambiguous);

        let changed = parse_session_usage_row(&json!({
            "session_id": "session-dup-1",
            "usage_time": 1_789_009_361i64,
            "credits_float": "0.250000",
        }))
        .unwrap();
        merge_session_usage(&mut rows, changed);
        assert!(rows["session-dup-1"].ambiguous);
    }

    #[test]
    fn usage_row_ingestion_keeps_session_candidate_and_existing_daily_total() {
        let row = json!({
            "session_id": "session-ingest-1",
            "usage_time": 1_789_009_361i64,
            "credits_float": 0.3,
            "model_name": "seedance",
        });
        let mut daily = BTreeMap::new();
        let mut sessions = BTreeMap::new();

        ingest_usage_row(&row, &mut daily, &mut sessions);

        let session = &sessions["session-ingest-1"];
        assert_eq!(session.credits_float.as_deref(), Some("0.3"));
        assert_eq!(daily[&session.date].sessions, 1);
        assert!((daily[&session.date].credits - 0.3).abs() < f64::EPSILON);
    }

    #[test]
    fn pending_core_poll_fetches_only_linked_accounts_and_persists_candidates() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-pending-usage-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let pending = vec![("uid-linked".to_string(), now_ts * 1000)];
        let credentials = vec![
            ("uid-linked".to_string(), "Linked".to_string(), "fixture-jwt".to_string()),
            ("uid-unrelated".to_string(), "Other".to_string(), "other-jwt".to_string()),
        ];
        let mut calls = Vec::new();
        let fetched = |session_id: &str, timestamp: i64| {
            let session = CachedSessionUsage {
                session_id: session_id.into(),
                usage_time: timestamp,
                date: local_date_of(timestamp).unwrap(),
                model_name: "seedance".into(),
                credits_float: Some("0.625000".into()),
                ambiguous: false,
                core_request_id: None,
                core_key_id: None,
                core_attribution_ambiguous: false,
            };
            FetchedAccountUsage {
                sessions: BTreeMap::from([(session_id.into(), session)]),
                ..Default::default()
            }
        };

        let count = refresh_pending_core_usage_with(
            &data_dir,
            &pending,
            &credentials,
            now_ts,
            |uid, jwt, _start, _end| {
                calls.push((uid.to_string(), jwt.to_string()));
                Ok(fetched("session-linked", now_ts))
            },
        ).unwrap();

        assert_eq!(count, 1);
        assert_eq!(calls, vec![("uid-linked".into(), "fixture-jwt".into())]);
        let cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
        assert_eq!(cache.accounts["uid-linked"].session_usage["session-linked"].credits_float.as_deref(), Some("0.625000"));
        assert!(!cache.accounts.contains_key("uid-unrelated"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn pending_core_poll_includes_session_from_upstream_clock_three_minutes_ahead() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-clock-skew-usage-test-{}-{}", std::process::id(), rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let pending = vec![("uid-a".to_string(), now_ts * 1000)];
        let credentials = vec![("uid-a".to_string(), "A".to_string(), "jwt-a".to_string())];
        let session_ts = now_ts + 180;
        let mut seen_end = 0;
        let _ = refresh_pending_core_usage_with(
            &data_dir, &pending, &credentials, now_ts,
            |_, _, _, end| {
                seen_end = end;
                let mut result = FetchedAccountUsage::default();
                if end >= session_ts {
                    result.sessions.insert("session-a".into(), CachedSessionUsage {
                        session_id: "session-a".into(), usage_time: session_ts,
                        date: local_date_of(session_ts).unwrap(), model_name: "DeepSeek".into(),
                        credits_float: Some("0.050400".into()), ambiguous: false,
                        core_request_id: None, core_key_id: None,
                        core_attribution_ambiguous: false,
                    });
                }
                Ok(result)
            },
        ).unwrap();
        assert!(seen_end >= session_ts, "a clock-ahead upstream session must be in the read-only query window");
        let cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
        assert!(cache.accounts["uid-a"].session_usage.contains_key("session-a"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn pending_core_poll_does_not_query_when_there_are_no_linked_attempts() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-no-pending-usage-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let result = refresh_pending_core_usage_with(
            &data_dir,
            &[],
            &[],
            chrono::Local::now().timestamp(),
            |_, _, _, _| panic!("no upstream query is allowed without pending Core attempts"),
        ).unwrap();
        assert_eq!(result, 0);
        assert!(!data_dir.join("data").join("usage_history.json").exists());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn stopping_after_first_usage_page_skips_later_http_and_preserves_cache() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-cancel-usage-pagination-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let date = local_date_of(now_ts).unwrap();
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        let mut cached = CacheFile::default();
        cached.fetched_at = Some(now_ts - 100);
        let account = cached.accounts.entry("uid-a".into()).or_default();
        account.last_fetch_end_ts = Some(now_ts - 100);
        account.daily.insert(date.clone(), UsageDayStat {
            date, credits: 7.0, sessions: 1, ..Default::default()
        });
        crate::fs_utils::write_json(&cache_path, &cached).unwrap();
        let original_bytes = std::fs::read(&cache_path).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (page_started_tx, page_started_rx) = std::sync::mpsc::channel();
        let (release_page_tx, release_page_rx) = std::sync::mpsc::channel();
        let page_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let worker_stop = stop.clone();
        let worker_calls = page_calls.clone();
        let worker_dir = data_dir.clone();
        let worker = std::thread::spawn(move || {
            refresh_pending_core_usage_with_stop_and_clock(
                &worker_dir,
                &[("uid-a".to_string(), now_ts * 1000)],
                &[("uid-a".to_string(), "A".to_string(), "fixture-jwt".to_string())],
                || now_ts,
                &worker_stop,
                |_, _, start_ts, end_ts, stop| {
                    fetch_account_usage_with_pages(
                        start_ts,
                        end_ts,
                        || stop.load(Ordering::Acquire),
                        |_, _, page| {
                            assert_eq!(page, 1, "no second page request may start after stop");
                            worker_calls.fetch_add(1, Ordering::SeqCst);
                            page_started_tx.send(()).unwrap();
                            release_page_rx.recv().unwrap();
                            let rows = (0..PAGE_SIZE).map(|index| json!({
                                "session_id": format!("partial-session-{index}"),
                                "usage_time": now_ts,
                                "credits_float": "1.000000",
                                "model_name": "fixture-model",
                            })).collect::<Vec<_>>();
                            Ok(json!({
                                "total": PAGE_SIZE * 2,
                                "user_usage_group_by_sessions": rows,
                            }))
                        },
                    )
                },
            )
        });

        page_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        stop.store(true, Ordering::Release);
        release_page_tx.send(()).unwrap();
        let result = worker.join().unwrap();

        assert!(result.is_err(), "an interrupted multi-page window is not successful");
        assert_eq!(page_calls.load(Ordering::SeqCst), 1);
        assert_eq!(std::fs::read(&cache_path).unwrap(), original_bytes,
            "partial first-page evidence must not replace the prior cache");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn concurrent_pending_refreshes_merge_the_latest_cache_instead_of_overwriting_other_accounts() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-concurrent-usage-merge-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let fetch_barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let now_ts = chrono::Local::now().timestamp();
        let mut workers = Vec::new();

        for uid in ["uid-a", "uid-b"] {
            let data_dir = data_dir.clone();
            let fetch_barrier = fetch_barrier.clone();
            let uid = uid.to_string();
            workers.push(std::thread::spawn(move || {
                let pending = vec![(uid.clone(), now_ts * 1000)];
                let credentials = vec![(uid.clone(), uid.clone(), "fixture-jwt".to_string())];
                refresh_pending_core_usage_with(
                    &data_dir,
                    &pending,
                    &credentials,
                    now_ts,
                    |_, _, _, _| {
                        // Both calls have already read the same initial cache before
                        // either fetch can complete.
                        fetch_barrier.wait();
                        Ok(FetchedAccountUsage::default())
                    },
                )
                .unwrap()
            }));
        }

        for worker in workers {
            assert_eq!(worker.join().unwrap(), 1);
        }
        let cache: CacheFile = crate::fs_utils::read_json(
            &data_dir.join("data").join("usage_history.json"),
        );
        assert!(cache.accounts.contains_key("uid-a"));
        assert!(cache.accounts.contains_key("uid-b"));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn slow_core_attribution_does_not_block_another_accounts_cache_merge() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-slow-usage-attribution-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let (matcher_started_tx, matcher_started_rx) = std::sync::mpsc::channel();
        let (release_matcher_tx, release_matcher_rx) = std::sync::mpsc::channel();
        let (account_b_attempting_tx, account_b_attempting_rx) = std::sync::mpsc::channel();
        let (account_b_done_tx, account_b_done_rx) = std::sync::mpsc::channel();

        let account_a = {
            let data_dir = data_dir.clone();
            std::thread::spawn(move || {
                let stop = std::sync::atomic::AtomicBool::new(false);
                refresh_pending_core_usage_with_stop_and_clock_and_matcher(
                    &data_dir,
                    &[("uid-a".to_string(), now_ts * 1000)],
                    &[("uid-a".to_string(), "A".to_string(), "fixture-jwt".to_string())],
                    || now_ts,
                    &stop,
                    |_, _, _, _, _| {
                        let mut fetched = FetchedAccountUsage::default();
                        fetched.sessions.insert(
                            "session-a".to_string(),
                            CachedSessionUsage {
                                session_id: "session-a".to_string(),
                                usage_time: now_ts,
                                date: local_date_of(now_ts).unwrap(),
                                model_name: "fixture-model".to_string(),
                                credits_float: Some("1.000000".to_string()),
                                ..Default::default()
                            },
                        );
                        Ok(fetched)
                    },
                    move |_, pairs| {
                        assert_eq!(pairs, &[("uid-a".to_string(), "session-a".to_string())]);
                        matcher_started_tx.send(()).unwrap();
                        release_matcher_rx.recv().unwrap();
                        Ok(HashMap::new())
                    },
                )
            })
        };
        matcher_started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();

        let account_b = {
            let data_dir = data_dir.clone();
            std::thread::spawn(move || {
                let stop = std::sync::atomic::AtomicBool::new(false);
                account_b_attempting_tx.send(()).unwrap();
                let result = refresh_pending_core_usage_with_stop_and_clock_and_matcher(
                    &data_dir,
                    &[("uid-b".to_string(), now_ts * 1000)],
                    &[("uid-b".to_string(), "B".to_string(), "fixture-jwt".to_string())],
                    || now_ts,
                    &stop,
                    |_, _, _, _, _| Ok(FetchedAccountUsage::default()),
                    |_, _| Ok(HashMap::new()),
                );
                account_b_done_tx.send(result).unwrap();
            })
        };
        account_b_attempting_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let completed_while_attribution_blocked = account_b_done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .ok();
        let completed_before_release = completed_while_attribution_blocked.is_some();

        release_matcher_tx.send(()).unwrap();
        assert_eq!(account_a.join().unwrap().unwrap(), 1);
        let account_b_result = match completed_while_attribution_blocked {
            Some(result) => result,
            None => account_b_done_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap(),
        };
        assert_eq!(account_b_result.unwrap(), 1);
        account_b.join().unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(
            &data_dir.join("data").join("usage_history.json"),
        );
        assert!(cache.accounts.contains_key("uid-a"));
        assert!(cache.accounts.contains_key("uid-b"));
        let _ = std::fs::remove_dir_all(data_dir);
        assert!(
            completed_before_release,
            "another account must merge while the first account's injected attribution lookup is blocked"
        );
    }

    #[test]
    fn overlapping_pending_refreshes_for_one_account_are_serialized() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-same-account-usage-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let active = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let maximum = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = std::sync::Arc::new(std::sync::Mutex::new(release_rx));

        let first = {
            let data_dir = data_dir.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let entered_tx = entered_tx.clone();
            let release_rx = release_rx.clone();
            std::thread::spawn(move || {
                let pending = vec![("uid-a".to_string(), now_ts * 1000)];
                let credentials = vec![("uid-a".to_string(), "A".to_string(), "jwt".to_string())];
                refresh_pending_core_usage_with(
                    &data_dir,
                    &pending,
                    &credentials,
                    now_ts,
                    |_, _, _, _| {
                        let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                        entered_tx.send(()).unwrap();
                        release_rx.lock().unwrap().recv().unwrap();
                        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(FetchedAccountUsage::default())
                    },
                )
            })
        };
        entered_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();

        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let second = {
            let data_dir = data_dir.clone();
            let active = active.clone();
            let maximum = maximum.clone();
            let entered_tx = entered_tx.clone();
            std::thread::spawn(move || {
                second_started_tx.send(()).unwrap();
                let pending = vec![("uid-a".to_string(), now_ts * 1000)];
                let credentials = vec![("uid-a".to_string(), "A".to_string(), "jwt".to_string())];
                refresh_pending_core_usage_with(
                    &data_dir,
                    &pending,
                    &credentials,
                    now_ts,
                    |_, _, _, _| {
                        let current = active.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
                        maximum.fetch_max(current, std::sync::atomic::Ordering::SeqCst);
                        entered_tx.send(()).unwrap();
                        active.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
                        Ok(FetchedAccountUsage::default())
                    },
                )
            })
        };
        second_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let entered_before_release = entered_rx
            .recv_timeout(std::time::Duration::from_millis(150))
            .is_ok();
        release_tx.send(()).unwrap();
        first.join().unwrap().unwrap();
        second.join().unwrap().unwrap();

        assert!(!entered_before_release, "same-account network reads must not overlap");
        assert_eq!(maximum.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn conflicting_rows_for_an_existing_session_remain_ambiguous_after_refresh() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-session-conflict-usage-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let date = local_date_of(now_ts).unwrap();
        let session_id = "session-conflict";
        let mut cache = CacheFile::default();
        cache.accounts.entry("uid-a".into()).or_default().session_usage.insert(
            session_id.into(),
            CachedSessionUsage {
                session_id: session_id.into(),
                usage_time: now_ts,
                date: date.clone(),
                model_name: "Seedance".into(),
                credits_float: Some("1.000000".into()),
                ambiguous: false,
                core_request_id: None,
                core_key_id: None,
                core_attribution_ambiguous: false,
            },
        );
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        crate::fs_utils::write_json(&cache_path, &cache).unwrap();

        let pending = vec![("uid-a".to_string(), now_ts * 1000)];
        let credentials = vec![("uid-a".to_string(), "A".to_string(), "jwt".to_string())];
        refresh_pending_core_usage_with(
            &data_dir,
            &pending,
            &credentials,
            now_ts,
            |_, _, _, _| {
                let mut fetched = FetchedAccountUsage::default();
                fetched.sessions.insert(session_id.into(), CachedSessionUsage {
                    session_id: session_id.into(),
                    usage_time: now_ts,
                    date: date.clone(),
                    model_name: "Seedance".into(),
                    credits_float: Some("2.000000".into()),
                    ambiguous: false,
                    core_request_id: None,
                    core_key_id: None,
                    core_attribution_ambiguous: false,
                });
                Ok(fetched)
            },
        ).unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(&cache_path);
        assert!(cache.accounts["uid-a"].session_usage[session_id].ambiguous);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn all_failed_pending_refreshes_preserve_the_previous_success_timestamp() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-failed-usage-timestamp-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        let mut cache = CacheFile::default();
        cache.fetched_at = Some(1234);
        crate::fs_utils::write_json(&cache_path, &cache).unwrap();

        let now_ts = chrono::Local::now().timestamp();
        let pending = vec![("uid-a".to_string(), now_ts * 1000)];
        let credentials = vec![("uid-a".to_string(), "A".to_string(), "jwt".to_string())];
        let result = refresh_pending_core_usage_with(
            &data_dir,
            &pending,
            &credentials,
            now_ts,
            |_, _, _, _| Err("fixture source unavailable".into()),
        );

        assert!(result.is_err());
        let cache: CacheFile = crate::fs_utils::read_json(&cache_path);
        assert_eq!(cache.fetched_at, Some(1234));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn manual_first_full_window_waits_for_background_refresh_and_still_covers_one_year() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-manual-full-after-background-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let (background_started_tx, background_started_rx) = std::sync::mpsc::channel();
        let (release_background_tx, release_background_rx) = std::sync::mpsc::channel();
        let background_dir = data_dir.clone();
        let background = std::thread::spawn(move || {
            refresh_pending_core_usage_with(
                &background_dir,
                &[("uid-a".to_string(), now_ts * 1000)],
                &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
                now_ts,
                |_, _, _, _| {
                    background_started_tx.send(()).unwrap();
                    release_background_rx.recv().unwrap();
                    Ok(FetchedAccountUsage::default())
                },
            ).unwrap()
        });
        background_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();

        let (manual_started_tx, manual_started_rx) = std::sync::mpsc::channel();
        let (manual_start_tx, manual_start_rx) = std::sync::mpsc::channel();
        let manual_dir = data_dir.clone();
        let manual = std::thread::spawn(move || {
            manual_started_tx.send(()).unwrap();
            refresh_manual_account_with(
                &manual_dir,
                "uid-a",
                "A",
                "jwt",
                |_, start_ts, _| {
                    manual_start_tx.send(start_ts).unwrap();
                    Ok(FetchedAccountUsage::default())
                },
            ).unwrap()
        });
        manual_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        release_background_tx.send(()).unwrap();
        background.join().unwrap();
        let manual_start_ts = manual_start_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        manual.join().unwrap();

        assert!(now_ts - manual_start_ts >= 364 * 86400,
            "first manual refresh after a background cache must request the full year");
        let cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
        assert!(cache.accounts["uid-a"].manual_full_sync_at.is_some());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn manual_increment_after_background_crosses_midnight_covers_current_day() {
        use std::cell::Cell;

        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-manual-midnight-window-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let today = chrono::Local::now().date_naive();
        let today_start = local_midnight_ts(&today.format("%Y-%m-%d").to_string()).unwrap();
        let tomorrow = today.succ_opt().unwrap();
        let tomorrow_start = local_midnight_ts(&tomorrow.format("%Y-%m-%d").to_string()).unwrap();
        let background_now = tomorrow_start - 2 * 60;
        let background_query_end = background_now + 5 * 60;
        let manual_now = background_now + 60;

        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        let mut cache = CacheFile::default();
        let account = cache.accounts.entry("uid-a".into()).or_default();
        account.last_fetch_end_ts = Some(background_query_end);
        account.manual_full_sync_at = Some(background_now);
        crate::fs_utils::write_json(&cache_path, &cache).unwrap();

        let observed_window = Cell::new(None);
        refresh_manual_account_with_clock(
            &data_dir,
            "uid-a",
            "A",
            "jwt",
            || manual_now,
            |_, start_ts, end_ts| {
                observed_window.set(Some((start_ts, end_ts)));
                Ok(FetchedAccountUsage::default())
            },
        )
        .unwrap();

        assert_eq!(
            observed_window.get(),
            Some((today_start, manual_now)),
            "a 23:59 manual refresh after a 23:58 background query must fetch today's elapsed window, not a reversed next-day range"
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn late_old_background_refresh_preserves_manual_history_and_success_times() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-manual-background-window-order-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let manual_started_at = chrono::Local::now().timestamp();
        let stale_background_now = manual_started_at - 30 * 86400;
        let (manual_fetch_started_tx, manual_fetch_started_rx) = std::sync::mpsc::channel();
        let (manual_dates_tx, manual_dates_rx) = std::sync::mpsc::channel();
        let (release_manual_tx, release_manual_rx) = std::sync::mpsc::channel();
        let manual_dir = data_dir.clone();
        let manual = std::thread::spawn(move || {
            refresh_manual_account_with(
                &manual_dir,
                "uid-a",
                "A",
                "jwt",
                move |_, start_ts, end_ts| {
                    manual_fetch_started_tx.send(()).unwrap();
                    release_manual_rx.recv().unwrap();
                    let history_date = local_date_of(start_ts + 86400).unwrap();
                    let current_date = local_date_of(end_ts).unwrap();
                    manual_dates_tx.send((history_date.clone(), current_date.clone())).unwrap();
                    let mut fetched = FetchedAccountUsage::default();
                    for (date, credits) in [(history_date, 1.0), (current_date, 2.0)] {
                        fetched.daily.insert(date.clone(), UsageDayStat {
                            date, credits, sessions: 1, ..Default::default()
                        });
                    }
                    Ok(fetched)
                },
            ).unwrap()
        });
        manual_fetch_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();

        let (background_attempting_tx, background_attempting_rx) = std::sync::mpsc::channel();
        let background_dir = data_dir.clone();
        let background = std::thread::spawn(move || {
            background_attempting_tx.send(()).unwrap();
            refresh_pending_core_usage_with_clock(
                &background_dir,
                &[("uid-a".to_string(), stale_background_now * 1000)],
                &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
                || stale_background_now,
                |_, _, _, _| {
                    let date = local_date_of(stale_background_now).unwrap();
                    let mut fetched = FetchedAccountUsage::default();
                    fetched.daily.insert(date.clone(), UsageDayStat {
                        date, credits: 3.0, sessions: 1, ..Default::default()
                    });
                    Ok(fetched)
                },
            ).unwrap()
        });
        background_attempting_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        release_manual_tx.send(()).unwrap();
        manual.join().unwrap();
        background.join().unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
        let account = &cache.accounts["uid-a"];
        let (history_date, current_date) = manual_dates_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        let stale_date = local_date_of(stale_background_now).unwrap();
        assert_eq!(account.daily[&current_date].credits, 2.0);
        assert!(account.daily.contains_key(&history_date), "manual year history must survive the later short poll");
        assert_eq!(account.daily[&stale_date].credits, 3.0);
        assert!(account.last_fetch_end_ts.unwrap() >= manual_started_at);
        assert!(cache.fetched_at.unwrap() >= manual_started_at);
        assert!(account.manual_full_sync_at.is_some());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn older_refresh_preserves_days_after_its_query_end_and_never_moves_success_times_backwards() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-usage-monotonic-window-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let now_ts = chrono::Local::now().timestamp();
        let now_date = local_date_of(now_ts).unwrap();
        let future_date = local_date_of(now_ts + 2 * 86400).unwrap();
        let later_success = now_ts + 2 * 86400;
        let mut cache = CacheFile::default();
        cache.fetched_at = Some(later_success);
        let account = cache.accounts.entry("uid-a".into()).or_default();
        account.last_fetch_end_ts = Some(later_success);
        account.daily.insert(future_date.clone(), UsageDayStat {
            date: future_date.clone(), credits: 9.0, sessions: 1,
            ..Default::default()
        });
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        crate::fs_utils::write_json(&cache_path, &cache).unwrap();

        refresh_pending_core_usage_with(
            &data_dir,
            &[("uid-a".to_string(), now_ts * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            now_ts,
            |_, _, _, _| {
                let mut fetched = FetchedAccountUsage::default();
                fetched.daily.insert(now_date.clone(), UsageDayStat {
                    date: now_date.clone(), credits: 2.0, sessions: 1,
                    ..Default::default()
                });
                Ok(fetched)
            },
        ).unwrap();

        let merged: CacheFile = crate::fs_utils::read_json(&cache_path);
        assert!(merged.accounts["uid-a"].daily.contains_key(&future_date));
        assert_eq!(merged.accounts["uid-a"].last_fetch_end_ts, Some(later_success));
        assert_eq!(merged.fetched_at, Some(later_success));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn query_end_timestamp_does_not_advance_to_completion_across_midnight() {
        use std::cell::Cell;

        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-query-end-across-midnight-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let query_date = chrono::Local::now().date_naive().pred_opt().unwrap();
        let current_day = query_date.succ_opt().unwrap();
        let current_midnight = local_midnight_ts(&current_day.format("%Y-%m-%d").to_string()).unwrap();
        let query_now = current_midnight - 20 * 60;
        let query_end = query_now + 5 * 60;
        let completed_at = current_midnight + 10 * 60;
        assert_ne!(local_date_of(query_end), local_date_of(completed_at));
        let calls = Cell::new(0);
        let stop = std::sync::atomic::AtomicBool::new(false);

        refresh_pending_core_usage_with_stop_and_clock(
            &data_dir,
            &[("uid-a".to_string(), query_now * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            || {
                if calls.replace(calls.get() + 1) == 0 { query_now } else { completed_at }
            },
            &stop,
            |_, _, _, end_ts, _| {
                assert_eq!(end_ts, query_end);
                Ok(FetchedAccountUsage::default())
            },
        ).unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(
            &data_dir.join("data").join("usage_history.json"),
        );
        assert_eq!(cache.accounts["uid-a"].last_fetch_end_ts, Some(query_end));
        assert_eq!(cache.fetched_at, Some(completed_at));

        let next_query_now = current_midnight + 3600;
        let observed_start = Cell::new(None);
        refresh_pending_core_usage_with_stop_and_clock(
            &data_dir,
            &[("uid-a".to_string(), next_query_now * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            || next_query_now,
            &stop,
            |_, _, start_ts, _, _| {
                observed_start.set(Some(start_ts));
                Ok(FetchedAccountUsage::default())
            },
        ).unwrap();
        assert_eq!(observed_start.get().map(local_date_of).flatten(), local_date_of(query_end),
            "the next range must use the query-end date, not the later completion date");
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn earlier_manual_empty_result_preserves_same_day_data_seen_by_clock_ahead_background_query() {
        use std::cell::Cell;

        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-same-day-clock-skew-merge-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let today = chrono::Local::now().date_naive();
        let today_midnight = local_midnight_ts(&today.format("%Y-%m-%d").to_string()).unwrap();
        let background_now = today_midnight + 12 * 3600;
        let background_query_end = background_now + 5 * 60;
        let background_completed_at = background_now + 30;
        let manual_now = background_now + 60;
        let manual_completed_at = background_now + 90;
        let date = local_date_of(background_query_end).unwrap();
        let fetched_date = date.clone();
        let background_calls = Cell::new(0);
        let stop = std::sync::atomic::AtomicBool::new(false);

        refresh_pending_core_usage_with_stop_and_clock(
            &data_dir,
            &[("uid-a".to_string(), background_now * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            || {
                if background_calls.replace(background_calls.get() + 1) == 0 {
                    background_now
                } else {
                    background_completed_at
                }
            },
            &stop,
            move |_, _, _, end_ts, _| {
                assert_eq!(end_ts, background_query_end);
                let mut fetched = FetchedAccountUsage::default();
                fetched.daily.insert(fetched_date.clone(), UsageDayStat {
                    date: fetched_date.clone(), credits: 7.0, sessions: 1, ..Default::default()
                });
                Ok(fetched)
            },
        ).unwrap();

        let manual_calls = Cell::new(0);
        refresh_manual_account_with_clock(
            &data_dir,
            "uid-a",
            "A",
            "jwt",
            || {
                if manual_calls.replace(manual_calls.get() + 1) == 0 {
                    manual_now
                } else {
                    manual_completed_at
                }
            },
            |_, _, end_ts| {
                assert_eq!(end_ts, manual_now);
                Ok(FetchedAccountUsage::default())
            },
        ).unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(
            &data_dir.join("data").join("usage_history.json"),
        );
        let account = &cache.accounts["uid-a"];
        assert_eq!(account.daily[&date].credits, 7.0,
            "an earlier empty partial-day query cannot erase data returned by a later query end");
        assert_eq!(account.last_fetch_end_ts, Some(background_query_end));
        assert_eq!(cache.fetched_at, Some(manual_completed_at));
        assert!(account.manual_full_sync_at.is_some());

        // Re-read the exact same query window after it has completed. The
        // upstream may have delivered a late row without advancing its end bound.
        let later_background_now = background_now;
        let later_background_query_end = background_query_end;
        let later_background_completed_at = background_now + 700;
        let later_background_calls = Cell::new(0);
        refresh_pending_core_usage_with_stop_and_clock(
            &data_dir,
            &[("uid-a".to_string(), later_background_now * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            || {
                if later_background_calls.replace(later_background_calls.get() + 1) == 0 {
                    later_background_now
                } else {
                    later_background_completed_at
                }
            },
            &stop,
            |_, _, _, end_ts, _| {
                assert_eq!(end_ts, later_background_query_end);
                let mut fetched = FetchedAccountUsage::default();
                fetched.daily.insert(date.clone(), UsageDayStat {
                    date: date.clone(), credits: 11.0, sessions: 2, ..Default::default()
                });
                fetched.sessions.insert("late-session".into(), CachedSessionUsage {
                    session_id: "late-session".into(),
                    usage_time: background_now + 100,
                    date: date.clone(),
                    model_name: "fixture-model".into(),
                    credits_float: Some("4.000000".into()),
                    ambiguous: false,
                    core_request_id: None,
                    core_key_id: None,
                    core_attribution_ambiguous: false,
                });
                Ok(fetched)
            },
        ).unwrap();

        let cache: CacheFile = crate::fs_utils::read_json(
            &data_dir.join("data").join("usage_history.json"),
        );
        let account = &cache.accounts["uid-a"];
        assert_eq!(account.daily[&date].credits, 11.0,
            "a later same-window read must incorporate delayed upstream usage even at an equal query end");
        assert_eq!(account.session_usage["late-session"].credits_float.as_deref(), Some("4.000000"),
            "a later same-range response must merge newly arrived session evidence");
        assert_eq!(account.last_fetch_end_ts, Some(later_background_query_end));
        assert_eq!(cache.fetched_at, Some(later_background_completed_at));
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn successful_fetch_does_not_replace_cache_corrupted_before_write_time_merge() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-corrupt-usage-cache-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        let original = b"{not-json";
        crate::fs_utils::write_json(&cache_path, &CacheFile::default()).unwrap();
        let fetch_called = std::sync::atomic::AtomicBool::new(false);
        let now_ts = chrono::Local::now().timestamp();

        let result = refresh_pending_core_usage_with(
            &data_dir,
            &[("uid-a".to_string(), now_ts * 1000)],
            &[("uid-a".to_string(), "A".to_string(), "jwt".to_string())],
            now_ts,
            |_, _, _, _| {
                fetch_called.store(true, std::sync::atomic::Ordering::SeqCst);
                std::fs::write(&cache_path, original).unwrap();
                Ok(FetchedAccountUsage::default())
            },
        );

        assert!(fetch_called.load(std::sync::atomic::Ordering::SeqCst));
        assert!(result.is_err(), "invalid write-time cache must stop the merge");
        assert_eq!(std::fs::read(&cache_path).unwrap(), original);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn cached_usage_sessions_map_to_exact_core_key_or_stay_ambiguous() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-session-match-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        for (request, key) in [("req-a", "key-a"), ("req-b", "key-b"),
            ("req-c", "key-c"), ("req-d", "key-d")] {
            crate::api_server::bridge_billing::persist_core_request_attribution(&data_dir, request, key).unwrap();
        }
        crate::api_server::bridge_billing::persist_core_session_attempt(&data_dir, "req-a", "uid-a", "session-same-text").unwrap();
        crate::api_server::bridge_billing::persist_core_session_attempt(&data_dir, "req-b", "uid-b", "session-same-text").unwrap();
        crate::api_server::bridge_billing::persist_core_session_attempt(&data_dir, "req-c", "uid-conflict", "session-shared").unwrap();
        crate::api_server::bridge_billing::persist_core_session_attempt(&data_dir, "req-d", "uid-conflict", "session-shared").unwrap();

        let timestamp = 1_789_009_361i64;
        let mut cache = CacheFile::default();
        for (uid, session_id) in [
            ("uid-a", "session-same-text"),
            ("uid-b", "session-same-text"),
            ("uid-conflict", "session-shared"),
            ("uid-unmatched", "session-same-text"),
        ] {
            let row = parse_session_usage_row(&json!({
                "session_id": session_id,
                "usage_time": timestamp,
                "credits_float": "0.625000",
                "model_name": "seedance",
            })).unwrap();
            cache.accounts.entry(uid.into()).or_default()
                .session_usage.insert(session_id.into(), row);
        }

        annotate_core_usage_candidates(&data_dir, &mut cache, None).unwrap();
        let key_a = &cache.accounts["uid-a"].session_usage["session-same-text"];
        let key_b = &cache.accounts["uid-b"].session_usage["session-same-text"];
        let conflicted = &cache.accounts["uid-conflict"].session_usage["session-shared"];
        let unmatched = &cache.accounts["uid-unmatched"].session_usage["session-same-text"];
        assert_eq!(key_a.core_request_id.as_deref(), Some("req-a"));
        assert_eq!(key_a.core_key_id.as_deref(), Some("key-a"));
        assert_eq!(key_b.core_request_id.as_deref(), Some("req-b"));
        assert_eq!(key_b.core_key_id.as_deref(), Some("key-b"));
        assert!(conflicted.core_attribution_ambiguous);
        assert!(conflicted.core_request_id.is_none());
        assert!(!unmatched.core_attribution_ambiguous);
        assert!(unmatched.core_request_id.is_none());
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn one_shot_receipt_candidate_requires_exact_unambiguous_request_key_and_session() {
        let data_dir = std::env::temp_dir().join(format!(
            "aiwork-one-shot-receipt-test-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        let mut cache = CacheFile::default();
        cache.accounts.entry("uid-one".into()).or_default().session_usage.insert(
            "session-one".into(),
            CachedSessionUsage {
                session_id: "session-one".into(),
                usage_time: 1_789_009_361,
                date: "2026-09-24".into(),
                model_name: "seedance".into(),
                credits_float: Some("12.345678".into()),
                ambiguous: false,
                core_request_id: Some("req-one".into()),
                core_key_id: Some("key-one".into()),
                core_attribution_ambiguous: false,
            },
        );
        let cache_path = data_dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&cache).unwrap()).unwrap();

        let candidate = core_usage_receipt_candidate(
            &data_dir, "uid-one", "session-one", "req-one", "key-one",
        ).unwrap().expect("exactly attributed session usage");
        assert_eq!(candidate.credits.to_string(), "12.345678");
        assert_eq!(candidate.session_id, "session-one");
        assert!(candidate.observed_at_ms > 0);

        assert!(core_usage_receipt_candidate(
            &data_dir, "uid-one", "session-one", "req-other", "key-one",
        ).unwrap().is_none());
        assert!(core_usage_receipt_candidate(
            &data_dir, "uid-one", "session-one", "req-one", "key-other",
        ).unwrap().is_none());
        let _ = std::fs::remove_dir_all(data_dir);
    }
}

#[cfg(test)]
mod budget_evidence_tests {
    use super::*;
    #[test]
    fn old_cache_without_source_read_time_cannot_be_finalized_by_reading_it_now() {
        let dir=std::env::temp_dir().join(format!("budget-evidence-{:032x}",rand::random::<u128>()));
        let mut cache=CacheFile::default();
        let mut row=parse_session_usage_row(&serde_json::json!({"session_id":"session","usage_time":1790000000,"model_name":"seedance","credits_float":"12.345678"})).unwrap();
        row.core_request_id=Some("request".into());row.core_key_id=Some("key".into());
        cache.accounts.entry("account".into()).or_default().session_usage.insert("session".into(),row);
        let path=dir.join("data").join("usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        crate::fs_utils::write_json(&path,&cache).unwrap();
        let result=budget_usage_receipt_evidence(&dir,"account","session","request","key",1790000001000).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(result.is_none(),"reading old cached evidence now is not a new post-execution source observation");
    }
    #[test]
    fn row_observation_is_post_terminal_and_not_advanced_by_absent_row_refresh() {
        let dir=std::env::temp_dir().join(format!("budget-source-row-{:032x}",rand::random::<u128>()));
        let mut cache=CacheFile::default();
        let mut row=parse_session_usage_row(&serde_json::json!({"session_id":"session","usage_time":1790000000,"model_name":"seedance","credits_float":"12.345678"})).unwrap();
        row.core_request_id=Some("request".into());row.core_key_id=Some("key".into());
        let fetched=|| {let mut value=FetchedAccountUsage::default();value.source_complete=true;value.sessions.insert("session".into(),row.clone());value};
        let path=dir.join("data").join("usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let finished=1790000001500_i64;
        // Query upper bound may be five minutes ahead; it is not observation time.
        merge_fetched_account(&mut cache,"account","A","2026-09-21","2026-09-21",1790000000,1790000300,1790000003,1790000000,fetched());
        crate::fs_utils::write_json(&path,&cache).unwrap();
        assert!(budget_usage_receipt_evidence(&dir,"account","session","request","key",finished).unwrap().is_none());
        merge_fetched_account(&mut cache,"account","A","2026-09-21","2026-09-21",1790000000,1790000305,1790000006,1790000005,fetched());
        crate::fs_utils::write_json(&path,&cache).unwrap();
        let evidence=budget_usage_receipt_evidence(&dir,"account","session","request","key",finished).unwrap().unwrap();
        assert_eq!(evidence.credits.to_string(),"12.345678");assert_eq!(evidence.read_started_at_ms,1790000005000);assert_eq!(evidence.observed_at_ms,1790000006000);
        merge_fetched_account(&mut cache,"account","A","2026-09-21","2026-09-21",1790000000,1790000310,1790000011,1790000010,FetchedAccountUsage::default());
        crate::fs_utils::write_json(&path,&cache).unwrap();
        let replay=budget_usage_receipt_evidence(&dir,"account","session","request","key",finished).unwrap().unwrap();
        assert_eq!(replay.observed_at_ms,evidence.observed_at_ms);
        assert!(budget_usage_receipt_evidence(&dir,"account","session","request","key",1790000007000).unwrap().is_none());
        assert!(budget_usage_receipt_evidence(&dir,"account","session","request","wrong-key",finished).unwrap().is_none());
        cache.accounts.get_mut("account").unwrap().session_observations.get_mut("session").unwrap().row.as_mut().unwrap().session_id="different-session".into();
        crate::fs_utils::write_json(&path,&cache).unwrap();
        let mismatched=budget_usage_receipt_evidence(&dir,"account","session","request","key",finished).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(mismatched.is_none(),"map key is not proof of the source row session identity");
    }
    #[test]
    fn completed_unique_observation_replaces_inflight_candidate_without_clearing_legacy_conflict() {
        let dir=std::env::temp_dir().join(format!("budget-inflight-{:032x}",rand::random::<u128>()));let mut cache=CacheFile::default();
        let fetched=|amount:&str| {let mut value=FetchedAccountUsage::default();value.source_complete=true;
            let mut row=parse_session_usage_row(&serde_json::json!({"session_id":"session","usage_time":1790000000,"model_name":"seedance","credits_float":amount})).unwrap();
            row.core_request_id=Some("request".into());row.core_key_id=Some("key".into());value.sessions.insert("session".into(),row);value};
        merge_fetched_account(&mut cache,"account","A","2026-09-21","2026-09-21",1790000000,1790000300,1790000001,1790000000,fetched("0.01"));
        merge_fetched_account(&mut cache,"account","A","2026-09-21","2026-09-21",1790000000,1790000305,1790000006,1790000005,fetched("0.08"));
        assert!(cache.accounts["account"].session_usage["session"].ambiguous,"legacy audit evidence remains unchanged");
        let path=dir.join("data").join("usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();crate::fs_utils::write_json(&path,&cache).unwrap();
        let result=budget_usage_receipt_evidence(&dir,"account","session","request","key",1790000002000).unwrap();
        std::fs::remove_dir_all(dir).unwrap();assert_eq!(result.expect("post-terminal raw observation").credits.as_microcredits(),80_000);
    }
    #[test]
    fn only_complete_pagination_is_eligible_as_billing_source() {
        let row=serde_json::json!({"session_id":"session","usage_time":1790000000,"model_name":"seedance","credits_float":"1"});
        for (response,expected) in [
            (serde_json::json!({"total":1,"user_usage_group_by_sessions":[row.clone()]}),true),
            (serde_json::json!({"user_usage_group_by_sessions":[row.clone()]}),false),
            (serde_json::json!({"total":1}),false),
            (serde_json::json!({"total":2,"user_usage_group_by_sessions":[row]}),false),
        ] {
            let result=fetch_account_usage_with_pages(1790000000,1790000010,||false,|_,_,_|Ok(response.clone())).unwrap();
            assert_eq!(result.source_complete,expected);
        }
    }
    #[test]
    fn equivalent_decimal_rows_are_not_conflicts_but_different_same_query_amounts_are() {
        let row=|amount:&str|serde_json::json!({"session_id":"session","usage_time":1790000000,"model_name":"seedance","credits_float":amount});
        for (other,ambiguous) in [("1.000000",false),("1.1",true)] {
            let result=fetch_account_usage_with_pages(1790000000,1790000010,||false,|_,_,_|Ok(serde_json::json!({"total":2,"user_usage_group_by_sessions":[row("1.0"),row(other)]}))).unwrap();
            assert!(result.source_complete);assert_eq!(result.sessions["session"].ambiguous,ambiguous);
        }
    }
}

/// Parse one upstream per-session usage row without inventing an identifier or amount.
/// The result is only a candidate until the source contract is verified.
fn parse_session_usage_row(value: &Value) -> Option<CachedSessionUsage> {
    let session_id = value
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| {
            !id.is_empty()
                && id.len() <= 256
                && !id.chars().any(char::is_control)
        })?
        .to_string();
    let usage_time = value.get("usage_time").and_then(Value::as_i64).filter(|ts| *ts > 0)?;
    let date = local_date_of(usage_time)?;
    let model_name = value
        .get("model_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string();
    let credits_float = ["credits_float", "amount_float"]
        .iter()
        .find_map(|field| value.get(*field))
        .and_then(|amount| match amount {
            Value::Number(number) => Some(number.to_string()),
            Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
            _ => None,
        });

    Some(CachedSessionUsage {
        session_id,
        usage_time,
        date,
        model_name,
        credits_float,
        ambiguous: false,
        core_request_id: None,
        core_key_id: None,
        core_attribution_ambiguous: false,
    })
}

#[cfg(test)]
fn annotate_core_usage_candidates(
    data_dir: &std::path::Path,
    cache: &mut CacheFile,
    account_filter: Option<&HashSet<String>>,
) -> Result<(), String> {
    annotate_core_usage_candidates_with_matcher(
        data_dir,
        cache,
        account_filter,
        &crate::api_server::bridge_billing::match_core_usage_sessions,
    )
}

#[cfg(test)]
fn annotate_core_usage_candidates_with_matcher<M>(
    data_dir: &std::path::Path,
    cache: &mut CacheFile,
    account_filter: Option<&HashSet<String>>,
    matcher: &M,
) -> Result<(), String>
where
    M: Fn(&std::path::Path, &[(String, String)]) -> Result<CoreUsageSessionMatches, String>,
{
    let pairs: Vec<(String, String)> = cache.accounts.iter()
        .filter(|(uid, _)| account_filter.map_or(true, |filter| filter.contains(*uid)))
        .flat_map(|(uid, account)| account.session_usage.keys()
            .map(|session_id| (uid.clone(), session_id.clone())))
        .collect();
    if pairs.is_empty() {
        return Ok(());
    }
    let matches = matcher(data_dir, &pairs)?;
    apply_core_usage_matches(cache, &pairs, &matches);
    Ok(())
}

fn apply_core_usage_matches(
    cache: &mut CacheFile,
    pairs: &[(String, String)],
    matches: &CoreUsageSessionMatches,
) {
    for (uid, session_id) in pairs {
        let Some(account)=cache.accounts.get_mut(uid) else {continue};
        let relation=matches.get(&(uid.clone(),session_id.clone()));
        if let Some(session)=account.session_usage.get_mut(session_id) {apply_one_core_usage_match(session,relation);}
        if let Some(session)=account.session_observations.get_mut(session_id).and_then(|source|source.row.as_mut()) {apply_one_core_usage_match(session,relation);}
    }
}
fn apply_one_core_usage_match(session:&mut CachedSessionUsage,relation:Option<&crate::api_server::bridge_billing::CoreUsageSessionMatch>) {
        if session.ambiguous {
            session.core_request_id = None;
            session.core_key_id = None;
            session.core_attribution_ambiguous = true;
            return;
        }
        match relation {
            Some(crate::api_server::bridge_billing::CoreUsageSessionMatch::Unique { request_id, core_key_id }) => {
                session.core_request_id = Some(request_id.clone());
                session.core_key_id = Some(core_key_id.clone());
                session.core_attribution_ambiguous = false;
            }
            Some(crate::api_server::bridge_billing::CoreUsageSessionMatch::Ambiguous) => {
                session.core_request_id = None;
                session.core_key_id = None;
                session.core_attribution_ambiguous = true;
            }
            None => {
                session.core_request_id = None;
                session.core_key_id = None;
                session.core_attribution_ambiguous = false;
            }
        }
}

fn merge_session_usage(
    rows: &mut BTreeMap<String, CachedSessionUsage>,
    incoming: CachedSessionUsage,
) {
    match rows.entry(incoming.session_id.clone()) {
        std::collections::btree_map::Entry::Vacant(entry) => {
            entry.insert(incoming);
        }
        std::collections::btree_map::Entry::Occupied(mut entry) => {
            let existing = entry.get_mut();
            let same_source_row = existing.session_id == incoming.session_id
                && existing.usage_time == incoming.usage_time
                && existing.date == incoming.date
                && existing.model_name == incoming.model_name
                && same_credit_text(existing.credits_float.as_deref(),incoming.credits_float.as_deref());
            if !same_source_row {
                existing.ambiguous = true;
                existing.core_request_id = None;
                existing.core_key_id = None;
                existing.core_attribution_ambiguous = true;
            }
        }
    }
}

fn same_credit_text(left:Option<&str>,right:Option<&str>)->bool {
    match (left,right) {
        (Some(left),Some(right))=>match (CreditAmount::parse(left,"credits"),CreditAmount::parse(right,"credits")) {
            (Ok(left),Ok(right))=>left==right,
            _=>left==right,
        },
        (None,None)=>true,
        _=>false,
    }
}
