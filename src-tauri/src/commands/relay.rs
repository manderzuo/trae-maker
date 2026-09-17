//! Trae Work CN 的安全「切换并继续工作」辅助。
//!
//! 第一批只读取客户端最近项目列表并保存接力包，然后在切号后重新打开项目。
//! 不复制账号专属的 state.vscdb / IndexedDB，也不把会话内容写入快照，避免把
//! 一个账号的登录凭证带到另一个账号。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tauri::State;
use tungstenite::{connect, Message};

use crate::models::DeviceEntry;
use crate::fs_utils;
use crate::state::AppState;

const STORAGE_SUFFIX: &str = r"User\globalStorage\storage.json";
const VSCDB_SUFFIX: &str = r"User\globalStorage\state.vscdb";
// Trae CN 当前远程 API 域名由启动时的网关配置决定。优先从代理日志读取实际域名，
// 只有在代理尚未记录过请求时才使用这个公开 CN 默认值。
const DEFAULT_SHARE_REMOTE_BASE: &str = "https://api5-normal.mchost.guru";
const SHARE_ORIGIN: &str = "https://share.traecontent.cn";
const SHARE_MAX_MESSAGES: usize = 100; // Trae 原生分享上限：最多 50 个 user + 50 个 assistant
const TRAE_CDP_DEFAULT_PORT: u16 = 9333;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraeRelayPackage {
    pub schema_version: u32,
    pub captured_at: String,
    pub source_uid: Option<String>,
    /// 最近一次打开的 Trae 会话 ID（不含凭证）。
    #[serde(default)]
    pub session_id: Option<String>,
    pub target_app: String,
    pub project_paths: Vec<String>,
    pub storage_path: Option<String>,
    pub state_db_path: Option<String>,
    pub notes: Vec<String>,
    /// 切号前生成的分享信息（旧接力包没有这些字段，serde default 保持兼容）。
    #[serde(default)]
    pub share_session_id: Option<String>,
    #[serde(default)]
    pub share_url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraeShareResult {
    pub session_id: String,
    pub share_session_id: String,
    pub share_url: String,
    pub status: String,
    pub title: String,
    pub resource_count: usize,
    pub unavailable_files: Vec<String>,
}

#[derive(Debug, Clone)]
struct LocalResource {
    source: PathBuf,
    path: String,
    kind: String,
    content_hash: String,
    size: u64,
    content_type: String,
}

fn app_data_dirs(target_app: &str) -> Vec<PathBuf> {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    match target_app {
        "TraeWork" => vec![
            PathBuf::from(&appdata).join("TRAE SOLO CN"),
            PathBuf::from(&appdata).join("TRAE SOLO"),
        ],
        "Trae" => vec![PathBuf::from(&appdata).join("Trae CN")],
        _ => vec![],
    }
}

fn normalize_target(target_app: Option<String>) -> Result<String, String> {
    let target = target_app.unwrap_or_else(|| "TraeWork".to_string());
    match target.as_str() {
        "TraeWork" | "Trae" => Ok(target),
        _ => Err("接力目前只支持 Trae Work CN 或 Trae CN".into()),
    }
}

fn decode_file_uri(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let decoded = urlencoding::decode(trimmed)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| trimmed.to_string());
    let mut path = if let Some(rest) = decoded.strip_prefix("file:///") {
        rest.to_string()
    } else if let Some(rest) = decoded.strip_prefix("file://") {
        rest.trim_start_matches('/').to_string()
    } else {
        decoded
    };
    // Windows file URI 的盘符前会多一个斜杠，UNC 路径则保留双反斜杠语义。
    if path.len() >= 3 && path.as_bytes()[0] == b'/' && path.as_bytes()[2] == b':' {
        path.remove(0);
    }
    path = path.replace('/', "\\");
    let p = PathBuf::from(path);
    if p.exists() {
        Some(p.to_string_lossy().to_string())
    } else {
        // 最近打开的项目可能已被移动；保留绝对路径，交给打开命令给出明确提示。
        if p.is_absolute() {
            Some(p.to_string_lossy().to_string())
        } else {
            None
        }
    }
}

fn add_candidate(value: &str, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    if let Some(path) = decode_file_uri(value) {
        let key = path.to_ascii_lowercase();
        if seen.insert(key) {
            out.push(path);
        }
    }
}

fn collect_path_values(value: &serde_json::Value, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        serde_json::Value::String(s) => {
            // storage.json / state.vscdb 的列表值有两个版本：直接 JSON 对象，或
            // 被再次序列化成字符串。先尝试解包，失败再按路径字符串处理。
            if let Ok(nested) = serde_json::from_str::<serde_json::Value>(s) {
                collect_path_values(&nested, out, seen);
            } else {
                add_candidate(s, out, seen);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                collect_path_values(item, out, seen);
            }
        }
        serde_json::Value::Object(obj) => {
            for key in ["folderUri", "fileUri", "workspaceUri", "path", "uri"] {
                if let Some(v) = obj.get(key) {
                    collect_path_values(v, out, seen);
                }
            }
            // VS Code 的 recentlyOpenedPathsList 通常放在 entries 下；兼容其它版本
            // 使用 workspaces / files / entries 的结构。
            for key in ["entries", "workspaces", "files", "folders"] {
                if let Some(v) = obj.get(key) {
                    collect_path_values(v, out, seen);
                }
            }
        }
        _ => {}
    }
}

fn extract_project_paths(value: &serde_json::Value, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    match value {
        serde_json::Value::Object(obj) => {
            for (key, child) in obj {
                let lower = key.to_ascii_lowercase();
                if lower.contains("recentlyopenedpathslist")
                    || lower == "local-project-folders"
                    || lower == "localprojectfolders"
                {
                    collect_path_values(child, out, seen);
                }
                // 某些版本将键包在 workbench/state 对象内，继续递归寻找同名键。
                extract_project_paths(child, out, seen);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                extract_project_paths(item, out, seen);
            }
        }
        _ => {}
    }
}

fn read_json_paths(path: &Path, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    extract_project_paths(&value, out, seen);
}

fn read_vscdb_paths(path: &Path, out: &mut Vec<String>, seen: &mut HashSet<String>) {
    let Ok(conn) = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    ) else {
        return;
    };
    let Ok(mut stmt) = conn.prepare("SELECT key, value FROM ItemTable") else { return };
    let Ok(rows) = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?.unwrap_or_default()))
    }) else {
        return;
    };
    for row in rows.flatten() {
        let (key, raw) = row;
        let lower = key.to_ascii_lowercase();
        if lower.contains("recentlyopenedpathslist") || lower.contains("local-project-folders") {
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) {
                collect_path_values(&value, out, seen);
            }
        }
    }
}

fn file_mtime(path: &Path) -> std::time::SystemTime {
    path.metadata()
        .and_then(|m| m.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}

fn renderer_logs(target_app: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for dir in app_data_dirs(target_app) {
        let root = dir.join("logs");
        let Ok(runs) = fs::read_dir(root) else { continue };
        for run in runs.flatten() {
            let run_dir = run.path();
            if !run_dir.is_dir() { continue; }
            let Ok(windows) = fs::read_dir(run_dir) else { continue };
            for window in windows.flatten() {
                let candidate = window.path().join("renderer.log");
                if candidate.is_file() { out.push(candidate); }
            }
        }
    }
    out.sort_by_key(|p| std::cmp::Reverse(file_mtime(p)));
    out
}

fn json_string_field(line: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\":\"");
    let start = line.rfind(&needle)? + needle.len();
    let tail = &line[start..];
    let end = tail.find('"')?;
    let value = &tail[..end];
    if value.is_empty() { None } else { Some(value.to_string()) }
}

/// Trae 的会话内容存放在云端，客户端日志会记录最近一次读取/更新的会话 ID。
/// 这里只读日志中的 ID，不读取或落盘 JWT、Cookie 等凭证。
fn detect_current_session_id(target_app: &str) -> Option<String> {
    for path in renderer_logs(target_app).into_iter().take(8) {
        let Ok(text) = fs::read_to_string(path) else { continue };
        for line in text.lines().rev() {
            if let Some(id) = json_string_field(line, "sessionId")
                .or_else(|| json_string_field(line, "chat_session_id"))
            {
                if id.len() >= 8 && id.len() <= 128 { return Some(id); }
            }
        }
    }
    None
}

fn proxy_log_files(log_dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = fs::read_dir(log_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(|name| name.starts_with("proxy_req_") && name.ends_with(".log"))
                .unwrap_or(false)
        })
        .collect();
    out.sort_by_key(|p| std::cmp::Reverse(file_mtime(p)));
    out
}

/// 读取最近一次原生 Trae 请求使用的远程域名，避免把部署环境写死。
fn detect_share_remote_base(log_dir: &Path) -> String {
    if let Ok(value) = std::env::var("TRAE_SHARE_REMOTE_BASE") {
        let value = value.trim().trim_end_matches('/');
        if value.starts_with("https://") && !value.contains([' ', '\"', '\'']) {
            return value.to_string();
        }
    }
    for path in proxy_log_files(log_dir).into_iter().take(4) {
        let Ok(text) = fs::read_to_string(path) else { continue };
        for line in text.lines().rev() {
            let Some(pos) = line.find("] POST ") else { continue };
            let rest = &line[pos + 7..];
            let Some(path_pos) = rest.find("/api/remote/v1/share") else { continue };
            let host = rest[..path_pos].trim();
            if !host.is_empty() && !host.contains(['/', ' ', ':']) {
                return format!("https://{host}");
            }
        }
    }
    DEFAULT_SHARE_REMOTE_BASE.to_string()
}

fn validate_session_id(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err("无效的 Trae 会话 ID".into());
    }
    Ok(value.to_string())
}

fn auth_value(jwt: &str) -> String {
    if jwt.trim_start().starts_with("Cloud-IDE-JWT ") {
        jwt.trim().to_string()
    } else {
        format!("Cloud-IDE-JWT {}", jwt.trim())
    }
}

fn share_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(300))
        .build()
}

fn share_request(req: ureq::Request, jwt: &str, device: &DeviceEntry) -> ureq::Request {
    let request_id = crate::commands::oauth::random_hex(32);
    let trace_id = format!("00-{}-01", crate::commands::oauth::random_hex(16));
    // Trae Work 的 remote/share 接口以 x-ide-token 为主鉴权头；保留
    // authorization 兼容旧账号/旧网关，但必须同时发送不带前缀的原始 JWT。
    let raw_jwt = jwt
        .trim()
        .strip_prefix("Cloud-IDE-JWT ")
        .unwrap_or(jwt.trim());
    let machine_id = crate::api_server::pool::seeded_hex(64, &device.device_id, "mach");
    let mut req = req
        .set("authorization", &auth_value(jwt))
        .set("x-ide-token", raw_jwt)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("x-trae-client-type", "lite")
        .set("x-preferenced-language", "zh-CN")
        .set("x-trae-user-timezone", "Asia/Shanghai")
        .set("x-user-region", "CN")
        .set("user-agent", "VSCode 1.107.1 (TRAE SOLO CN)")
        .set("origin", "vscode-file://vscode-app")
        .set("x-app-id", "6eefa01c-1036-4c7e-9ca5-d891f63bfcd8")
        .set("x-app-version", "default")
        .set("x-app-version-code", "20260901")
        .set("x-ide-version", "0.1.65")
        .set("x-ide-version-code", "20260901")
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-device-brand", "CREFG-XX")
        .set("x-device-cpu", "Intel")
        .set("x-machine-id", &machine_id)
        .set("request-traffic-type", "prod")
        .set("x-market-client-id", "VSCode 1.107.1")
        .set("x-market-user-id", device.market_user_id.as_deref().unwrap_or(""))
        .set("x-device-id", &device.device_id)
        .set("x-lgw-req-sdk-type", "3")
        .set("package-type", "stable_cn")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("app-version", "0.1.65")
        .set("x-request-id", &request_id)
        .set("x-ss-dp", "787976")
        .set("x-tt-trace-id", &trace_id);
    if let Some(session_id) = device.session_id.as_deref() {
        if !session_id.is_empty() {
            req = req.set("vscode-sessionid", session_id);
        }
    }
    req
}

fn response_json(resp: ureq::Response, operation: &str) -> Result<Value, String> {
    let body: Value = resp
        .into_json()
        .map_err(|e| format!("{operation} 响应解析失败：{e}"))?;
    let code = body.get("code").and_then(|v| {
        v.as_i64().or_else(|| v.as_str().and_then(|text| text.parse::<i64>().ok()))
    }).unwrap_or(0);
    if code != 0 {
        let msg = body.get("message").and_then(Value::as_str).unwrap_or("未知错误");
        return Err(format!("{operation} 失败（code={code}）：{msg}"));
    }
    Ok(body)
}

fn share_error(error: ureq::Error, operation: &str) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let body = response.into_string().unwrap_or_default();
            let snippet: String = body.chars().take(240).collect();
            format!("{operation} HTTP {code}：{snippet}")
        }
        other => format!("{operation} 请求失败：{other}"),
    }
}

/// 将分享链路的失败记录到 app.log，但不把响应正文、URL 查询参数或任何凭证写入日志。
/// 这条链路会处理会话内容和签名上传地址，日志只保留阶段、HTTP 状态/错误类别。
fn share_error_label(error: &str) -> String {
    let compact = error.replace(['\r', '\n'], " ");
    for marker in [
        "HTTP 400", "HTTP 401", "HTTP 403", "HTTP 404", "HTTP 408",
        "HTTP 409", "HTTP 429", "HTTP 500", "HTTP 502", "HTTP 503",
        "响应解析失败", "请求失败", "没有可分享", "上传地址为空", "资源 URI 为空",
    ] {
        if compact.contains(marker) {
            return marker.to_string();
        }
    }
    if compact.contains("code=") {
        return compact
            .split("：")
            .next()
            .unwrap_or("接口返回业务错误")
            .chars()
            .take(80)
            .collect();
    }
    "未分类错误".to_string()
}

fn log_share_failure(data_dir: &Path, stage: &str, error: &str) {
    fs_utils::app_log(
        data_dir,
        &format!("Trae 分享创建失败: stage={stage} kind={}", share_error_label(error)),
    );
}

fn post_share_json(agent: &ureq::Agent, url: &str, jwt: &str, device: &DeviceEntry, body: &Value, operation: &str) -> Result<Value, String> {
    let req = share_request(agent.post(url), jwt, device);
    let resp = req.send_json(body).map_err(|e| share_error(e, operation))?;
    response_json(resp, operation)
}

fn get_share_json(agent: &ureq::Agent, url: &str, jwt: &str, device: &DeviceEntry, operation: &str) -> Result<Value, String> {
    let req = share_request(agent.get(url), jwt, device);
    let resp = req.call().map_err(|e| share_error(e, operation))?;
    response_json(resp, operation)
}

fn data_of(body: Value) -> Value {
    let mut current = body;
    // Trae renderer → Aha → lite 适配器会产生两层 {code,message,data} 包装，
    // 远端 HTTP 通常只有一层。按“确实是响应包装”的形状逐层解包，避免
    // 误把普通业务对象中名为 data 的字段剥掉。
    loop {
        let is_envelope = current.get("code").is_some()
            && current.get("message").is_some()
            && current.get("data").is_some();
        if !is_envelope {
            return current;
        }
        current = current.get("data").cloned().unwrap_or(Value::Null);
    }
}

fn timestamp_iso(value: Option<&Value>) -> String {
    if let Some(value) = value {
        if let Some(text) = value.as_str() {
            if let Ok(dt) = DateTime::parse_from_rfc3339(text) {
                return dt.with_timezone(&Utc).to_rfc3339();
            }
            if let Ok(number) = text.parse::<i64>() {
                return timestamp_iso(Some(&Value::Number(number.into())));
            }
        }
        if let Some(number) = value.as_i64() {
            let millis = if number.abs() < 1_000_000_000_000 { number * 1000 } else { number };
            if let Some(dt) = DateTime::<Utc>::from_timestamp_millis(millis) {
                return dt.to_rfc3339();
            }
        }
    }
    Utc::now().to_rfc3339()
}

fn scrub_value(value: &Value) -> Value {
    const FORBIDDEN: [&str; 8] = [
        "agent_run_id", "sub_agent_call_description", "confirm_info", "meta",
        "already_emitted_generating_event", "already_emitted_run_event", "error_message", "interrupt",
    ];
    match value {
        Value::Array(items) => Value::Array(items.iter().map(scrub_value).collect()),
        Value::Object(obj) => {
            let mut out = Map::new();
            for (key, child) in obj {
                if FORBIDDEN.iter().any(|blocked| key == blocked) { continue; }
                out.insert(key.clone(), scrub_value(child));
            }
            Value::Object(out)
        }
        _ => value.clone(),
    }
}

fn scrub_serialized(value: Option<&Value>) -> Option<Value> {
    let value = value?;
    if let Some(text) = value.as_str() {
        if let Ok(parsed) = serde_json::from_str::<Value>(text) {
            return Some(Value::String(serde_json::to_string(&scrub_value(&parsed)).unwrap_or_else(|_| text.to_string())));
        }
    }
    Some(scrub_value(value))
}

fn sanitize_message(raw: &Value) -> Option<Value> {
    let object = raw.as_object()?;
    let role = object.get("role")?.as_str()?;
    if role != "user" && role != "assistant" { return None; }
    let mut out = Map::new();
    for key in [
        "message_id", "turn_id", "message_type", "role", "message_index", "reply_to_message_id",
        "references", "search_reference_data", "doc_references", "agent_type", "agent_name",
        "agent_avatar_id", "from_append_msg", "chat_start_time", "chat_end_time",
    ] {
        if let Some(value) = object.get(key) { out.insert(key.to_string(), scrub_value(value)); }
    }
    out.insert("status".into(), object.get("status").cloned().unwrap_or_else(|| Value::String("completed".into())));
    if let Some(content) = scrub_serialized(object.get("content")) { out.insert("content".into(), content); }
    if let Some(query) = scrub_serialized(object.get("query")) { out.insert("query".into(), query); }
    if let Some(context) = object.get("user_message_context") {
        if let Some(hidden) = context.get("hide_user_query") {
            out.insert("hide_user_query".into(), hidden.clone());
        }
    }
    if let Some(created) = object.get("created_at") {
        out.insert("created_at".into(), Value::String(timestamp_iso(Some(created))));
    }
    Some(Value::Object(out))
}

fn collect_named_strings(value: &Value, field: &str, out: &mut Vec<String>) {
    match value {
        Value::Array(items) => for item in items { collect_named_strings(item, field, out); },
        Value::Object(obj) => {
            if let Some(Value::String(path)) = obj.get(field) { out.push(path.clone()); }
            for child in obj.values() { collect_named_strings(child, field, out); }
        }
        _ => {}
    }
}

fn safe_name(value: &str) -> String {
    let normalized = value.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap_or("resource");
    let filtered: String = name.chars().map(|c| {
        if c.is_control() || matches!(c, '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c }
    }).collect();
    if filtered.is_empty() { "resource".into() } else { filtered }
}

fn local_path_from_uri(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.to_ascii_lowercase().starts_with("trae-res://") { return None; }
    let decoded = urlencoding::decode(trimmed).ok()?.into_owned();
    let mut path = if let Some(rest) = decoded.strip_prefix("file:///") {
        rest.to_string()
    } else if let Some(rest) = decoded.strip_prefix("file://") {
        rest.trim_start_matches('/').to_string()
    } else if decoded.to_ascii_lowercase().starts_with("computer:///") {
        return None;
    } else { decoded };
    if path.len() >= 3 && path.as_bytes().first() == Some(&b'/') && path.as_bytes().get(2) == Some(&b':') {
        path.remove(0);
    }
    let path = PathBuf::from(path.replace('/', "\\"));
    path.is_file().then_some(path)
}

fn content_type_for(path: &Path) -> &'static str {
    match path.extension().and_then(|v| v.to_str()).unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png", "jpg" | "jpeg" => "image/jpeg", "gif" => "image/gif",
        "webp" => "image/webp", "mp4" => "video/mp4", "webm" => "video/webm",
        "mov" => "video/quicktime", "html" | "htm" => "text/html", "txt" => "text/plain",
        "json" => "application/json", _ => "application/octet-stream",
    }
}

fn add_local_resource(resources: &mut Vec<LocalResource>, seen: &mut HashSet<String>, source: PathBuf, path: String, kind: &str) -> Result<(), String> {
    let key = source.to_string_lossy().to_ascii_lowercase();
    if !seen.insert(key) { return Ok(()); }
    let bytes = fs::read(&source).map_err(|e| format!("读取分享资源失败 {}：{e}", source.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let hash = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect::<String>();
    resources.push(LocalResource {
        size: bytes.len() as u64,
        content_hash: format!("sha256:{hash}"),
        content_type: content_type_for(&source).to_string(),
        source,
        path,
        kind: kind.to_string(),
    });
    Ok(())
}

fn unique_resource_path(resources: &[LocalResource], candidate: String) -> String {
    if !resources.iter().any(|resource| resource.path == candidate) {
        return candidate;
    }
    let (stem, extension) = match candidate.rsplit_once('.') {
        Some((stem, extension)) if !stem.ends_with('/') => (stem.to_string(), format!(".{extension}")),
        _ => (candidate, String::new()),
    };
    for index in 2..=10_000 {
        let path = format!("{stem}-{index}{extension}");
        if !resources.iter().any(|resource| resource.path == path) {
            return path;
        }
    }
    // 上限只在异常巨量资源时触发；调用方随后会按资源数上限报错。
    format!("{stem}-{}", resources.len() + 1)
}

fn save_relay_package(data_dir: &Path, package: &TraeRelayPackage) -> Result<(), String> {
    let dir = data_dir.join("trae_relay");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建接力目录失败：{e}"))?;
    let stamp = Utc::now().format("%Y%m%d-%H%M%S%.3f").to_string().replace('.', "_");
    crate::fs_utils::write_json(&dir.join(format!("relay-{stamp}.json")), package)
}

/// 读取 Trae 当前最近项目并生成一个不含凭证的接力包。
#[tauri::command(async)]
pub fn trae_relay_capture(
    state: State<AppState>,
    target_app: Option<String>,
) -> Result<TraeRelayPackage, String> {
    let target_app = normalize_target(target_app)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 接力抓取开始: target_app={target_app}"),
    );
    let dirs = app_data_dirs(&target_app);
    let storage = dirs
        .iter()
        .map(|dir| dir.join(STORAGE_SUFFIX))
        .find(|path| path.is_file());
    let state_db = dirs
        .iter()
        .map(|dir| dir.join(VSCDB_SUFFIX))
        .find(|path| path.is_file());
    let mut project_paths = Vec::new();
    let mut seen = HashSet::new();
    if let Some(path) = &storage {
        read_json_paths(path, &mut project_paths, &mut seen);
    }
    if let Some(path) = &state_db {
        read_vscdb_paths(path, &mut project_paths, &mut seen);
    }
    let source_uid = crate::commands::trae_apps::infer_current_cloud_uid(&target_app);
    let session_id = detect_current_session_id(&target_app);
    let mut notes = Vec::new();
    if storage.is_none() && state_db.is_none() {
        notes.push("未找到 Trae globalStorage；请先启动并打开过一个项目".into());
    }
    if project_paths.is_empty() {
        notes.push("未解析到最近项目路径；切号仍可继续，但不会自动重开项目".into());
    }
    if session_id.is_none() {
        notes.push("未从 Trae renderer.log 解析到当前会话 ID；自动分享需要先在 Trae 中打开目标对话".into());
    }
    let package = TraeRelayPackage {
        schema_version: 1,
        captured_at: Utc::now().to_rfc3339(),
        source_uid,
        session_id,
        target_app,
        project_paths,
        storage_path: storage.map(|p| p.to_string_lossy().to_string()),
        state_db_path: state_db.map(|p| p.to_string_lossy().to_string()),
        notes,
        share_session_id: None,
        share_url: None,
    };
    save_relay_package(&state.data_dir, &package)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 接力抓取完成: project_paths={} session_detected={}", package.project_paths.len(), package.session_id.is_some()),
    );
    Ok(package)
}

fn account_jwt(state: &AppState, user_id: &str) -> Result<String, String> {
    let accounts = crate::vault::load_accounts(state);
    let account = accounts
        .accounts
        .iter()
        .find(|account| account.user_id.as_deref() == Some(user_id))
        .ok_or_else(|| format!("账号 {user_id} 不在账号池中"))?;
    if account.jwt.trim().is_empty() {
        return Err(format!("账号 {user_id} 没有可用 JWT"));
    }
    Ok(account.jwt.trim().to_string())
}

fn messages_from_data(data: &Value) -> Vec<Value> {
    data.get("items")
        .and_then(Value::as_array)
        .cloned()
        .or_else(|| data.as_array().cloned())
        .unwrap_or_default()
}

/// 读取 Trae Work 的页面 CDP 目标。Trae Work 的会话数据不在 renderer 的
/// HTTP 层，而是由页面里的 `window.vscode.ahaIpc` 转发到内置 ai-agent。
/// 这里只连接 127.0.0.1，绝不扫描或连接远端调试端口。
fn trae_cdp_target() -> Result<String, String> {
    let port = std::env::var("AIWORK_TRAE_CDP_PORT")
        .ok()
        .and_then(|value| value.trim().parse::<u16>().ok())
        .unwrap_or(TRAE_CDP_DEFAULT_PORT);
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(2))
        .timeout_read(Duration::from_secs(3))
        .build();
    let body = agent
        .get(&format!("http://127.0.0.1:{port}/json/list"))
        .call()
        .map_err(|e| format!("Trae Work 本地调试桥未启动（127.0.0.1:{port}）：{e}"))?
        .into_string()
        .map_err(|e| format!("读取 Trae Work 本地调试目标失败：{e}"))?;
    let targets: Vec<Value> = serde_json::from_str(&body)
        .map_err(|e| format!("解析 Trae Work 本地调试目标失败：{e}"))?;
    targets
        .iter()
        .filter(|target| target.get("type").and_then(Value::as_str) == Some("page"))
        .filter_map(|target| target.get("webSocketDebuggerUrl").and_then(Value::as_str))
        .next()
        .map(str::to_string)
        .ok_or_else(|| "Trae Work 本地调试桥没有可用页面目标；请先打开 Trae Work 主窗口".into())
}

fn random_connect_session_id() -> String {
    // 与 Trae 的 icube.cloudide.aiSessionID 一样使用 UUID 形态；这个值只作为
    // Aha 请求的连接关联 ID，不写入账号池，也不包含任何登录凭证。
    format!(
        "{}-{}-4{}-a{}-{}",
        crate::commands::oauth::random_hex(4),
        crate::commands::oauth::random_hex(2),
        crate::commands::oauth::random_hex(3),
        crate::commands::oauth::random_hex(3),
        crate::commands::oauth::random_hex(6),
    )
}

/// 在 Trae renderer 内调用一次普通 JSON-RPC `request`。Aha IPC 的 packet
/// 封装、server id 协商、心跳都由 Trae 页面自己的 `ahaIpc` 客户端负责，
/// AI Work Assistant 不会直接碰 Windows named pipe，也不会复制账号凭证。
///
/// `request_lite` 只允许极少数专用服务（例如 pty_bridge），`lite` 会被
/// ai-agent 明确拒绝。因此这里必须复刻 Trae 自身 TransportManager 的
/// `request` 外层包，并携带 user_info/client_info 等字段。
fn trae_lite_call(
    session_id: &str,
    method: &str,
    data: Value,
    user_info: &Value,
) -> Result<Value, String> {
    let ws_url = trae_cdp_target()?;
    let (mut socket, _) = connect(&ws_url)
        .map_err(|e| format!("连接 Trae Work 本地调试桥失败：{e}"))?;
    let connect_session_id = random_connect_session_id();
    let channel_id = random_connect_session_id();
    let request = json!({
        "packet_type": "request",
        "channel_id": channel_id,
        "session_id": connect_session_id,
        "params": {
            "service": "lite",
            "method": method,
            "data": data,
            "user_info": user_info,
            "common_params": {},
            "streamlined_common_params": {},
            "client_info": {
                "connect_session_id": connect_session_id,
                "chat_session_id": session_id,
            }
        }
    });
    let request_id = "aiwork-1";
    let request_json = serde_json::to_string(&request)
        .map_err(|e| format!("序列化 Trae 本地请求失败：{e}"))?;
    let expression = format!(
        r#"(async () => {{
            const conn = await window.vscode?.ahaIpc?.connect("ai-agent");
            if (!conn) throw new Error("AhaIpc is not available");
            const id = {request_id:?};
            const request = {request_json};
            try {{
                const result = await new Promise((resolve, reject) => {{
                    let done = false;
                    const finish = (fn, value) => {{
                        if (done) return;
                        done = true;
                        clearTimeout(timer);
                        try {{ conn.off?.("message", onMessage); }} catch (_) {{}}
                        fn(value);
                    }};
                    const onMessage = (raw) => {{
                        try {{
                            const parsed = typeof raw === "string" ? JSON.parse(raw) : raw;
                            // ahaIpc 的 renderer 包装器已经把 packet.payload
                            // 解包后再触发 message；兼容原始 packet 形态。
                            const payload = parsed?.jsonrpc ? parsed
                                : (parsed && typeof parsed.payload === "string"
                                    ? JSON.parse(parsed.payload) : parsed?.payload);
                            if (!payload || String(payload.id) !== id) return;
                            if (payload.error) return finish(reject, new Error(payload.error.message || "Aha RPC error"));
                            finish(resolve, payload.result?.params ?? payload.result ?? null);
                        }} catch (_) {{}}
                    }};
                    const timer = setTimeout(() => finish(reject, new Error("Aha RPC response timeout")), 15000);
                    conn.on("message", onMessage);
                    conn.send(JSON.stringify({{jsonrpc:"2.0", id, method:"request", params:[request]}}));
                }});
                return result;
            }} finally {{
                try {{ await conn.disconnect?.(); }} catch (_) {{}}
            }}
        }})()"#,
        request_id = request_id,
        request_json = request_json,
    );
    let cdp_request = json!({
        "id": 1,
        "method": "Runtime.evaluate",
        "params": {
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
        }
    });
    socket
        .send(Message::Text(cdp_request.to_string().into()))
        .map_err(|e| format!("发送 Trae 本地会话请求失败：{e}"))?;
    loop {
        let message = socket
            .read()
            .map_err(|e| format!("读取 Trae 本地会话响应失败：{e}"))?;
        if !message.is_text() {
            continue;
        }
        let value: Value = serde_json::from_str(
            &message
                .into_text()
                .map_err(|e| format!("读取 Trae 本地会话文本失败：{e}"))?,
        )
        .map_err(|e| format!("解析 Trae 本地会话响应失败：{e}"))?;
        if value.get("id").and_then(Value::as_i64) != Some(1) {
            continue;
        }
        if let Some(error) = value.get("error") {
            return Err(format!("Trae 本地会话 Runtime.evaluate 失败：{error}"));
        }
        let result = value
            .get("result")
            .and_then(|v| v.get("result"))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(description) = result.get("description").and_then(Value::as_str) {
            return Err(format!("Trae 本地会话调用失败：{description}"));
        }
        let value = data_of(result.get("value").cloned().unwrap_or(Value::Null));
        if let Some(code) = value.get("code").and_then(Value::as_i64) {
            if code != 0 {
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("未知错误");
                return Err(format!("Trae 本地会话调用失败（code={code}）：{message}"));
            }
        }
        return Ok(value);
    }
}

fn collect_share_messages_via_local_bridge(
    session_id: &str,
    user_info: &Value,
) -> Result<(Value, Vec<Value>), String> {
    let session = trae_lite_call(
        session_id,
        "get_chat_session",
        json!({ "chat_session_id": session_id, "env": "local" }),
        user_info,
    )?;
    let session = data_of(session);
    let mut all_messages = Vec::new();
    let mut page_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();
    loop {
        let mut request_data = json!({
            "chat_session_id": session_id,
            "page_size": 100,
            "env": "local"
        });
        if let Some(token) = &page_token {
            request_data["page_token"] = Value::String(token.clone());
        }
        let page = data_of(trae_lite_call(
            session_id,
            "get_messages",
            request_data,
            user_info,
        )?);
        all_messages.extend(messages_from_data(&page));
        let next = page
            .get("next_page_token")
            .or_else(|| page.get("nextPageToken"))
            .and_then(Value::as_str)
            .map(str::to_string);
        if next.as_deref().is_none() || !seen_tokens.insert(next.clone().unwrap_or_default()) {
            break;
        }
        page_token = next;
    }
    let mut messages: Vec<Value> = all_messages
        .iter()
        .filter_map(sanitize_message)
        .collect();
    messages.sort_by_key(|message| {
        message
            .get("message_index")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    if messages.len() > SHARE_MAX_MESSAGES {
        messages = messages.split_off(messages.len() - SHARE_MAX_MESSAGES);
    }
    if messages.is_empty() {
        return Err("当前 Trae 会话没有可分享的 user/assistant 消息".into());
    }
    Ok((session, messages))
}

fn collect_share_messages(
    agent: &ureq::Agent,
    base: &str,
    session_id: &str,
    source_uid: &str,
    jwt: &str,
    device: &DeviceEntry,
) -> Result<(Value, Vec<Value>), String> {
    // 本地会话优先走 Trae renderer 内置 lite/Aha 桥。远端 HTTP 读取在
    // Trae Work CN 对 local session 稳定返回 404，禁止作为隐式降级路径。
    let user_info = json!({
        "name": source_uid,
        "token": jwt,
        "is_internal": false,
        "user_id": source_uid,
        "scope": "marscode",
        "loginScope": "trae",
    });
    let bridge_error = match collect_share_messages_via_local_bridge(session_id, &user_info) {
        Ok(value) => return Ok(value),
        Err(error) => error,
    };
    if !remote_local_chat_read_enabled() {
        return Err(format!(
            "Trae Work 本地会话桥接不可用：{bridge_error}；已停止远端会话读取（未切换账号、未上传内容）。请用带本地调试桥的 Trae Work 启动入口重开客户端"
        ));
    }
    let encoded = urlencoding::encode(session_id);
    let session_body = get_share_json(
        agent,
        &format!("{base}/api/remote/v1/chat_sessions/{encoded}?env=local"),
        jwt,
        device,
        "读取 Trae 会话",
    )?;
    let session = data_of(session_body);
    let mut all = Vec::new();
    let mut page_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();
    loop {
        let mut url = format!("{base}/api/remote/v1/chat_sessions/{encoded}/messages?page_size=100&env=local");
        if let Some(token) = &page_token {
            url.push_str("&page_token=");
            url.push_str(&urlencoding::encode(token));
        }
        let data = data_of(get_share_json(agent, &url, jwt, device, "读取 Trae 会话消息")?);
        all.extend(messages_from_data(&data));
        let next = data.get("next_page_token").and_then(Value::as_str).map(str::to_string);
        if next.as_deref().is_none() || !seen_tokens.insert(next.clone().unwrap_or_default()) {
            break;
        }
        page_token = next;
    }
    let mut messages: Vec<Value> = all.iter().filter_map(sanitize_message).collect();
    messages.sort_by_key(|message| {
        message
            .get("message_index")
            .and_then(Value::as_i64)
            .unwrap_or(i64::MAX)
    });
    if messages.len() > SHARE_MAX_MESSAGES {
        messages = messages.split_off(messages.len() - SHARE_MAX_MESSAGES);
    }
    if messages.is_empty() {
        return Err("当前 Trae 会话没有可分享的 user/assistant 消息".into());
    }
    Ok((session, messages))
}

/// 仅供受支持的本地桥接完成后临时回归测试使用；不在用户环境默认启用。
fn remote_local_chat_read_enabled() -> bool {
    std::env::var("AIWORK_ENABLE_REMOTE_LOCAL_CHAT_READ")
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

fn collect_share_resources(
    messages: &[Value],
    project_paths: &[String],
) -> Result<(Vec<LocalResource>, Vec<String>), String> {
    let mut resources = Vec::new();
    let mut seen = HashSet::new();
    let mut unavailable = Vec::new();
    for message in messages {
        let message_id = message.get("message_id").and_then(Value::as_str).unwrap_or("message");
        if let Some(query) = message.get("query").and_then(Value::as_str) {
            if let Ok(items) = serde_json::from_str::<Value>(query) {
                if let Some(items) = items.as_array() {
                    for item in items {
                        if item.get("type").and_then(Value::as_str) != Some("attachment") { continue; }
                        let Some(uri) = item.get("data").and_then(|data| data.get("uri")).and_then(Value::as_str) else { continue; };
                        let filename = item.get("data")
                            .and_then(|data| data.get("filename"))
                            .and_then(Value::as_str)
                            .unwrap_or(uri);
                        if let Some(path) = local_path_from_uri(uri) {
                            let attachment_path = unique_resource_path(
                                &resources,
                                format!("attachments/{}/{}", safe_name(message_id), safe_name(filename)),
                            );
                            add_local_resource(
                                &mut resources,
                                &mut seen,
                                path,
                                attachment_path,
                                "attachment",
                            )?;
                        } else {
                            unavailable.push(safe_name(filename));
                        }
                    }
                }
            }
        }
        if message.get("role").and_then(Value::as_str) == Some("assistant") {
            if let Some(content) = message.get("content").and_then(Value::as_str) {
                if let Ok(parsed) = serde_json::from_str::<Value>(content) {
                    let mut paths = Vec::new();
                    collect_named_strings(&parsed, "file_path", &mut paths);
                    for raw in paths {
                        let candidate = local_path_from_uri(&raw).or_else(|| {
                            project_paths.iter().map(|root| PathBuf::from(root).join(&raw)).find(|path| path.is_file())
                        });
                        if let Some(path) = candidate {
                            let artifact_path = unique_resource_path(
                                &resources,
                                format!("artifacts/{}", safe_name(&raw)),
                            );
                            add_local_resource(
                                &mut resources,
                                &mut seen,
                                path,
                                artifact_path,
                                "artifact",
                            )?;
                        } else if !raw.trim().is_empty() {
                            unavailable.push(safe_name(&raw));
                        }
                    }
                }
            }
        }
    }
    if resources.len() > 200 {
        return Err("当前会话资源超过 Trae 分享上限（200 个）".into());
    }
    Ok((resources, unavailable))
}

fn upload_share_resources(
    agent: &ureq::Agent,
    base: &str,
    session_id: &str,
    jwt: &str,
    device: &DeviceEntry,
    resources: &[LocalResource],
) -> Result<Vec<Value>, String> {
    let mut uploaded = Vec::with_capacity(resources.len());
    for resource in resources {
        let presign = json!({
            "local_session_id": session_id,
            "chat_session_id": session_id,
            "path": resource.path,
            "kind": resource.kind,
            "content_length": resource.size,
            "content_hash": resource.content_hash,
            "env": "local",
        });
        let data = data_of(post_share_json(
            agent,
            &format!("{base}/api/remote/v1/share/local_conversation/resources/presign-upload"),
            jwt,
            device,
            &presign,
            "获取分享资源上传地址",
        )?);
        let upload_url = data.get("upload_url").and_then(Value::as_str).ok_or("分享资源上传地址为空")?;
        let resource_uri = data.get("resource_uri").and_then(Value::as_str).ok_or("分享资源 URI 为空")?;
        let bytes = fs::read(&resource.source).map_err(|e| format!("读取分享资源失败 {}：{e}", resource.source.display()))?;
        let mut request = agent
            .put(upload_url)
            .set("content-type", &resource.content_type)
            .set("x-trae-client-type", "lite");
        if let Some(headers) = data.get("upload_headers") {
            let parsed = if let Some(text) = headers.as_str() {
                serde_json::from_str::<Value>(text).unwrap_or(Value::Null)
            } else { headers.clone() };
            if let Some(obj) = parsed.as_object() {
                for (key, value) in obj {
                    if let Some(value) = value.as_str() { request = request.set(key, value); }
                }
            }
        }
        request
            .send_bytes(&bytes)
            .map_err(|e| share_error(e, "上传分享资源"))?;
        uploaded.push(json!({
            "path": resource.path,
            "resource_uri": resource_uri,
            "kind": resource.kind,
            "content_hash": resource.content_hash,
        }));
    }
    Ok(uploaded)
}

fn poll_share_audit(agent: &ureq::Agent, base: &str, share_id: &str, jwt: &str, device: &DeviceEntry) -> Result<String, String> {
    let encoded = urlencoding::encode(share_id);
    let mut last = "auditing".to_string();
    for attempt in 0..15 {
        if attempt > 0 { std::thread::sleep(Duration::from_secs(1)); }
        let data = data_of(get_share_json(
            agent,
            &format!("{base}/api/remote/v1/share/{encoded}/audit_status"),
            jwt,
            device,
            "查询分享审核状态",
        )?);
        let status = data.get("status").and_then(Value::as_str).unwrap_or("auditing").to_string();
        last = status.clone();
        if status == "passed" || status == "active" { return Ok(status); }
        if status == "failed" || status == "audit_failed" {
            return Err("Trae 分享内容审核未通过".into());
        }
    }
    Ok(last)
}

/// 在切换账号前按 Trae 原生协议创建当前会话分享包，并只在本地保存分享 URL。
/// 明确的用户授权由“切换并继续”动作提供；除 Trae 分享服务外不上传到第三方。
#[tauri::command(async)]
pub fn trae_relay_create_share(
    state: State<AppState>,
    session_id: Option<String>,
    target_app: Option<String>,
) -> Result<TraeShareResult, String> {
    create_share_inner(&state, session_id, target_app)
}

fn create_share_inner(
    state: &AppState,
    session_id: Option<String>,
    target_app: Option<String>,
) -> Result<TraeShareResult, String> {
    let target = match normalize_target(target_app) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "校验目标应用", &error);
            return Err(error);
        }
    };
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 分享创建开始: target_app={} session_hint={}", target, session_id.is_some()),
    );
    let source_uid = match crate::commands::trae_apps::infer_current_cloud_uid(&target) {
        Some(value) => value,
        None => {
            let error = "未识别当前 Trae 登录账号，无法创建分享".to_string();
            log_share_failure(&state.data_dir, "识别当前账号", &error);
            return Err(error);
        }
    };
    let session_id = match session_id {
        Some(value) => match validate_session_id(&value) {
            Ok(value) => value,
            Err(error) => {
                log_share_failure(&state.data_dir, "校验会话 ID", &error);
                return Err(error);
            }
        },
        None => match detect_current_session_id(&target) {
            Some(value) => value,
            None => {
                let error = "未识别当前 Trae 会话，请先在 Trae Work 中打开要继承的对话".to_string();
                log_share_failure(&state.data_dir, "识别当前会话", &error);
                return Err(error);
            }
        },
    };
    let jwt = match account_jwt(&state, &source_uid) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "读取账号凭据", &error);
            return Err(error);
        }
    };
    let device = crate::commands::accounts::resolve_device(&state, &source_uid);
    let base = detect_share_remote_base(&state.logs_dir());
    let agent = share_agent();
    let (session, messages) = match collect_share_messages(&agent, &base, &session_id, &source_uid, &jwt, &device) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "读取会话消息", &error);
            return Err(error);
        }
    };
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 分享会话读取完成: messages={}", messages.len()),
    );
    let project_paths = {
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        let dirs = app_data_dirs(&target);
        for dir in dirs {
            let storage = dir.join(STORAGE_SUFFIX);
            if storage.is_file() { read_json_paths(&storage, &mut paths, &mut seen); }
            let state_db = dir.join(VSCDB_SUFFIX);
            if state_db.is_file() { read_vscdb_paths(&state_db, &mut paths, &mut seen); }
        }
        paths
    };
    let (resources, unavailable_files) = match collect_share_resources(&messages, &project_paths) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "收集本地资源", &error);
            return Err(error);
        }
    };
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 分享资源收集完成: resources={} unavailable={}", resources.len(), unavailable_files.len()),
    );
    let uploaded = match upload_share_resources(&agent, &base, &session_id, &jwt, &device, &resources) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "上传分享资源", &error);
            return Err(error);
        }
    };
    let title = session.get("title").and_then(Value::as_str).filter(|v| !v.is_empty()).unwrap_or("Shared conversation").to_string();
    let mode = session.get("mode").and_then(Value::as_str).unwrap_or("work");
    let creator_name = session.get("creator_name").and_then(Value::as_str).unwrap_or("AI Work 用户");
    let mut payload = json!({
        "local_session_id": session_id,
        "chat_session_id": session_id,
        "source_updated_at": timestamp_iso(session.get("updated_at")),
        "title": title,
        "mode": mode,
        "anonymous": false,
        "creator_name": creator_name,
        "messages": messages,
        "resources": uploaded,
        "env": "local",
    });
    if mode == "remote" {
        payload.as_object_mut().map(|obj| obj.remove("env"));
    }
    let created = match post_share_json(
        &agent,
        &format!("{base}/api/remote/v1/share/local_conversation"),
        &jwt,
        &device,
        &payload,
        "创建 Trae 会话分享",
    ) {
        Ok(value) => data_of(value),
        Err(error) => {
            log_share_failure(&state.data_dir, "创建会话分享", &error);
            return Err(error);
        }
    };
    let share_id = match created.get("share_session_id").and_then(Value::as_str) {
        Some(value) => value.to_string(),
        None => {
            let error = "Trae 分享响应缺少 share_session_id".to_string();
            log_share_failure(&state.data_dir, "解析分享响应", &error);
            return Err(error);
        }
    };
    let status = match poll_share_audit(&agent, &base, &share_id, &jwt, &device) {
        Ok(value) => value,
        Err(error) => {
            log_share_failure(&state.data_dir, "等待分享审核", &error);
            return Err(error);
        }
    };
    let share_url = format!("{SHARE_ORIGIN}/share/{share_id}?enter_from=pc");
    let package = TraeRelayPackage {
        schema_version: 2,
        captured_at: Utc::now().to_rfc3339(),
        source_uid: Some(source_uid),
        session_id: Some(session_id.clone()),
        target_app: target,
        project_paths,
        storage_path: None,
        state_db_path: None,
        notes: unavailable_files.iter().map(|file| format!("分享时未找到本地资源：{file}")).collect(),
        share_session_id: Some(share_id.clone()),
        share_url: Some(share_url.clone()),
    };
    if let Err(error) = save_relay_package(&state.data_dir, &package) {
        log_share_failure(&state.data_dir, "保存接力包", &error);
        return Err(error);
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("Trae 分享创建完成: resources={} status={}", uploaded.len(), status),
    );
    Ok(TraeShareResult {
        session_id,
        share_session_id: share_id,
        share_url,
        status,
        title,
        resource_count: resources.len(),
        unavailable_files,
    })
}

/// 仅允许打开 Trae 自己生成的分享 URL，避免“打开 URL”命令被用于任意外链。
#[tauri::command]
pub fn trae_relay_open_share(share_url: String) -> Result<(), String> {
    let url = share_url.trim();
    if !url.starts_with(&format!("{SHARE_ORIGIN}/share/")) || url.contains(['"', '\'', ' ']) {
        return Err("拒绝打开非 Trae 分享链接".into());
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("cmd")
            .arg("/c")
            .raw_arg(format!("start \"\" \"{url}\""))
            .creation_flags(0x08000000)
            .spawn()
            .map_err(|e| format!("打开分享链接失败：{e}"))?;
    }
    #[cfg(not(windows))]
    {
        Command::new("xdg-open").arg(url).spawn().map_err(|e| format!("打开分享链接失败：{e}"))?;
    }
    Ok(())
}

/// 切号后用原生 Trae 命令行重新打开项目。project_path 只读校验，不会创建/删除文件。
#[tauri::command(async)]
pub fn open_trae_project(
    state: State<AppState>,
    project_path: String,
    target_app: Option<String>,
    proxy_port: Option<u16>,
) -> Result<(), String> {
    let target = normalize_target(target_app)?;
    let project = PathBuf::from(project_path.trim());
    if !project.exists() {
        return Err(format!("项目路径不存在：{}", project.display()));
    }
    let exe = crate::commands::env::detect_target_exe(&state, &target)?;
    if proxy_port.is_none() {
        crate::commands::proxy::cleanup_stale_local_proxy(&state);
    } else {
        crate::commands::process::graceful_kill_app(&target)?;
    }
    let mut cmd = Command::new(&exe);
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    // Electron/VS Code fork 接受目录作为位置参数；--reuse-window 避免额外打开第二个窗口。
    cmd.args(["--reuse-window", project.to_string_lossy().as_ref()]);
    cmd.spawn().map_err(|e| format!("打开 Trae 项目失败：{e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_decode_handles_windows_path() {
        let path = decode_file_uri("file:///C:/Users/Test%20User/project").unwrap();
        assert!(path.to_ascii_lowercase().contains("c:\\users\\test user\\project"));
    }

    #[test]
    fn extracts_recent_paths_and_deduplicates() {
        let value = serde_json::json!({
            "recentlyOpenedPathsList": {"entries": [
                {"folderUri": "file:///C:/work/demo"},
                {"folderUri": "file:///C:/work/demo"}
            ]}
        });
        let mut paths = Vec::new();
        let mut seen = HashSet::new();
        extract_project_paths(&value, &mut paths, &mut seen);
        // C:\work\demo 不一定存在于测试机，解析函数仍只保留绝对路径；若不存在则为空。
        assert!(paths.len() <= 1);
    }

    /// 本机回归用：只调用 Trae renderer 的 get_chat_session/get_messages，
    /// 不创建分享、不上传资源、不消耗积分。默认 ignored，需显式运行。
    #[test]
    #[ignore]
    fn live_local_bridge_reads_current_session() {
        let state = crate::state::AppState::new().expect("app state");
        let uid = crate::commands::trae_apps::infer_current_cloud_uid("TraeWork")
            .expect("current Trae uid");
        let accounts = crate::vault::load_accounts(&state);
        let account = accounts
            .accounts
            .iter()
            .find(|entry| entry.user_id.as_deref() == Some(uid.as_str()))
            .expect("uid in account pool");
        let jwt = account.jwt.trim();
        assert!(!jwt.is_empty(), "account jwt unavailable");
        let session_id = detect_current_session_id("TraeWork").expect("current session id");
        let user_info = json!({
                "name": uid,
                "token": jwt,
                "is_internal": false,
                "user_id": uid,
                "scope": "marscode",
                "loginScope": "trae"
            });
        let direct_session = trae_lite_call(
            &session_id,
            "get_chat_session",
            json!({ "chat_session_id": session_id, "env": "local" }),
            &user_info,
        )
        .expect("get session");
        eprintln!("bridge session result code={:?} message={:?}", direct_session.get("code"), direct_session.get("message"));
        let direct_messages = trae_lite_call(
            &session_id,
            "get_messages",
            json!({ "chat_session_id": session_id, "page_size": 100, "env": "local" }),
            &user_info,
        )
        .expect("get messages");
        eprintln!("bridge messages result code={:?} message={:?} items={}", direct_messages.get("code"), direct_messages.get("message"), messages_from_data(&data_of(direct_messages.clone())).len());
        let (session, messages) = collect_share_messages_via_local_bridge(&session_id, &user_info)
        .expect("local bridge call");
        assert!(session.is_object());
        assert!(!messages.is_empty());
    }

    #[test]
    #[ignore]
    fn live_local_bridge_lists_sessions() {
        let state = crate::state::AppState::new().expect("app state");
        let uid = crate::commands::trae_apps::infer_current_cloud_uid("TraeWork")
            .expect("current Trae uid");
        let accounts = crate::vault::load_accounts(&state);
        let account = accounts
            .accounts
            .iter()
            .find(|entry| entry.user_id.as_deref() == Some(uid.as_str()))
            .expect("uid in account pool");
        let jwt = account.jwt.trim();
        let device = crate::commands::accounts::resolve_device(&state, &uid);
        let user_info = json!({
            "name": uid,
            "token": jwt,
            "is_internal": false,
            "user_id": uid,
            "scope": "marscode",
            "loginScope": "trae"
        });
        let result = trae_lite_call(
            "probe-session",
            "list_chat_sessions",
            json!({ "page_size": 100, "env": "local" }),
            &user_info,
        )
        .expect("list sessions");
        let data = data_of(result);
        eprintln!("bridge list sessions code=0 items={}", messages_from_data(&data).len());
        assert!(data.is_object() || data.is_array());
        let _ = device;
    }

    /// 本机回归用：按 Trae 原生协议创建一次会话分享，验证“读取→上传→保存”链路。
    /// 该测试会把当前会话内容上传到 Trae 官方分享服务；不调用模型、不消耗积分，
    /// 仅在用户明确要求全面测试时显式运行。
    #[test]
    #[ignore]
    fn live_create_share_package() {
        let state = crate::state::AppState::new().expect("app state");
        let result = create_share_inner(&state, None, Some("TraeWork".to_string()))
            .expect("create share package");
        assert!(!result.share_session_id.is_empty());
        assert!(result.share_url.starts_with(SHARE_ORIGIN));
        eprintln!(
            "share created status={} resources={} unavailable={}",
            result.status,
            result.resource_count,
            result.unavailable_files.len()
        );
    }
}
