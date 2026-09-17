//! BitBrowser → Trae Work 账号发现与导入。
//!
//! BitBrowser 的 Local API 只负责找到 profile 并打开它的 CDP 调试端口；
//! 账号凭据从 Trae Work 页面 `localStorage["Cloud-IDE-Token"]` 临时读取，
//! 随即写入 AI Work 助手的 Stronghold vault，并在首次导入时生成原生 icube 快照；
//! 可选接管的 refresh_token 用于后续本机续期。密码、Cookie 与完整 JWT 均不返回前端，
//! 也不写入日志。Local API 地址被限制为本机回环地址，避免把凭据转发到远端。

use serde::Serialize;
use serde_json::{json, Value};
use std::time::Duration;
use tauri::State;
use tungstenite::{connect, Message};

use crate::commands::trae_apps::DiscoveredAccount;
use crate::state::AppState;

const DEFAULT_API_URL: &str = "http://127.0.0.1:54345";
const TRAE_APP: &str = "TraeWork";
const TRAE_LABEL: &str = "BitBrowser · Trae Work";
const DETAIL_INTERVAL: Duration = Duration::from_millis(450);

/// BitBrowser Local API 返回的一个 profile（字段在不同版本中略有差异，故用 Value 解析）。
#[derive(Clone, Debug)]
pub(crate) struct BrowserProfile {
    id: String,
    seq: Option<i64>,
    name: Option<String>,
    status: Option<bool>,
    hint: String,
}

#[derive(Clone, Debug)]
struct CapturedToken {
    token: String,
    refresh_token: Option<String>,
    user_id: String,
    exp_timestamp: Option<i64>,
    page_url: String,
}

#[derive(Serialize, Clone)]
pub struct BitBrowserImportResult {
    pub user_id: String,
    pub name: String,
    pub updated: bool,
    pub token_exp_timestamp: Option<i64>,
    pub refresh_token_captured: bool,
    /// 首次 BitBrowser 导入是否已自动生成原生 TRAE Work CN 切换快照。
    #[serde(default)]
    pub native_snapshot_created: bool,
    /// 已有快照时，是否已用本次最新 JWT 更新快照。
    #[serde(default)]
    pub native_snapshot_updated: bool,
    /// 快照生成失败时保留账号导入结果，并把原因显示给前端。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_snapshot_error: Option<String>,
}

/// BitBrowser profile 的安全视图。只返回窗口定位所需的元数据，
/// 不包含密码、Cookie 或浏览器 Local API 凭据。
#[derive(Serialize, Clone)]
pub struct BitBrowserProfileView {
    pub id: String,
    pub seq: Option<i64>,
    pub name: Option<String>,
    pub hint: String,
    pub open: Option<bool>,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(12))
        .timeout_connect(Duration::from_secs(4))
        .build()
}

/// 仅允许回环地址。BitBrowser 官方 Local API 默认监听 127.0.0.1:54345。
pub(crate) fn local_api_base(state: &AppState) -> Result<String, String> {
    let configured = state.settings().bitbrowser_api_url;
    let raw = if configured.trim().is_empty() {
        DEFAULT_API_URL
    } else {
        configured.trim()
    };
    let base = raw.trim_end_matches('/');
    let authority = base
        .strip_prefix("http://")
        .or_else(|| base.strip_prefix("https://"))
        .ok_or_else(|| "BitBrowser API 地址必须以 http:// 或 https:// 开头".to_string())?
        .split('/')
        .next()
        .unwrap_or("");
    let host = if let Some(rest) = authority.strip_prefix('[') {
        rest.split(']').next().unwrap_or(rest)
    } else {
        authority.rsplit_once(':').map(|(h, _)| h).unwrap_or(authority)
    };
    let host = host.trim_matches(['[', ']']);
    if !matches!(host.to_ascii_lowercase().as_str(), "127.0.0.1" | "localhost" | "::1") {
        return Err("为保护登录凭据，BitBrowser API 只允许配置本机地址（127.0.0.1/localhost/::1）".into());
    }
    Ok(base.to_string())
}

fn post_json(base: &str, path: &str, body: Value) -> Result<Value, String> {
    let url = format!("{base}{path}");
    let response = agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "application/json")
        .send_json(body)
        .map_err(|e| format!("BitBrowser {path} 请求失败：{e}"))?;
    let value: Value = response
        .into_json()
        .map_err(|e| format!("BitBrowser {path} 响应解析失败：{e}"))?;
    if value.get("success").and_then(Value::as_bool) == Some(false) {
        let msg = value
            .get("msg")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("未知错误");
        return Err(format!("BitBrowser {path}：{msg}"));
    }
    Ok(value.get("data").cloned().unwrap_or(Value::Null))
}

fn value_string(value: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        })
    })
}

fn value_i64(value: &Value, names: &[&str]) -> Option<i64> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
        })
    })
}

fn value_bool(value: &Value, names: &[&str]) -> Option<bool> {
    names.iter().find_map(|name| {
        value.get(*name).and_then(|v| {
            v.as_bool().or_else(|| {
                v.as_i64().map(|n| n != 0).or_else(|| {
                    v.as_str().and_then(|s| match s.to_ascii_lowercase().as_str() {
                        "true" | "1" | "open" | "running" => Some(true),
                        "false" | "0" | "closed" | "stop" => Some(false),
                        _ => None,
                    })
                })
            })
        })
    })
}

fn decoded(s: &str) -> String {
    urlencoding::decode(s)
        .map(|v| v.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

fn is_trae_hint(s: &str) -> bool {
    let lower = decoded(s).to_ascii_lowercase();
    lower.contains("trae.cn")
        || lower.contains("trae.com.cn")
        || lower.contains("trae.ai")
        || lower.contains("trae work")
        || lower.contains("traework")
        || lower.contains("solo cn")
        || lower.contains("aiwork")
        || lower.contains("ai work")
}

fn profile_from_value(value: &Value) -> Option<BrowserProfile> {
    let id = value_string(value, &["id", "browserId", "browser_id"])?;
    if id.trim().is_empty() {
        return None;
    }
    let mut hint_parts = Vec::new();
    for key in [
        "url",
        "platform",
        "platformName",
        "name",
        "remark",
        "groupName",
        "browserFingerPrint",
    ] {
        if let Some(s) = value_string(value, &[key]) {
            hint_parts.push(s);
        }
    }
    Some(BrowserProfile {
        id,
        seq: value_i64(value, &["seq", "sequence"]),
        name: value_string(value, &["name", "remark"]),
        status: value_bool(value, &["status", "open", "running"]),
        hint: hint_parts.join(" "),
    })
}

pub(crate) fn list_profiles(base: &str) -> Result<Vec<BrowserProfile>, String> {
    // BitBrowser 某些版本会忽略过大的 pageSize（即使请求 1000 也只返回 100），
    // 所以不能只取第一页；按响应中的 totalNum/pageSize 继续翻页，确保 UID 查找
    // 不会在 profile 数量较多时漏掉目标窗口。
    let mut page = 0i64;
    let mut data = post_json(
        base,
        "/browser/list",
        json!({"page": page, "pageSize": 1000}),
    )?;
    let mut profiles = Vec::new();
    loop {
        let entries = data
            .get("list")
            .and_then(Value::as_array)
            .or_else(|| data.as_array())
            .ok_or_else(|| "BitBrowser /browser/list 响应中没有 profile 列表".to_string())?;
        let batch_len = entries.len();
        profiles.extend(entries.iter().filter_map(profile_from_value));

        let total = value_i64(&data, &["totalNum", "total", "totalCount"]);
        let page_size = value_i64(&data, &["pageSize"])
            .filter(|size| *size > 0)
            .unwrap_or(batch_len as i64);
        let complete = batch_len == 0
            || page_size <= 0
            || total.is_none()
            || profiles.len() as i64 >= total.unwrap_or(0)
            || batch_len < page_size as usize;
        if complete || page >= 999 {
            break;
        }

        page += 1;
        data = post_json(
            base,
            "/browser/list",
            json!({"page": page, "pageSize": page_size}),
        )?;
    }
    Ok(profiles)
}

/// 列出 BitBrowser profile 的安全视图，供 OAuth 登录窗口选择目标 profile。
/// 完整 profile 响应中可能包含密码/Cookie，本命令只下发定位元数据。
#[tauri::command(async)]
pub fn bitbrowser_profiles_list(
    state: State<AppState>,
) -> Result<Vec<BitBrowserProfileView>, String> {
    let base = local_api_base(&state)?;
    Ok(list_profiles(&base)?
        .into_iter()
        .map(|profile| BitBrowserProfileView {
            id: profile.id,
            seq: profile.seq,
            name: profile.name,
            hint: profile.hint,
            open: profile.status,
        })
        .collect())
}

fn detail_profile(base: &str, id: &str, listed: &BrowserProfile) -> Result<BrowserProfile, String> {
    let detail = post_json(base, "/browser/detail", json!({"id": id}))?;
    let mut profile = profile_from_value(&detail).unwrap_or_else(|| listed.clone());
    if profile.name.is_none() {
        profile.name = listed.name.clone();
    }
    if profile.seq.is_none() {
        profile.seq = listed.seq;
    }
    if profile.status.is_none() {
        profile.status = listed.status;
    }
    if profile.hint.is_empty() {
        profile.hint = listed.hint.clone();
    } else if !listed.hint.is_empty() {
        profile.hint = format!("{} {}", profile.hint, listed.hint);
    }
    Ok(profile)
}

fn http_base(raw: &str) -> String {
    if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.trim_end_matches('/').to_string()
    } else {
        format!("http://{}", raw.trim_end_matches('/'))
    }
}

fn open_profile(base: &str, profile: &BrowserProfile) -> Result<String, String> {
    let data = post_json(
        base,
        "/browser/open",
        json!({"id": profile.id, "args": [], "queue": true}),
    )?;
    let raw_http = value_string(&data, &["http", "httpPort", "debugAddress"])
        .ok_or_else(|| "BitBrowser 已打开 profile，但没有返回 CDP HTTP 地址".to_string())?;
    Ok(http_base(&raw_http))
}

fn devtools_targets_once(http: &str) -> Result<Vec<Value>, String> {
    let url = format!("{}/json/list", http.trim_end_matches('/'));
    let response = agent()
        .get(&url)
        .call()
        .map_err(|e| format!("读取 BitBrowser CDP 目标失败：{e}"))?;
    response
        .into_json::<Vec<Value>>()
        .map_err(|e| format!("解析 BitBrowser CDP 目标失败：{e}"))
}

/// profile 刚打开时 Chrome 可能先返回空/占位目标，短暂轮询直到 Trae 页面完成加载。
fn devtools_targets(http: &str) -> Result<Vec<Value>, String> {
    let mut last_error = String::new();
    for _ in 0..20 {
        match devtools_targets_once(http) {
            Ok(targets)
                if targets.iter().any(|target| {
                    target.get("type").and_then(Value::as_str) == Some("page")
                        && is_trae_page(target)
                        && target
                            .get("webSocketDebuggerUrl")
                            .and_then(Value::as_str)
                            .is_some()
                }) =>
            {
                return Ok(targets)
            }
            Ok(_) => last_error = "CDP 暂无页面目标".to_string(),
            Err(error) => last_error = error,
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err(format!("等待 BitBrowser Trae Work 页面超时：{last_error}"))
}

/// profile 可能当前停留在任意网页；OAuth 导航只需要一个可用的普通页面，
/// 不应要求它在导航前已经打开 Trae 页面。
fn devtools_any_page_targets(http: &str) -> Result<Vec<Value>, String> {
    let mut last_error = String::new();
    for _ in 0..20 {
        match devtools_targets_once(http) {
            Ok(targets) if normal_page_target(&targets).is_some() => return Ok(targets),
            Ok(_) => last_error = "CDP 暂无可导航页面目标".to_string(),
            Err(error) => last_error = error,
        }
        std::thread::sleep(Duration::from_millis(300));
    }
    Err(format!("等待 BitBrowser 可导航页面超时：{last_error}"))
}

fn target_page_url(target: &Value) -> String {
    target
        .get("url")
        .and_then(Value::as_str)
        .map(decoded)
        .unwrap_or_default()
}

fn is_trae_page(target: &Value) -> bool {
    let url = target_page_url(target);
    let title = target
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    is_trae_hint(&url) || title.to_ascii_lowercase().contains("trae")
}

fn cdp_evaluate(ws_url: &str) -> Result<Value, String> {
    let (mut socket, _) = connect(ws_url)
        .map_err(|e| format!("连接 Trae Work CDP 页面失败：{e}"))?;
    let request = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": {
            "expression": "(() => { const out = {}; try { for (const k of Object.keys(localStorage)) { const lk = k.toLowerCase(); if (lk === 'cloud-ide-token' || lk === 'cloud-ide-jwt' || (lk.includes('token') && (lk.includes('cloud-ide') || lk.includes('refresh')))) { out[k] = localStorage.getItem(k); } } } catch (_) {} return out; })()",
            "returnByValue": true,
            "awaitPromise": true
        }
    });
    socket
        .send(Message::Text(request.to_string().into()))
        .map_err(|e| format!("发送 Trae Work CDP 请求失败：{e}"))?;

    loop {
        let message = socket
            .read()
            .map_err(|e| format!("读取 Trae Work CDP 响应失败：{e}"))?;
        if !message.is_text() {
            continue;
        }
        let text = message
            .into_text()
            .map_err(|e| format!("读取 Trae Work CDP 文本失败：{e}"))?
            .to_string();
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 Trae Work CDP 响应失败：{e}"))?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("Trae Work CDP Runtime.evaluate 失败：{error}"));
        }
        return Ok(value
            .get("result")
            .and_then(|v| v.get("result"))
            .and_then(|v| v.get("value"))
            .cloned()
            .unwrap_or(Value::Null));
    }
}

/// 请求 Trae Work 页面重新加载。Trae Work 网页在加载时会调用
/// `/cloudide/api/v3/common/GetUserToken`，服务端据当前会话签发新的短期 JWT，
/// 随后写回 `localStorage[Cloud-IDE-Token]`。只等待 CDP 的 reload ACK，
/// 新 token 的可见性由 `capture_profile_refreshed` 轮询确认。
fn cdp_reload(ws_url: &str) -> Result<(), String> {
    let (mut socket, _) = connect(ws_url)
        .map_err(|e| format!("连接 Trae Work CDP 页面刷新失败：{e}"))?;
    let request = json!({
        "id": 1,
        "method": "Page.reload",
        "params": { "ignoreCache": false }
    });
    socket
        .send(Message::Text(request.to_string().into()))
        .map_err(|e| format!("发送 Trae Work 页面刷新请求失败：{e}"))?;

    loop {
        let message = socket
            .read()
            .map_err(|e| format!("读取 Trae Work 页面刷新响应失败：{e}"))?;
        if !message.is_text() {
            continue;
        }
        let text = message
            .into_text()
            .map_err(|e| format!("读取 Trae Work 页面刷新文本失败：{e}"))?
            .to_string();
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 Trae Work 页面刷新响应失败：{e}"))?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("Trae Work 页面刷新失败：{error}"));
        }
        return Ok(());
    }
}

/// 在 BitBrowser profile 的当前页面中导航到指定 URL。
///
/// 只供本地 OAuth 桥使用：调用方负责生成并校验 URL，避免把任意远端
/// 导航能力暴露给前端。CDP 连接仅在本次导航期间存在，不保存浏览器会话。
fn cdp_navigate(ws_url: &str, url: &str) -> Result<(), String> {
    let (mut socket, _) = connect(ws_url)
        .map_err(|e| format!("连接 BitBrowser OAuth 页面失败：{e}"))?;
    let request = json!({
        "id": 1,
        "method": "Page.navigate",
        "params": { "url": url }
    });
    socket
        .send(Message::Text(request.to_string().into()))
        .map_err(|e| format!("发送 BitBrowser OAuth 导航请求失败：{e}"))?;

    loop {
        let message = socket
            .read()
            .map_err(|e| format!("读取 BitBrowser OAuth 导航响应失败：{e}"))?;
        if !message.is_text() {
            continue;
        }
        let text = message
            .into_text()
            .map_err(|e| format!("读取 BitBrowser OAuth 导航文本失败：{e}"))?
            .to_string();
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| format!("解析 BitBrowser OAuth 导航响应失败：{e}"))?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("BitBrowser OAuth 页面导航失败：{error}"));
        }
        return Ok(());
    }
}

fn normal_page_target(targets: &[Value]) -> Option<&Value> {
    targets.iter().find(|target| {
        target.get("type").and_then(Value::as_str) == Some("page")
            && target
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .is_some()
            && !target_page_url(target).starts_with("chrome://")
            && !target_page_url(target).contains("console.bitbrowser.net")
    })
}

/// 打开 BitBrowser profile 并导航到助手生成的 OAuth URL。
///
/// 返回前只确认 CDP 已接受导航命令；OAuth 回调由调用方的本地监听器接收。
pub(crate) fn navigate_profile_url(
    base: &str,
    profile_id: &str,
    url: &str,
) -> Result<(), String> {
    let id = profile_id.trim();
    if id.is_empty()
        || id.len() > 200
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("无效的 BitBrowser profile id".into());
    }
    if !(url.starts_with("https://www.trae.cn/authorization?")
        || url.starts_with("https://trae.cn/authorization?"))
    {
        return Err("OAuth 地址不是受支持的 Trae 授权地址".into());
    }

    let listed = BrowserProfile {
        id: id.to_string(),
        seq: None,
        name: None,
        status: None,
        hint: String::new(),
    };
    let profile = detail_profile(base, id, &listed)?;
    let http = open_profile(base, &profile)?;
    let targets = devtools_any_page_targets(&http)?;
    let target = normal_page_target(&targets)
        .or_else(|| {
            targets.iter().find(|target| {
                target.get("type").and_then(Value::as_str) == Some("page")
                    && target
                        .get("webSocketDebuggerUrl")
                        .and_then(Value::as_str)
                        .is_some()
            })
        })
        .ok_or_else(|| "BitBrowser profile 中没有可导航的页面".to_string())?;
    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "BitBrowser 页面没有 CDP WebSocket 地址".to_string())?;
    cdp_navigate(ws_url, url)
}

/// 读取指定 profile 中现有的 Cloud-IDE token（仅在 Rust 内存中短暂存在）。
/// OAuth 桥将其作为可选的 x-cloudide-token 请求头使用，缺失时仍可继续授权。
pub(crate) fn capture_profile_token_by_id(
    base: &str,
    profile_id: &str,
) -> Option<String> {
    let listed = BrowserProfile {
        id: profile_id.trim().to_string(),
        seq: None,
        name: None,
        status: None,
        hint: String::new(),
    };
    let profile = detail_profile(base, profile_id.trim(), &listed).ok()?;
    let http = open_profile(base, &profile).ok()?;
    let targets = devtools_targets(&http).ok()?;
    let target = normal_page_target(&targets).or_else(|| {
        targets.iter().find(|target| {
            target.get("type").and_then(Value::as_str) == Some("page")
                && target
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .is_some()
        })
    })?;
    capture_token(target).ok().map(|captured| captured.token)
}

fn normalise_token(value: &str) -> String {
    let mut token = value.trim().to_string();
    if let Ok(parsed) = serde_json::from_str::<String>(&token) {
        token = parsed;
    }
    if let Some(rest) = token.strip_prefix("Bearer ") {
        token = rest.trim().to_string();
    }
    if let Some(rest) = token.strip_prefix("Cloud-IDE-JWT ") {
        token = rest.trim().to_string();
    }
    token
}

fn capture_token(target: &Value) -> Result<CapturedToken, String> {
    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Trae Work 页面没有 CDP WebSocket 地址".to_string())?;
    let storage = cdp_evaluate(ws_url)?;
    let object = storage
        .as_object()
        .ok_or_else(|| "Trae Work 页面 localStorage 读取结果不是对象".to_string())?;
    let refresh_token = object.iter().find_map(|(key, value)| {
        let lower = key.to_ascii_lowercase();
        if !lower.contains("refresh") || !lower.contains("token") {
            return None;
        }
        value.as_str().map(normalise_token).filter(|token| !token.is_empty())
    });
    let mut candidates: Vec<(&String, &Value)> = object
        .iter()
        .filter(|(key, _)| {
            let lower = key.to_ascii_lowercase();
            !lower.contains("refresh") && (lower.contains("cloud-ide") || lower == "cloud-ide-jwt")
        })
        .collect();
    candidates.sort_by_key(|(key, _)| {
        let lower = key.to_ascii_lowercase();
        if lower == "cloud-ide-token" {
            0
        } else if lower == "cloud-ide-jwt" {
            1
        } else {
            2
        }
    });
    for (_, value) in candidates {
        let Some(raw) = value.as_str() else { continue };
        let token = normalise_token(raw);
        if token.is_empty() {
            continue;
        }
        let info = crate::jwt::parse(&token);
        let Some(user_id) = info.user_id else { continue };
        if user_id.trim().is_empty() {
            continue;
        }
        return Ok(CapturedToken {
            token,
            refresh_token,
            user_id,
            exp_timestamp: info.exp_timestamp,
            page_url: target_page_url(target),
        });
    }
    Err("Trae Work 页面未找到有效的 Cloud-IDE-Token（可能尚未登录或页面尚未加载完成）".into())
}

fn capture_profile(base: &str, profile: &BrowserProfile) -> Result<CapturedToken, String> {
    let http = open_profile(base, profile)?;
    let targets = devtools_targets(&http)?;
    let target = targets.iter().find(|target| {
        target.get("type").and_then(Value::as_str) == Some("page")
            && is_trae_page(target)
            && target
                .get("webSocketDebuggerUrl")
                .and_then(Value::as_str)
                .is_some()
    })
        .ok_or_else(|| "BitBrowser profile 中没有可用的 Trae Work 页面".to_string())?;
    capture_token(target)
}

/// 真正刷新 BitBrowser 中 Trae Work 的网页登录态后再捕获 JWT。
///
/// 仅重复读取 localStorage 只能得到原 token，过期时间不会变化；Trae Work
/// 页面 reload 会触发 common/GetUserToken，服务端按当前会话返回新的 JWT。
/// 这里要求 token 字符串确实发生变化，避免把“重新保存旧 token”误报为续期成功。
fn capture_profile_refreshed(
    base: &str,
    profile: &BrowserProfile,
    expected_user_id: Option<&str>,
) -> Result<CapturedToken, String> {
    let http = open_profile(base, profile)?;
    let targets = devtools_targets(&http)?;
    let target = targets
        .iter()
        .find(|target| {
            target.get("type").and_then(Value::as_str) == Some("page")
                && is_trae_page(target)
                && target
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .is_some()
        })
        .ok_or_else(|| "BitBrowser profile 中没有可用的 Trae Work 页面".to_string())?;
    let ws_url = target
        .get("webSocketDebuggerUrl")
        .and_then(Value::as_str)
        .ok_or_else(|| "Trae Work 页面没有 CDP WebSocket 地址".to_string())?
        .to_string();
    let before = capture_token(target)?;

    // 续期只应刷新目标账号。先读取一次当前 UID，避免为了查找一个账号而
    // 对 BitBrowser 中其他 Trae profile 也触发 GetUserToken。
    if let Some(expected) = expected_user_id {
        if before.user_id != expected {
            return Ok(before);
        }
    }

    cdp_reload(&ws_url)?;

    // reload ACK 早于页面脚本完成；轮询 localStorage，直到页面完成
    // GetUserToken 并写入新值，最多等待约 12 秒。
    let mut last_error = String::new();
    for _ in 0..24 {
        std::thread::sleep(Duration::from_millis(500));
        let targets = match devtools_targets_once(&http) {
            Ok(targets) => targets,
            Err(error) => {
                last_error = error;
                continue;
            }
        };
        let Some(target) = targets.iter().find(|target| {
            target.get("type").and_then(Value::as_str) == Some("page")
                && is_trae_page(target)
                && target
                    .get("webSocketDebuggerUrl")
                    .and_then(Value::as_str)
                    .is_some()
        }) else {
            last_error = "CDP 暂无 Trae Work 页面目标".to_string();
            continue;
        };
        match capture_token(target) {
            Ok(next) if next.user_id != before.user_id => {
                return Err(format!(
                    "页面刷新后账号发生变化：原 UID={}，当前 UID={}",
                    before.user_id, next.user_id
                ));
            }
            Ok(next) if next.token != before.token => return Ok(next),
            Ok(_) => last_error = "页面仍返回旧 JWT，GetUserToken 尚未产生新值".to_string(),
            Err(error) => last_error = error,
        }
    }
    Err(format!(
        "Trae Work 页面已刷新，但未取得新 JWT（{last_error}）"
    ))
}

/// 导入时尽量补齐展示名，但使用短超时；名字查询失败不影响 JWT 导入。
fn user_name_fast(access_token: &str) -> Option<String> {
    let auth = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {access_token}")
    };
    let response = agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .send_json(json!({}))
        .ok()?;
    let body: Value = response.into_json().ok()?;
    let data = body.get("data").or_else(|| body.get("result"))?;
    let uid = data
        .get("user_id")
        .or_else(|| data.get("UserID"))
        .or_else(|| data.get("userId"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let name = data
        .get("name")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("userName"))
        .or_else(|| data.get("nickname"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let result = if !name.trim().is_empty() { name } else { uid };
    (!result.trim().is_empty()).then(|| result.to_string())
}

fn discovered_from_capture(
    profile: &BrowserProfile,
    captured: Result<CapturedToken, String>,
    known: &std::collections::HashSet<String>,
) -> DiscoveredAccount {
    match captured {
        Ok(token) => DiscoveredAccount {
            user_id: token.user_id.clone(),
            dc_uid: None,
            uid_confident: true,
            app: TRAE_APP.to_string(),
            app_label: TRAE_LABEL.to_string(),
            in_pool: known.contains(&token.user_id),
            storage_path: format!("BitBrowser profile {}", profile.id),
            source: "bitbrowser".to_string(),
            window_id: Some(profile.id.clone()),
            window_seq: profile.seq,
            window_name: profile.name.clone(),
            page_url: Some(token.page_url),
            token_present: true,
            token_exp_timestamp: token.exp_timestamp,
            read_error: None,
        },
        Err(error) => DiscoveredAccount {
            // 没有账号 uid 时使用空串；前端只展示 profile 信息并禁用导入按钮。
            user_id: String::new(),
            dc_uid: None,
            uid_confident: false,
            app: TRAE_APP.to_string(),
            app_label: TRAE_LABEL.to_string(),
            in_pool: false,
            storage_path: format!("BitBrowser profile {}", profile.id),
            source: "bitbrowser".to_string(),
            window_id: Some(profile.id.clone()),
            window_seq: profile.seq,
            window_name: profile.name.clone(),
            page_url: None,
            token_present: false,
            token_exp_timestamp: None,
            read_error: Some(error),
        },
    }
}

/// 扫描 BitBrowser 中 URL/备注指向 Trae Work 的 profile，并读取当前页面登录 uid。
/// 只返回 uid、窗口元数据和 token 是否存在；完整凭据仅在 Rust 内存中短暂存在。
#[tauri::command(async)]
pub fn bitbrowser_accounts_discover(state: State<AppState>) -> Result<Vec<DiscoveredAccount>, String> {
    let base = local_api_base(&state)?;
    let profiles = list_profiles(&base)?;
    let accounts = crate::vault::load_accounts(&state);
    let known = crate::commands::trae_apps::pool_uid_set_for_discovery(&accounts);
    let mut out = Vec::new();

    for (index, listed) in profiles.into_iter().enumerate() {
        // BitBrowser 对 detail 接口有频率限制；串行留出间隔，避免 429 导致漏掉 profile。
        if index > 0 {
            std::thread::sleep(DETAIL_INTERVAL);
        }
        let profile = match detail_profile(&base, &listed.id, &listed) {
            Ok(p) => p,
            Err(error) => {
                // detail 失败的 profile 没有可靠的 URL，不能猜测其是否为 Trae。
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("BitBrowser profile detail 读取失败: id={} error={error}", listed.id),
                );
                continue;
            }
        };
        if !is_trae_hint(&profile.hint) {
            continue;
        }
        let captured = capture_profile(&base, &profile);
        if let Err(error) = &captured {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("BitBrowser Trae Work profile 未读取到登录 token: id={} error={error}", profile.id),
            );
        }
        out.push(discovered_from_capture(&profile, captured, &known));
    }
    Ok(out)
}

/// 将一次捕获到的 BitBrowser 登录态写入账号池。完整 token 只在此函数的 Rust 内存中流转，
/// save_accounts 会在写入 checkin_accounts.json 前将其置空并持久化到 Stronghold vault。
fn persist_captured_token(
    state: &AppState,
    profile: &BrowserProfile,
    captured: &CapturedToken,
    account_name: Option<String>,
) -> Result<BitBrowserImportResult, String> {
    let mut name = account_name.unwrap_or_default().trim().to_string();
    if name.is_empty() {
        name = profile
            .name
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_default();
    }
    if name.is_empty() {
        // GetUserInfo 失败不影响导入，避免因网络波动丢失刚捕获的登录态。
        name = user_name_fast(&captured.token).unwrap_or_default();
    }
    if name.is_empty() {
        let tail: String = captured
            .user_id
            .chars()
            .rev()
            .take(4)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        name = format!("Trae Work-…{tail}");
    }

    let mut accounts = crate::vault::load_accounts(state);
    let mut updated = false;
    // 如果本次 BitBrowser 页面只读到了短期 JWT，没有读到 refresh_token，
    // 必须沿用账号池里已有的原生 refresh_token。此前这里虽然保留了 vault
    // 中的 token，却把 None 传给快照生成器，导致 storage.json 的
    // refreshToken 被清空，下一次切换只能带着即将过期的 JWT 启动。
    let effective_refresh_token: Option<String>;
    if let Some(account) = accounts
        .accounts
        .iter_mut()
        .find(|a| a.user_id.as_deref() == Some(captured.user_id.as_str()))
    {
        account.jwt = captured.token.clone();
        if let Some(refresh_token) = captured.refresh_token.clone() {
            account.refresh_token = Some(refresh_token);
        }
        effective_refresh_token = account
            .refresh_token
            .clone()
            .filter(|token| !token.trim().is_empty());
        account.credential_source = Some(if account.refresh_token.as_ref().is_some_and(|token| !token.is_empty()) {
            "native"
        } else {
            "bitbrowser"
        }.to_string());
        if account.name.trim().is_empty() || account.name.starts_with("Trae Work-…") {
            account.name = name.clone();
        }
        account.updated_at = Some(crate::fs_utils::now_iso());
        updated = true;
    } else {
        effective_refresh_token = captured
            .refresh_token
            .clone()
            .filter(|token| !token.trim().is_empty());
        accounts.accounts.push(crate::models::RawAccount {
            name: name.clone(),
            user_id: Some(captured.user_id.clone()),
            jwt: captured.token.clone(),
            refresh_token: captured.refresh_token.clone(),
            added_at: Some(crate::fs_utils::now_iso()),
            updated_at: Some(crate::fs_utils::now_iso()),
            dc_id: None,
            credential_source: Some(if captured.refresh_token.is_some() { "native" } else { "bitbrowser" }.to_string()),
        });
    }
    crate::vault::save_accounts(state, &mut accounts)?;

    // BitBrowser 页面可能长时间未刷新，捕获到的 JWT 已接近过期。只要账号池
    // 已有 refresh_token，就先走一次本机 ExchangeToken 换出新 access token，
    // 这样首次接管不会把一个“刚导入即过期”的 JWT 写进原生快照。
    let mut snapshot_token = captured.token.clone();
    let near_expiry = captured
        .exp_timestamp
        .map(|exp| exp <= chrono::Utc::now().timestamp() + 300)
        .unwrap_or(false);
    if effective_refresh_token.is_some() && near_expiry {
        match crate::commands::accounts::refresh_jwt_impl(state, &captured.user_id) {
            Ok(token) => snapshot_token = token,
            Err(error) => crate::fs_utils::app_log(
                &state.data_dir,
                &format!("BitBrowser 接管时原生 JWT 预刷新失败: uid={} error={error}", captured.user_id),
            ),
        }
    }

    // BitBrowser 页面 token 只存在网页 localStorage，原生切换桥要求 profiles/<uid>
    // 下有 icube 快照。首次导入立即生成，后续续期只原子更新快照中的 storage.json，
    // 因此用户无需再手动登录 TRAE Work CN 或点击“保存当前登录态”。快照失败不回滚
    // 账号池导入（网页 JWT 仍可通过 BitBrowser 续期），但把原因返回到 UI 便于处理。
    let snapshot = crate::native_snapshot::ensure_native_snapshot(
        state,
        &captured.user_id,
        &snapshot_token,
        effective_refresh_token.as_deref(),
        &name,
    );
    let (native_snapshot_created, native_snapshot_updated, native_snapshot_error) = match snapshot {
        Ok(outcome) => (outcome.created, outcome.updated, None),
        Err(error) => {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("BitBrowser 账号原生快照生成失败: uid={} error={error}", captured.user_id),
            );
            (false, false, Some(error))
        }
    };

    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "BitBrowser Trae Work 账号{}: uid={} profile={}",
            if updated { "JWT 更新" } else { "导入" },
            captured.user_id,
            profile.id
        ),
    );
    Ok(BitBrowserImportResult {
        user_id: captured.user_id.clone(),
        name,
        updated,
        token_exp_timestamp: captured.exp_timestamp,
        refresh_token_captured: captured.refresh_token.is_some(),
        native_snapshot_created,
        native_snapshot_updated,
        native_snapshot_error,
    })
}

/// 在 BitBrowser 所有 Trae Work profile 中按 UID 查找并重捕获登录态。
/// profile 删除后重建也不受影响：只要新 profile 中仍登录同一 UID，即可重新匹配。
#[tauri::command(async)]
pub fn bitbrowser_account_renew(
    state: State<AppState>,
    user_id: String,
) -> Result<BitBrowserImportResult, String> {
    let uid = user_id.trim();
    if uid.is_empty() || uid.len() > 64 || !uid.chars().all(|c| c.is_ascii_digit()) {
        return Err("无效的账号 UserID".into());
    }
    let base = local_api_base(&state)?;
    let profiles = list_profiles(&base)?;
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!("BitBrowser Trae Work JWT 真刷新开始: uid={uid}, profiles={}", profiles.len()),
    );
    let mut last_error = String::new();
    for (index, listed) in profiles.into_iter().enumerate() {
        if index > 0 {
            std::thread::sleep(DETAIL_INTERVAL);
        }
        // /browser/list 通常已经带有 URL/备注。对已明确标记为 Trae 的 profile
        // 直接使用列表数据，避免 BitBrowser /browser/detail 的频率限制；只有
        // 无法从列表判断时才补一次 detail 查询。
        let profile = if is_trae_hint(&listed.hint) {
            listed.clone()
        } else {
            match detail_profile(&base, &listed.id, &listed) {
                Ok(p) => p,
                Err(error) => {
                    last_error = error;
                    continue;
                }
            }
        };
        if !is_trae_hint(&profile.hint) {
            continue;
        }
        match capture_profile_refreshed(&base, &profile, Some(uid)) {
            Ok(captured) if captured.user_id == uid => {
                return persist_captured_token(&state, &profile, &captured, None);
            }
            Ok(captured) => {
                last_error = format!("profile 捕获到其他 UID {}", captured.user_id);
            }
            Err(error) => last_error = error,
        }
    }
    if last_error.is_empty() {
        Err(format!("BitBrowser 中没有找到已登录 Trae Work 账号 {uid}"))
    } else {
        Err(format!("BitBrowser 中没有找到账号 {uid}；最后一次读取信息：{last_error}"))
    }
}

/// 从指定 BitBrowser profile 重新捕获 JWT 并导入账号池。
/// expected_user_id 用于防止用户在扫描后切换 profile 登录账号造成错绑。
#[tauri::command(async)]
pub fn bitbrowser_account_import(
    state: State<AppState>,
    window_id: String,
    expected_user_id: Option<String>,
    account_name: Option<String>,
) -> Result<BitBrowserImportResult, String> {
    let id = window_id.trim();
    if id.is_empty() || id.len() > 200 || !id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')) {
        return Err("无效的 BitBrowser profile id".into());
    }
    let base = local_api_base(&state)?;
    let listed = BrowserProfile {
        id: id.to_string(),
        seq: None,
        name: None,
        status: None,
        hint: String::new(),
    };
    let profile = detail_profile(&base, id, &listed)?;
    if !is_trae_hint(&profile.hint) {
        return Err("指定的 BitBrowser profile 不是 Trae Work 页面".into());
    }
    let mut captured = capture_profile(&base, &profile)?;
    if let Some(expected) = expected_user_id.as_deref().filter(|s| !s.trim().is_empty()) {
        if expected.trim() != captured.user_id {
            return Err(format!(
                "账号已变化：扫描时为 {}，当前页面为 {}，已取消导入",
                expected.trim(), captured.user_id
            ));
        }
    }

    // 首次 BitBrowser 登录完成后，自动做一次短时原生 OAuth 握手，尽量把
    // refresh_token 一并接管。这样后续续期可完全走本机 Trae ExchangeToken，
    // BitBrowser 只承担本次首次登录；若授权页需要人工确认，保留已捕获 JWT，
    // 不阻断账号入池和快照生成。
    if captured.refresh_token.is_none() {
        match crate::commands::oauth::native_oauth_exchange_for_bitbrowser(
            &state,
            id,
            // BitBrowser 首次打开授权页可能需要几十秒加载/跳转；25 秒会把
            // 正常的授权流程误判为失败，随后只留下很快过期的网页 JWT。
            Duration::from_secs(90),
        ) {
            Ok(exchange) if exchange.user_id == captured.user_id => {
                let info = crate::jwt::parse(&exchange.jwt);
                captured = CapturedToken {
                    token: exchange.jwt,
                    refresh_token: Some(exchange.refresh_token),
                    user_id: exchange.user_id,
                    exp_timestamp: info.exp_timestamp,
                    page_url: captured.page_url,
                };
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("BitBrowser 首次登录已自动接管原生 refresh_token: uid={} profile={}", captured.user_id, id),
                );
            }
            Ok(exchange) => {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("BitBrowser 原生 OAuth 返回其他 UID，保留网页 JWT: expected={} actual={}", captured.user_id, exchange.user_id),
                );
            }
            Err(error) => {
                crate::fs_utils::app_log(
                    &state.data_dir,
                    &format!("BitBrowser 首次原生 OAuth 自动接管未完成，保留网页 JWT: uid={} error={error}", captured.user_id),
                );
            }
        }
    }

    persist_captured_token(&state, &profile, &captured, account_name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trae_hint_accepts_encoded_work_url() {
        assert!(is_trae_hint("https%3A%2F%2Fwww.trae.cn%2Fwork"));
        assert!(is_trae_hint("TRAE SOLO CN"));
        assert!(!is_trae_hint("https://example.com"));
    }

    #[test]
    fn profile_parses_string_id_and_status() {
        let p = profile_from_value(&json!({
            "id": "abc-123",
            "seq": 20,
            "status": 1,
            "url": "https%3A%2F%2Fwww.trae.cn%2Fwork"
        }))
        .unwrap();
        assert_eq!(p.id, "abc-123");
        assert_eq!(p.seq, Some(20));
        assert_eq!(p.status, Some(true));
        assert!(is_trae_hint(&p.hint));
    }

    #[test]
    fn token_normalisation_removes_auth_prefixes() {
        assert_eq!(normalise_token("Bearer abc.def.ghi"), "abc.def.ghi");
        assert_eq!(normalise_token("Cloud-IDE-JWT abc.def.ghi"), "abc.def.ghi");
    }
}
