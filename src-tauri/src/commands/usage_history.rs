//! Trae Work 积分消耗历史（`POST /trae/api/v1/pay/query_user_usage_group_by_session`）。
//!
//! 此前积分看板的「消耗」口径是 credits_daily 快照的余额差值推算（含签到获得等噪声）；
//! 本模块改为直连接口拉取会话级用量（credits_float / model_name / token 明细），按本地
//! 自然日聚合落盘 data/usage_history.json，供积分趋势图查询展示。
//!
//! 增量语义（避免重复计数）：
//! - 首次拉取（无缓存）：全量拉取近一年（FULL_PULL_DAYS）；
//! - 后续拉取（fresh=true）：从「上次拉取 end_time 所在本地日的 00:00」起重拉，
//!   并**替换**缓存中该日期及之后的日聚合（当天多次拉取不叠加；更早的历史保持不动）；
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
use std::collections::{BTreeMap, HashSet};
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
    daily: BTreeMap<String, UsageDayStat>,
    /// Per-session candidates retained internally for exact request attribution.
    /// This field is intentionally not included in `UsageHistoryAccount` responses.
    #[serde(default)]
    session_usage: BTreeMap<String, CachedSessionUsage>,
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
}

fn cache_path(state: &AppState) -> std::path::PathBuf {
    state.data_dir.join("data").join("usage_history.json")
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

    let mut agg: BTreeMap<String, UsageDayStat> = BTreeMap::new();
    let mut sessions = BTreeMap::new();
    let mut chunk_end = end_ts;
    loop {
        let chunk_start = (chunk_end - CHUNK_DAYS * 86400 + 1).max(start_ts);
        fetch_chunk(&agent, jwt, chunk_start, chunk_end, &mut agg, &mut sessions)?;
        if chunk_start <= start_ts {
            break;
        }
        chunk_end = chunk_start - 1;
    }
    Ok(FetchedAccountUsage { daily: agg, sessions })
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
pub(crate) fn refresh_pending_core_usage(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
) -> Result<usize, String> {
    refresh_pending_core_usage_with(data_dir, pending_accounts, credentials, chrono::Local::now().timestamp(),
        |_, jwt, start, end| fetch_account_usage(jwt, start, end))
}

fn refresh_pending_core_usage_with<F>(
    data_dir: &std::path::Path,
    pending_accounts: &[(String, i64)],
    credentials: &[(String, String, String)],
    now_ts: i64,
    mut fetch: F,
) -> Result<usize, String>
where
    F: FnMut(&str, &str, i64, i64) -> Result<FetchedAccountUsage, String>,
{
    if pending_accounts.is_empty() {
        return Ok(0);
    }
    let mut cache: CacheFile = crate::fs_utils::read_json(&data_dir.join("data").join("usage_history.json"));
    let poll_floor = now_ts.saturating_sub(7 * 86400);
    let floor_date = local_date_of(poll_floor).unwrap_or_default();
    let mut refreshed = 0usize;
    let mut failed = false;
    let mut refreshed_uids = HashSet::new();

    for (uid, attempt_at_ms) in pending_accounts {
        let Some((_, name, jwt)) = credentials.iter().find(|(account_uid, _, jwt)| account_uid == uid && !jwt.trim().is_empty()) else {
            continue;
        };
        let attempt_date = local_date_of(attempt_at_ms.div_euclid(1000)).unwrap_or_else(|| floor_date.clone());
        let cached_date = cache.accounts.get(uid)
            .and_then(|account| account.last_fetch_end_ts)
            .and_then(local_date_of);
        let requested_date = cached_date.map_or(attempt_date.clone(), |date| date.min(attempt_date));
        let start_date = requested_date.max(floor_date.clone());
        let start_ts = local_midnight_ts(&start_date).unwrap_or(poll_floor);

        // The local Windows clock may lag the upstream clock by several minutes.
        // Only widen this read-only query window; attribution still requires an
        // exact Core request, key, account and upstream session match.
        match fetch(uid, jwt, start_ts, now_ts.saturating_add(5 * 60)) {
            Ok(fetched) => {
                let entry = cache.accounts.entry(uid.clone()).or_default();
                entry.name = name.clone();
                entry.daily.retain(|date, _| date < &start_date);
                entry.session_usage.retain(|_, session| session.date < start_date);
                for (date, stat) in fetched.daily {
                    entry.daily.insert(date, stat);
                }
                for (_, session) in fetched.sessions {
                    merge_session_usage(&mut entry.session_usage, session);
                }
                entry.last_fetch_end_ts = Some(now_ts);
                refreshed += 1;
                refreshed_uids.insert(uid.clone());
            }
            Err(_) => failed = true,
        }
    }

    if refreshed > 0 {
        let _ = annotate_core_usage_candidates(data_dir, &mut cache, Some(&refreshed_uids));
        cache.fetched_at = Some(now_ts);
        let path = data_dir.join("data").join("usage_history.json");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| "usage history cache directory unavailable".to_string())?;
        }
        crate::fs_utils::write_json(&path, &cache)
            .map_err(|_| "usage history cache write failed".to_string())?;
    }
    if failed && refreshed == 0 {
        return Err("pending Core usage source query failed".into());
    }
    Ok(refreshed)
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

/// 单分块（≤30 天）分页拉取并聚合进 agg
fn fetch_chunk(
    agent: &ureq::Agent,
    jwt: &str,
    start_ts: i64,
    end_ts: i64,
    agg: &mut BTreeMap<String, UsageDayStat>,
    sessions: &mut BTreeMap<String, CachedSessionUsage>,
) -> Result<(), String> {
    let mut page: u32 = 1;
    let mut got: usize = 0;
    let mut total: Option<usize> = None;

    loop {
        let body = json!({
            "start_time": start_ts,
            "end_time": end_ts,
            "page_size": PAGE_SIZE,
            "page_num": page,
            "usage_type": [USAGE_TYPE],
        });
        let resp = web_usage_post(agent, jwt, body)?;
        if total.is_none() {
            total = Some(resp.get("total").and_then(Value::as_u64).unwrap_or(0) as usize);
        }
        let arr = resp
            .get("user_usage_group_by_sessions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if arr.is_empty() {
            break;
        }
        for row in &arr {
            ingest_usage_row(row, agg, sessions);
        }
        got += arr.len();
        let total_n = total.unwrap_or(0);
        // 终止条件：已取满 total / 本页不满页大小（服务端截断页）/ 超过安全页数上限
        if got >= total_n || (arr.len() as u32) < PAGE_SIZE || page >= MAX_PAGES {
            break;
        }
        page += 1;
    }
    Ok(())
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
    let now_ts = chrono::Local::now().timestamp();
    let accounts = crate::vault::load_accounts(&state);
    let mut cache: CacheFile = crate::fs_utils::read_json(&cache_path(&state));

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

    // 增量拉取：无缓存账号全量近一年；已有账号从上次拉取日 00:00 重拉并替换该日及之后
    let full_start = now_ts - FULL_PULL_DAYS * 86400;
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
        let (start_ts, refetch_from) =
            match cache.accounts.get(&uid).and_then(|c| c.last_fetch_end_ts) {
                Some(last_end) => {
                    let from_date = local_date_of(last_end)
                        .or_else(|| local_date_of(now_ts))
                        .unwrap_or_default();
                    let midnight = local_midnight_ts(&from_date).unwrap_or(now_ts - 86400);
                    (midnight, from_date)
                }
                None => (full_start, String::new()),
            };
        match fetch_account_usage(&a.jwt, start_ts, now_ts) {
            Ok(new_agg) => {
                let entry = cache.accounts.entry(uid.clone()).or_default();
                entry.name = name;
                if refetch_from.is_empty() {
                    // 全量：整体替换
                    entry.daily = new_agg.daily;
                    entry.session_usage = new_agg.sessions;
                } else {
                    // 增量：替换 refetch_from 及之后的日聚合（当天多次拉取不叠加）
                    entry
                        .daily
                        .retain(|d, _| d.as_str() < refetch_from.as_str());
                    entry.session_usage.retain(|_, session| session.date < refetch_from);
                    for (d, v) in new_agg.daily {
                        entry.daily.insert(d, v);
                    }
                    for (_, session) in new_agg.sessions {
                        merge_session_usage(&mut entry.session_usage, session);
                    }
                }
                entry.last_fetch_end_ts = Some(now_ts);
            }
            Err(e) => {
                // 拉取失败：保留旧缓存，错误在结果中注明
                errors.insert(uid, e);
            }
        }
    }

    cache.fetched_at = Some(now_ts);
    let _ = annotate_core_usage_candidates(&state.data_dir, &mut cache, None);
    let _ = crate::fs_utils::write_json(&cache_path(&state), &cache);

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
        fetched_at: now_ts,
        cached: false,
        accounts: out,
    })
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

fn annotate_core_usage_candidates(
    data_dir: &std::path::Path,
    cache: &mut CacheFile,
    account_filter: Option<&HashSet<String>>,
) -> Result<(), String> {
    let pairs: Vec<(String, String)> = cache.accounts.iter()
        .filter(|(uid, _)| account_filter.map_or(true, |filter| filter.contains(*uid)))
        .flat_map(|(uid, account)| account.session_usage.keys()
            .map(|session_id| (uid.clone(), session_id.clone())))
        .collect();
    if pairs.is_empty() {
        return Ok(());
    }
    let matches = crate::api_server::bridge_billing::match_core_usage_sessions(data_dir, &pairs)?;
    for (uid, account) in &mut cache.accounts {
        if account_filter.map_or(false, |filter| !filter.contains(uid)) {
            continue;
        }
        for session in account.session_usage.values_mut() {
            if session.ambiguous {
                session.core_request_id = None;
                session.core_key_id = None;
                session.core_attribution_ambiguous = true;
                continue;
            }
            match matches.get(&(uid.clone(), session.session_id.clone())) {
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
    }
    Ok(())
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
                && existing.credits_float == incoming.credits_float;
            if !same_source_row {
                existing.ambiguous = true;
                existing.core_request_id = None;
                existing.core_key_id = None;
                existing.core_attribution_ambiguous = true;
            }
        }
    }
}
