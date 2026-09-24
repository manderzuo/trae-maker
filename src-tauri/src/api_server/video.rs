//! Trae Work CN Seedance 视频网关。
//!
//! 请求体沿用 Trae Work CN 原生 `tool_text_to_video_stream` 契约，
//! 通过账号池里的 Work 积分账号直连上游 SSE；任务索引与视频产物按配置持久化，
//! 不把 JWT、Cookie 或提示词写入日志。视频文件默认落在 `data/videos`，可由
//! `AIWORK_VIDEO_DIR` 指定独立磁盘目录。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::net::IpAddr;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::pool::PickedAccount;
use super::pool::ResourceKind;
use super::trae_resource_upload::{self, NativeUploadError};
use super::limits::{Permit, RateLimiter};
use super::{ApiSharedState, ErrKind, APP_ID, AGENT_HOST, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BillingStatus {
    Absent,
    Unverified,
    Verified,
}

impl Default for BillingStatus {
    fn default() -> Self {
        Self::Absent
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VideoBillingReceipt {
    pub status: BillingStatus,
    pub actual_credits: Option<String>,
    pub unit: Option<String>,
    pub source: Option<String>,
    pub task_ref: Option<String>,
    pub observed_at_ms: u64,
}

impl Default for VideoBillingReceipt {
    fn default() -> Self {
        Self {
            status: BillingStatus::Absent,
            actual_credits: None,
            unit: None,
            source: None,
            task_ref: None,
            observed_at_ms: 0,
        }
    }
}

impl VideoBillingReceipt {
    fn unverified(observed_at_ms: u64) -> Self {
        Self {
            status: BillingStatus::Unverified,
            source: Some("upstream_candidate".into()),
            observed_at_ms,
            ..Self::default()
        }
    }
}

#[derive(Clone, Serialize)]
pub struct VideoTask {
    pub id: String,
    pub object: String,
    pub model: String,
    pub status: String,
    pub prompt: String,
    pub created_at: u64,
    pub updated_at: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 传输层状态，便于客户端区分异步任务桥与其它资源类型。
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_duration: Option<f64>,
    /// 网关内可鉴权访问的相对资源地址；上游地址不可用时客户端优先使用它。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_url: Option<String>,
    /// 生成成功但本地缓存失败时给客户端的可读提示；不影响上游 video_url。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_error: Option<String>,
    /// 仅暴露经过 allowlist 校验的单任务积分回执候选，不包含原始上游响应。
    pub billing: VideoBillingReceipt,
    /// 进程重启后恢复幂等键；只保存哈希后的内部键，不保存客户端原文。
    #[serde(skip)]
    request_key: Option<String>,
    /// 本地 API Key 所有权；只用于任务查询隔离，不序列化到客户端。
    #[serde(skip)]
    pub(crate) owner_key_id: String,
}

static TASKS: OnceLock<Mutex<HashMap<String, VideoTask>>> = OnceLock::new();
static SEQ: OnceLock<Mutex<u64>> = OnceLock::new();
static IDEMPOTENCY: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
static IDEMPOTENCY_CREATE: OnceLock<Mutex<()>> = OnceLock::new();
static TASK_DATA_DIR: OnceLock<Mutex<Option<std::path::PathBuf>>> = OnceLock::new();
static JOB_PERMITS: OnceLock<Mutex<HashMap<String, Permit>>> = OnceLock::new();
const MAX_IN_MEMORY_TASKS: usize = 2048;
const TERMINAL_TASK_RETENTION_SECS: u64 = 24 * 60 * 60;
const TASKS_FILE: &str = "video_tasks.json";

fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "canceled" | "timeout")
}

fn default_owner_key() -> String {
    "anonymous".into()
}

/// 落盘 DTO：API 不序列化 owner_key_id，但重启后仍需要保留 Key 隔离信息。
#[derive(Clone, Debug, Deserialize, Serialize)]
struct PersistedVideoTask {
    id: String,
    object: String,
    model: String,
    status: String,
    prompt: String,
    created_at: u64,
    updated_at: u64,
    error: Option<String>,
    transport: String,
    video_url: Option<String>,
    resource_uri: Option<String>,
    video_duration: Option<f64>,
    #[serde(default)]
    content_url: Option<String>,
    #[serde(default)]
    artifact_error: Option<String>,
    #[serde(default)]
    billing: VideoBillingReceipt,
    #[serde(default)]
    request_key: Option<String>,
    #[serde(default = "default_owner_key")]
    owner_key_id: String,
}

fn task_data_dir() -> &'static Mutex<Option<std::path::PathBuf>> {
    TASK_DATA_DIR.get_or_init(|| Mutex::new(None))
}

/// 配置任务索引目录；桌面端和未来无头服务器都通过此入口指定运行目录。
pub fn configure(data_dir: &std::path::Path) {
    let mut guard = task_data_dir().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(data_dir.to_path_buf());
}

fn tasks() -> &'static Mutex<HashMap<String, VideoTask>> {
    TASKS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn idempotency() -> &'static Mutex<HashMap<String, String>> {
    IDEMPOTENCY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn idempotency_create_lock() -> &'static Mutex<()> {
    IDEMPOTENCY_CREATE.get_or_init(|| Mutex::new(()))
}

fn job_permits() -> &'static Mutex<HashMap<String, Permit>> {
    JOB_PERMITS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn has_job_permit(task_id: &str) -> bool {
    job_permits()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(task_id)
}

fn mark_native_worker_unknown(task_id: &str, reason: impl Into<String>) {
    let reason = reason.into();
    update_task(task_id, |task| {
        if !is_terminal_status(&task.status) {
            task.status = "unknown".into();
            task.error = Some(reason.clone());
        }
    });
}

struct NativeWorkerGuard {
    task_id: String,
}

impl NativeWorkerGuard {
    fn new(task_id: String) -> Self {
        Self { task_id }
    }
}

impl Drop for NativeWorkerGuard {
    fn drop(&mut self) {
        if get(&self.task_id)
            .map(|task| !is_terminal_status(&task.status) && task.status != "unknown")
            .unwrap_or(false)
        {
            mark_native_worker_unknown(
                &self.task_id,
                "视频 worker 异常退出，等待恢复/核查",
            );
        }
    }
}

fn restore_key_limits(data_dir: &std::path::Path, owner_key_id: &str) -> super::api_keys::KeyLimits {
    let owner_key_id = owner_key_id.trim();
    if owner_key_id.is_empty() || owner_key_id == "anonymous" {
        return super::api_keys::KeyLimits::default();
    }
    super::api_keys::constraints_for(data_dir, owner_key_id)
        .map(|resolved| resolved.limits)
        .unwrap_or_default()
}

fn restore_job_permits_for_tasks<I>(
    data_dir: &std::path::Path,
    limiter: &RateLimiter,
    tasks: I,
) -> usize
where
    I: IntoIterator<Item = (String, String, String)>,
{
    let mut restored = 0;
    for (task_id, status, owner_key_id) in tasks {
        if is_terminal_status(&status) {
            release_job_permit(&task_id);
            continue;
        }
        if has_job_permit(&task_id) {
            continue;
        }
        let owner_key_id = if owner_key_id.trim().is_empty() {
            "anonymous"
        } else {
            owner_key_id.trim()
        };
        let key_limits = restore_key_limits(data_dir, owner_key_id);
        let permit = limiter.restore_video_job(owner_key_id, &key_limits);
        if retain_job_permit(&task_id, permit) {
            restored += 1;
        }
    }
    restored
}

/// Restore the video-job permits owned by persisted legacy tasks after the
/// runtime limiter has been constructed. Terminal tasks never consume a
/// permit, and repeated startup/recovery calls keep existing permits intact.
pub fn restore_persisted_job_permits(
    data_dir: &std::path::Path,
    limiter: &RateLimiter,
) -> usize {
    let persisted_tasks = tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(|task| (task.id.clone(), task.status.clone(), task.owner_key_id.clone()))
        .collect::<Vec<_>>();
    restore_job_permits_for_tasks(data_dir, limiter, persisted_tasks)
}

/// 将视频任务许可从 HTTP 提交作用域转移到任务生命周期。
/// 已有任务不会覆盖旧许可；重复提交的许可由调用方所有权自动释放。
pub fn retain_job_permit(task_id: &str, permit: Permit) -> bool {
    let mut permits = job_permits().lock().unwrap_or_else(|e| e.into_inner());
    if permits.contains_key(task_id) {
        return false;
    }
    permits.insert(task_id.to_string(), permit);
    true
}

/// 释放任务许可；不存在许可时是幂等空操作，适合恢复/终态重复通知。
pub fn release_job_permit(task_id: &str) {
    job_permits()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(task_id);
}

fn task_store_path(data_dir: &std::path::Path) -> std::path::PathBuf {
    data_dir.join("data").join(TASKS_FILE)
}

fn persisted(task: &VideoTask) -> PersistedVideoTask {
    PersistedVideoTask {
        id: task.id.clone(),
        object: task.object.clone(),
        model: task.model.clone(),
        status: task.status.clone(),
        prompt: task.prompt.clone(),
        created_at: task.created_at,
        updated_at: task.updated_at,
        error: task.error.clone(),
        transport: task.transport.clone(),
        video_url: task.video_url.clone(),
        resource_uri: task.resource_uri.clone(),
        video_duration: task.video_duration,
        content_url: task.content_url.clone(),
        artifact_error: task.artifact_error.clone(),
        billing: task.billing.clone(),
        request_key: task.request_key.clone(),
        owner_key_id: task.owner_key_id.clone(),
    }
}

fn restored(task: PersistedVideoTask) -> VideoTask {
    VideoTask {
        id: task.id,
        object: task.object,
        model: task.model,
        status: task.status,
        prompt: task.prompt,
        created_at: task.created_at,
        updated_at: task.updated_at,
        error: task.error,
        transport: task.transport,
        video_url: task.video_url,
        resource_uri: task.resource_uri,
        video_duration: task.video_duration,
        content_url: task.content_url,
        artifact_error: task.artifact_error,
        billing: task.billing,
        request_key: task.request_key,
        owner_key_id: task.owner_key_id,
    }
}

fn persist_all(data_dir: &std::path::Path) {
    let values: Vec<PersistedVideoTask> = tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .values()
        .map(persisted)
        .collect();
    let path = task_store_path(data_dir);
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(data_dir));
    let _ = crate::fs_utils::write_json(&path, &values);
}

fn persist_current(_task_id: &str) {
    let dir = task_data_dir()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(dir) = dir {
        persist_all(&dir);
    }
}

/// 启动网关时恢复最近任务，并清理过期索引与视频文件。
pub fn load_persisted(data_dir: &std::path::Path) {
    configure(data_dir);
    let path = task_store_path(data_dir);
    let list: Vec<PersistedVideoTask> = crate::fs_utils::read_json(&path);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    // 任务索引是当前数据目录的完整快照；重启或切换数据目录时，不能让旧目录
    // 遗留的幂等键继续指向已经不可见的任务。
    idempotency()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clear();
    let old_task_ids = tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    for task_id in old_task_ids {
        release_job_permit(&task_id);
    }
    let mut map = tasks().lock().unwrap_or_else(|e| e.into_inner());
    map.clear();
    for item in list {
        if item.id.trim().is_empty()
            || (is_terminal_status(&item.status)
                && now.saturating_sub(item.updated_at) > TERMINAL_TASK_RETENTION_SECS)
        {
            continue;
        }
        let task = restored(item);
        if is_terminal_status(&task.status) {
            release_job_permit(&task.id);
        }
        if let Some(key) = task.request_key.clone() {
            if !key.is_empty() {
                idempotency()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, task.id.clone());
            }
        }
        map.insert(task.id.clone(), task);
    }
    drop(map);
    let _ = crate::api_server::video_store::cleanup(data_dir, TERMINAL_TASK_RETENTION_SECS);
    persist_all(data_dir);
}

fn next_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let mut seq = SEQ
        .get_or_init(|| Mutex::new(0))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    *seq = seq.wrapping_add(1);
    format!("video-{now}-{}", *seq)
}

fn insecure_reference_urls_allowed() -> bool {
    matches!(
        std::env::var("AIWORK_ALLOW_INSECURE_ASSET_BASE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn private_or_reserved_ip(host: &str) -> bool {
    let Ok(ip) = host.parse::<IpAddr>() else { return false };
    match ip {
        IpAddr::V4(value) => {
            let octets = value.octets();
            octets[0] == 0
                || value.is_unspecified()
                || value.is_loopback()
                || value.is_private()
                || value.is_link_local()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 0)
                || (octets[0] == 192 && octets[1] == 2)
                || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
                || (octets[0] == 198 && (18..=19).contains(&octets[1]))
                || (octets[0] == 198 && octets[1] == 51 && octets[2] == 100)
                || (octets[0] == 203 && octets[1] == 0 && octets[2] == 113)
                || octets[0] >= 224
        }
        IpAddr::V6(value) => {
            value.to_ipv4_mapped()
                .map(|mapped| private_or_reserved_ip(&mapped.to_string()))
                .unwrap_or(false)
                || value.is_unspecified()
                || value.is_loopback()
                || value.is_unique_local()
                || value.is_unicast_link_local()
                || value.is_multicast()
        }
    }
}

fn validate_reference_url(value: &str, allow_insecure_http: bool) -> Result<(), String> {
    let raw = value.trim();
    let parsed = url::Url::parse(raw).map_err(|_| "参考素材 URL 无效".to_string())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err("参考素材 URL 只允许 http(s)".into());
    }
    if parsed.scheme() == "http" && !allow_insecure_http {
        return Err("参考素材 URL 默认必须使用 HTTPS".into());
    }
    if parsed.username().is_empty() == false || parsed.password().is_some() {
        return Err("参考素材 URL 不允许携带用户名或密码".into());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "参考素材 URL 缺少主机名".to_string())?;
    let host_lower = host.to_ascii_lowercase();
    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    if host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".lan")
        || host_lower.ends_with(".internal")
        || private_or_reserved_ip(ip_host)
    {
        return Err("参考素材 URL 不允许回环、私网或保留地址".into());
    }
    Ok(())
}

fn is_native_resource_uri(value: &str) -> bool {
    let raw = value.trim();
    raw.starts_with("tos-")
        && !raw.contains("://")
        && !raw.chars().any(char::is_whitespace)
        && raw.len() <= 4096
}

pub fn validate_request(body: &Value) -> Result<(String, String), String> {
    let prompt = body
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "prompt 不能为空".to_string())?;
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("seedance")
        .to_string();
    if model.len() > 96 {
        return Err("model 过长（最多 96 个字符）".into());
    }
    if let Some(duration) = body.get("duration") {
        let valid = duration
            .as_u64()
            .map(|v| (2..=15).contains(&v))
            .unwrap_or(false);
        if !valid {
            return Err("duration 必须是 2-15 秒的整数".into());
        }
    }
    if let Some(resolution) = body.get("resolution").and_then(Value::as_str) {
        let normalized = resolution.trim().to_ascii_lowercase();
        if !matches!(normalized.as_str(), "480p" | "720p" | "1080p" | "4k") {
            return Err("resolution 仅支持 480p、720p、1080p 或 4K".into());
        }
    }
    if let Some(ratio) = body.get("ratio").and_then(Value::as_str) {
        if !matches!(ratio.trim(), "16:9" | "9:16" | "1:1" | "4:3" | "3:4" | "21:9") {
            return Err("ratio 不是支持的画面比例".into());
        }
    }
    // 网关进程不能读取远端调用方的 C:\\ / 本地路径，也不把 Base64 大字段混入
    // 任务请求；MCP 桥会先调用 /v1/assets，再改用 asset_ids。直接 HTTP 客户端
    // 给出明确错误，避免把内部字段静默转发到 Trae 原生接口。
    for key in ["image_paths", "video_paths", "image_data", "video_data"] {
        if body.get(key).is_some() {
            return Err(format!(
                "{key} 只能由本地 MCP 桥预处理；请先 POST /v1/assets，再使用 image_asset_ids/video_asset_ids"
            ));
        }
    }
    for key in ["image_urls", "video_urls", "image_asset_ids", "video_asset_ids"] {
        if let Some(values) = body.get(key) {
            let Some(items) = values.as_array() else {
                return Err(format!("{key} 必须是字符串数组"));
            };
            if items.len() > 10 || items.iter().any(|v| v.as_str().map(str::trim).map_or(true, str::is_empty)) {
                return Err(format!("{key} 最多 10 个非空字符串"));
            }
            if matches!(key, "image_urls" | "video_urls") {
                for value in items {
                    let url = value.as_str().expect("non-empty string checked above");
                    if !is_native_resource_uri(url) {
                        validate_reference_url(url, insecure_reference_urls_allowed())?;
                    }
                }
            }
        }
    }
    validate_native_reference_inputs(body)?;
    Ok((model, prompt.to_string()))
}

fn validate_native_reference_inputs(input: &Value) -> Result<(), String> {
    let obj = input
        .as_object()
        .ok_or_else(|| "video request 必须是 JSON 对象".to_string())?;
    for key in ["image_urls", "video_urls"] {
        let Some(values) = obj.get(key) else { continue };
        let Some(values) = values.as_array() else {
            return Err(format!("{key} 必须是字符串数组"));
        };
        for value in values {
            let raw = value
                .as_str()
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .ok_or_else(|| format!("{key} 只能包含非空字符串"))?;
            if !is_native_resource_uri(raw) {
                return Err(format!(
                    "{key} 不能直接传公网 URL；请先 POST /v1/assets，再使用 image_asset_ids/video_asset_ids"
                ));
            }
        }
    }
    Ok(())
}

fn build_native_request_with_uris(
    input: &Value,
    image_uris: &[String],
    video_uris: &[String],
) -> Result<Value, String> {
    let mut body = input.clone();
    let obj = body
        .as_object_mut()
        .ok_or_else(|| "video request 必须是 JSON 对象".to_string())?;

    for (asset_key, url_key, label) in [
        ("image_asset_ids", "image_urls", "图片"),
        ("video_asset_ids", "video_urls", "参考视频"),
    ] {
        let mut urls = obj.remove(url_key).unwrap_or_else(|| Value::Array(Vec::new()));
        let urls = urls
            .as_array_mut()
            .ok_or_else(|| format!("{url_key} 必须是字符串数组"))?;
        if urls.iter().any(|value| {
            value
                .as_str()
                .map(|item| !is_native_resource_uri(item))
                .unwrap_or(true)
        }) {
            return Err(format!(
                "{label}引用不能直接传公网 URL；请先 POST /v1/assets，再使用 {asset_key}"
            ));
        }
        let additions = if url_key == "image_urls" {
            image_uris
        } else {
            video_uris
        };
        urls.extend(additions.iter().cloned().map(Value::String));
        if urls.len() > 10 {
            return Err(format!("{url_key} 最多 10 个非空字符串"));
        }
        obj.insert(url_key.to_string(), Value::Array(urls.clone()));
        obj.remove(asset_key);
    }
    Ok(body)
}

pub fn prepare_native_request(
    data_dir: &std::path::Path,
    owner_key_id: &str,
    input: &Value,
    account: &PickedAccount,
) -> Result<Value, NativeUploadError> {
    validate_native_reference_inputs(input)
        .map_err(|error| NativeUploadError::new(error, false))?;
    let obj = input
        .as_object()
        .ok_or_else(|| NativeUploadError::new("video request 必须是 JSON 对象", false))?;
    let mut image_uris = Vec::new();
    let mut video_uris = Vec::new();
    for (asset_key, output, label) in [
        ("image_asset_ids", &mut image_uris, "图片"),
        ("video_asset_ids", &mut video_uris, "参考视频"),
    ] {
        let Some(ids) = obj.get(asset_key) else { continue };
        let ids = ids
            .as_array()
            .ok_or_else(|| NativeUploadError::new(format!("{asset_key} 必须是字符串数组"), false))?;
        for id in ids {
            let id = id
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| NativeUploadError::new(format!("{asset_key} 只能包含非空字符串"), false))?;
            let (record, bytes) = super::assets::read_owned(data_dir, owner_key_id, id)
                .map_err(|error| NativeUploadError::new(format!("{label}素材 {id} 无法读取：{error}"), false))?;
            let uri = trae_resource_upload::upload_asset(account, &record, &bytes)
                .map_err(|error| NativeUploadError::new(
                    format!("{label}素材 {id} 原生上传失败：{}", error.message),
                    error.retryable_account,
                ))?;
            output.push(uri);
        }
    }
    build_native_request_with_uris(input, &image_uris, &video_uris)
        .map_err(|error| NativeUploadError::new(error, false))
}

pub fn create_pending_for(model: String, prompt: String, owner_key_id: &str) -> VideoTask {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let task = VideoTask {
        id: next_id(),
        object: "video".into(),
        model,
        status: "queued".to_string(),
        prompt,
        created_at: now,
        updated_at: now,
        error: None,
        transport: "trae_work_native_sse".into(),
        video_url: None,
        resource_uri: None,
        video_duration: None,
        content_url: None,
        artifact_error: None,
        billing: VideoBillingReceipt::default(),
        request_key: None,
        owner_key_id: if owner_key_id.trim().is_empty() {
            "anonymous".into()
        } else {
            owner_key_id.trim().to_string()
        },
    };
    let mut map = tasks().lock().unwrap_or_else(|e| e.into_inner());
    let cutoff = now.saturating_sub(TERMINAL_TASK_RETENTION_SECS);
    map.retain(|_, item| {
        !is_terminal_status(&item.status) || item.updated_at >= cutoff
    });
    if map.len() >= MAX_IN_MEMORY_TASKS {
        let mut terminal: Vec<(String, u64)> = map
            .iter()
            .filter(|(_, item)| is_terminal_status(&item.status))
            .map(|(id, item)| (id.clone(), item.updated_at))
            .collect();
        terminal.sort_by_key(|(_, updated)| *updated);
        let remove = map.len().saturating_sub(MAX_IN_MEMORY_TASKS - 1);
        for (id, _) in terminal.into_iter().take(remove) {
            map.remove(&id);
        }
    }
    map.insert(task.id.clone(), task.clone());
    drop(map);
    persist_current(&task.id);
    task
}

/// 测试/内部调用的匿名任务构造器；HTTP 路由使用原子幂等 helper 绑定 Key。
pub fn create_pending(model: String, prompt: String) -> VideoTask {
    create_pending_for(model, prompt, "anonymous")
}

pub(crate) enum IdempotentCreateResult {
    Existing(VideoTask),
    Created(VideoTask),
}

/// 原子地重放或创建视频任务。
///
/// `internal_key` 必须是已按 API Key 作用域哈希后的内部键；原始客户端键
/// 不进入这里，也不会被保存。键映射和任务索引仍是两个数据结构，因此用
/// 专用临界区把“检查、清理悬空映射、创建、绑定”串成一个操作。
pub(crate) fn create_pending_with_idempotency(
    model: String,
    prompt: String,
    owner_key_id: &str,
    internal_key: Option<&str>,
) -> IdempotentCreateResult {
    let Some(key) = internal_key.map(str::trim).filter(|key| !key.is_empty()) else {
        return IdempotentCreateResult::Created(create_pending_for(model, prompt, owner_key_id));
    };

    let _critical_section = idempotency_create_lock()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mapped_task_id = idempotency()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .get(key)
        .cloned();
    if let Some(task_id) = mapped_task_id {
        if let Some(task) = get(&task_id) {
            return IdempotentCreateResult::Existing(task);
        }
        idempotency()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(key);
    }

    let task = create_pending_for(model, prompt, owner_key_id);
    bind_idempotency(key, &task.id);
    IdempotentCreateResult::Created(task)
}

pub fn visible_to(task: &VideoTask, owner_key_id: &str) -> bool {
    let owner = if owner_key_id.trim().is_empty() {
        "anonymous"
    } else {
        owner_key_id.trim()
    };
    task.owner_key_id == owner
}

/// 返回幂等键对应的仍然可查询任务；键只保存任务 ID，不落盘正文或凭证。
pub fn find_idempotent(key: &str) -> Option<VideoTask> {
    let key = key.trim();
    if key.is_empty() {
        return None;
    }
    let task_id = idempotency()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(key)
        .cloned()?;
    get(&task_id)
}

/// 将客户端幂等键限制在 API Key 命名空间内，避免两个调用方使用同名键
/// 时互相看到对方的任务或提示词。`scope` 仅使用已鉴权的 Key ID（或
/// `anonymous`），不写入日志。
pub fn scoped_idempotency_key(scope: &str, key: &str) -> String {
    let scope = scope.trim();
    let key = key.trim();
    let raw = format!("{}:{}", if scope.is_empty() { "anonymous" } else { scope }, key);
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    digest.update(raw.as_bytes());
    format!("idem-{:x}", digest.finalize())
}

/// Seedance HTTP 错误有时把额度/会话错误包在 400 响应里，不能只按状态码
/// 归类。优先读取业务 code/message，再回退到通用 HTTP 分类。
fn classify_video_error(status: u16, body: &str) -> ErrKind {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        let code = value
            .get("code")
            .or_else(|| value.get("error_code"))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let message = value
            .get("message")
            .or_else(|| value.get("error").and_then(|v| v.get("message")))
            .and_then(Value::as_str)
            .unwrap_or(body);
        let kind = super::classify_solo_error(code, message);
        if kind != ErrKind::Server || code != 0 {
            return kind;
        }
    }
    let lower = body.to_ascii_lowercase();
    if lower.contains("insufficient credit")
        || lower.contains("credits exhausted")
        || lower.contains("积分不足")
        || lower.contains("余额不足")
    {
        return ErrKind::HardCredit;
    }
    super::classify_error(status, body)
}

fn retryable_account_error(kind: ErrKind) -> bool {
    matches!(kind, ErrKind::HardCredit | ErrKind::PlanLimit | ErrKind::SessionDead)
}

fn video_error_message(data: &str) -> String {
    serde_json::from_str::<Value>(data)
        .ok()
        .and_then(|value| {
            value
                .get("message")
                .or_else(|| value.get("error").and_then(|v| v.get("message")))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| data.chars().take(240).collect())
}

/// 建立幂等键映射并限制内存增长。已存在的键由路由层先返回，不会覆盖原任务。
pub fn remember_idempotent(key: &str, task_id: &str) {
    let key = key.trim();
    if key.is_empty() {
        return;
    }
    let mut map = idempotency().lock().unwrap_or_else(|e| e.into_inner());
    if map.len() >= 1024 {
        let remove = map.len() - 1023;
        let keys: Vec<String> = map.keys().take(remove).cloned().collect();
        for old in keys {
            map.remove(&old);
        }
    }
    map.insert(key.to_string(), task_id.to_string());
}

/// 将已隔离/哈希后的幂等键绑定到任务并持久化，网关重启后仍能正确重放。
pub fn bind_idempotency(key: &str, task_id: &str) {
    let key = key.trim();
    if key.is_empty() {
        return;
    }
    remember_idempotent(key, task_id);
    if let Some(task) = tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(task_id)
    {
        task.request_key = Some(key.to_string());
    }
    persist_current(task_id);
}

fn update_task(task_id: &str, update: impl FnOnce(&mut VideoTask)) {
    let mut terminal = false;
    let changed = if let Some(task) = tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_mut(task_id)
    {
        update(task);
        terminal = is_terminal_status(&task.status);
        task.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default();
        true
    } else {
        false
    };
    if changed {
        persist_current(task_id);
        if terminal {
            release_job_permit(task_id);
        }
    }
}

fn now_id(prefix: &str) -> String {
    format!("{}-{}", prefix, crate::commands::oauth::random_hex(24))
}

fn build_request_body(input: &Value) -> Value {
    let mut body = input.clone();
    let obj = body.as_object_mut().expect("video request is object");
    obj.entry("image_urls").or_insert_with(|| Value::Array(Vec::new()));
    obj.entry("video_urls").or_insert_with(|| Value::Array(Vec::new()));
    obj.entry("resolution").or_insert_with(|| Value::String("480p".into()));
    obj.entry("ratio").or_insert_with(|| Value::String("16:9".into()));
    obj.entry("duration").or_insert_with(|| Value::from(4));
    obj.entry("mode_type").or_insert_with(|| Value::from(1));
    obj.entry("request_type").or_insert_with(|| Value::String("solo_work_lite".into()));
    obj.entry("chat_mode").or_insert_with(|| Value::from(0));
    obj.entry("session_id").or_insert_with(|| Value::String(now_id("video-session")));
    obj.entry("access_type").or_insert_with(|| Value::from(1));
    obj.entry("watermark").or_insert_with(|| Value::Bool(false));
    // 资产 ID 只在网关内部使用，不能污染 Trae 原生请求体。
    obj.remove("image_asset_ids");
    obj.remove("video_asset_ids");
    body
}

fn result_fields(value: &Value) -> (Option<String>, Option<String>, Option<f64>) {
    let mut values = vec![value];
    for key in ["data", "result", "video"] {
        if let Some(nested) = value.get(key) {
            values.push(nested);
        }
    }
    let uri = values.iter().find_map(|item| {
        ["uri", "resource_uri", "video_uri"]
            .into_iter()
            .find_map(|key| item.get(key).and_then(Value::as_str).map(str::to_string))
    });
    let url = values.iter().find_map(|item| {
        item.get("url")
            .and_then(Value::as_str)
            .filter(|s| s.starts_with("https://"))
            .map(str::to_string)
    });
    let duration = values.iter().find_map(|item| {
        item.get("video_duration")
            .or_else(|| item.get("duration"))
            .and_then(Value::as_f64)
    });
    (uri, url, duration)
}

fn billing_observed_at_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn extract_billing_candidate(data: &str, expected_task_id: &str) -> Result<VideoBillingReceipt, String> {
    let value: Value = serde_json::from_str(data)
        .map_err(|error| format!("billing receipt JSON 无效: {error}"))?;
    Ok(extract_billing_candidate_value(
        &value,
        expected_task_id,
        credit_value_contains_exponent(data),
    ))
}

fn credit_value_contains_exponent(data: &str) -> bool {
    let lower = data.to_ascii_lowercase();
    let mut offset = 0;
    while let Some(relative) = lower[offset..].find("\"credits\"") {
        let key_end = offset + relative + "\"credits\"".len();
        let Some(colon) = lower[key_end..].find(':') else {
            return false;
        };
        let value_start = key_end + colon + 1;
        let rest = lower[value_start..].trim_start();
        let end = rest
            .find(|character| character == ',' || character == '}')
            .unwrap_or(rest.len());
        let token = rest[..end].trim().trim_matches('"');
        if token.contains('e') {
            return true;
        }
        offset = value_start + end;
    }
    false
}

fn extract_billing_candidate_value(
    value: &Value,
    expected_task_id: &str,
    credit_value_has_exponent: bool,
) -> VideoBillingReceipt {
    let observed_at_ms = billing_observed_at_ms();
    let mut objects = vec![value];
    for key in ["data", "result", "output", "video"] {
        if let Some(nested) = value.get(key).filter(|candidate| candidate.is_object()) {
            objects.push(nested);
        }
    }

    let mut saw_billing_signal = false;
    for object in objects {
        if [
            "task_id",
            "task_ref",
            "usage",
            "credits",
            "unit",
            "balance_before",
            "balance_after",
            "total_tokens",
            "cost",
        ]
        .iter()
        .any(|key| object.get(*key).is_some())
        {
            saw_billing_signal = true;
        }

        let Some(task_ref) = ["task_id", "task_ref"].iter().find_map(|key| {
            object.get(*key).and_then(Value::as_str)
        }) else {
            continue;
        };
        let Some(usage) = object.get("usage").and_then(Value::as_object) else {
            continue;
        };
        if task_ref != expected_task_id
            || usage.get("unit").and_then(Value::as_str) != Some("credits")
        {
            continue;
        }
        if credit_value_has_exponent {
            return VideoBillingReceipt::unverified(observed_at_ms);
        }
        let Some(_actual_credits) = usage
            .get("credits")
            .and_then(canonical_credit_amount)
        else {
            continue;
        };
        // A shaped `usage.credits` field is only a candidate until an
        // authoritative per-task billing contract has been verified. Do not
        // publish or persist it as a charge amount.
        let mut candidate = VideoBillingReceipt::unverified(observed_at_ms);
        candidate.unit = Some("credits".into());
        candidate.task_ref = Some(expected_task_id.to_string());
        return candidate;
    }

    if saw_billing_signal {
        VideoBillingReceipt::unverified(observed_at_ms)
    } else {
        VideoBillingReceipt::default()
    }
}

fn canonical_credit_amount(value: &Value) -> Option<String> {
    let raw = match value {
        Value::Number(number) => number.to_string(),
        Value::String(string) => string.trim().to_string(),
        _ => return None,
    };
    if raw.is_empty()
        || raw.len() > 64
        || raw.starts_with('-')
        || raw.contains(['e', 'E'])
    {
        return None;
    }
    let (whole, fraction) = raw.split_once('.').unwrap_or((&raw, ""));
    if whole.is_empty()
        || whole.len() > 24
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.is_empty()
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

fn merge_billing_candidate(task: &mut VideoTask, candidate: VideoBillingReceipt) {
    if candidate.status == BillingStatus::Verified
        || (task.billing.status == BillingStatus::Absent
            && candidate.status == BillingStatus::Unverified)
    {
        task.billing = candidate;
    }
}

fn apply_result_payload(task_id: &str, value: &Value, billing: VideoBillingReceipt) {
    let (uri, url, duration) = result_fields(value);
    if uri.is_some() || url.is_some() || duration.is_some() || billing.status != BillingStatus::Absent {
        update_task(task_id, |task| {
            if uri.is_some() {
                task.resource_uri = uri;
            }
            if url.is_some() {
                task.video_url = url;
            }
            if duration.is_some() {
                task.video_duration = duration;
            }
            merge_billing_candidate(task, billing);
        });
    }
}

fn parse_sse_event(event: &str, data: &str, task_id: &str) -> Result<bool, String> {
    if event == "result" || event == "output" {
        let value: Value = serde_json::from_str(data)
            .map_err(|e| format!("Seedance {event} JSON 无效: {e}"))?;
        let billing = extract_billing_candidate(data, task_id)?;
        apply_result_payload(task_id, &value, billing);
    } else if event == "done" {
        // “done” 没有 result 资源时不能向调用方报告成功，否则会得到一个
        // 永远无法下载的空任务。允许 done 携带最后一条结果数据作为兜底。
        if !data.trim().is_empty() {
            if let Ok(value) = serde_json::from_str::<Value>(data) {
                let billing = extract_billing_candidate(data, task_id)
                    .map_err(|error| error.to_string())?;
                apply_result_payload(task_id, &value, billing);
            }
        }
        let has_resource = get(task_id)
            .map(|task| task.resource_uri.is_some() || task.video_url.is_some())
            .unwrap_or(false);
        update_task(task_id, |task| {
            if has_resource {
                task.status = "completed".into();
            } else {
                task.status = "failed".into();
                task.error = Some("Seedance 已结束但未返回视频资源".into());
            }
        });
        return Ok(true);
    } else if event == "error" {
        let message = video_error_message(data);
        update_task(task_id, |task| {
            task.status = "failed".into();
            task.error = Some(message);
        });
        return Ok(true);
    }
    Ok(false)
}

fn resolve_resource_url(account: &PickedAccount, uri: &str) -> Result<String, String> {
    let url = format!("{}{}", AGENT_HOST, "/api/ide/v1/get_resource_url");
    let body = serde_json::json!({ "uri_list": [uri] });
    let trace = now_id("trace");
    let trace_short = trace.chars().take(32).collect::<String>();
    let response = super::streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("user-agent", "TraeClient/TTNet")
        .set("x-ide-token", &account.jwt)
        .set("x-app-id", APP_ID)
        .set("x-device-type", "windows")
        .set("x-device-brand", "H610E-B")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", &account.device_id)
        .set("x-machine-id", &account.machine_id)
        .set("x-os-version", "Windows 10 Pro")
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-custom-trace-id", &trace_short)
        .send_json(body)
        .map_err(|error| format!("获取视频资源地址失败: {error}"))?;
    let value: Value = response
        .into_json()
        .map_err(|error| format!("解析视频资源地址失败: {error}"))?;
    value
        .get("url_map")
        .and_then(|map| map.get(uri))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|url| url.starts_with("https://"))
        .ok_or_else(|| "上游未返回可用视频地址".into())
}

/// 启动后台 Seedance SSE 任务；不阻塞网关 tokio worker。
pub fn start_native_task(
    state: std::sync::Arc<ApiSharedState>,
    task_id: String,
    input: Value,
    owner_key_id: String,
    core_attribution: Option<super::bridge_billing::CoreRequestAttribution>,
    account: PickedAccount,
    permit: Permit,
) {
    if !retain_job_permit(&task_id, permit) {
        update_task(&task_id, |task| {
            task.status = "failed".into();
            task.error = Some("视频任务许可重复绑定".into());
        });
        return;
    }
    thread::spawn(move || {
        let _worker_guard = NativeWorkerGuard::new(task_id.clone());
        // HTTP 层尚未收到 SSE 前允许切到下一个 Work 账号；一旦进入 SSE，
        // 账号切换时必须用新账号重新上传参考素材，不能复用上一个账号的 TOS URI。
        let mut account = account;
        let mut tried = HashSet::from([account.uid.clone()]);
        let response = loop {
            let body = match prepare_native_request(
                &state.data_dir,
                &owner_key_id,
                &input,
                &account,
            ) {
                Ok(value) => build_request_body(&value),
                Err(error) => {
                    if error.retryable_account {
                        state.pool.note_error(&account.uid, ErrKind::SessionDead);
                        if let Some(next) = state.pool.pick_excluding_for(&tried, ResourceKind::Work) {
                            tried.insert(next.uid.clone());
                            account = next;
                            continue;
                        }
                    }
                    update_task(&task_id, |task| {
                        task.status = "failed".into();
                        task.error = Some(error.message);
                    });
                    return;
                }
            };
            let mut body = body;
            if let Some(attribution) = core_attribution.as_ref() {
                let session_id = super::bridge_billing::core_usage_session_id(
                    &attribution.request_id,
                    &account.uid,
                );
                if super::payload::bind_upstream_session_id_value(&mut body, &session_id).is_err() {
                    update_task(&task_id, |task| {
                        task.status = "failed".into();
                        task.error = Some("Core 请求无法建立独立的上游用量会话，已阻止未记录的上游调用".into());
                    });
                    return;
                }
            }
            if super::bridge_billing::BridgeBillingStore::record_core_upstream_attempt_from_payload(
                &state.data_dir,
                core_attribution.as_ref(),
                &account.uid,
                &body,
            ).is_err() {
                update_task(&task_id, |task| {
                    task.status = "failed".into();
                    task.error = Some("Core 请求与上游会话归因暂不可用，已阻止未记录的上游调用".into());
                });
                return;
            }
            let trace = now_id("trace");
            let url = format!("{}{}", AGENT_HOST, "/api/ide/v1/tool_text_to_video_stream");
            let referer = format!("{}{}", REFERER_BASE, "/api/ide/v1/tool_text_to_video_stream");
            let request_id = now_id("req");
            let trace_short = trace.chars().take(32).collect::<String>();
            let response = super::streaming_agent()
                .post(&url)
                .set("content-type", "application/json")
                .set("accept", "text/event-stream")
                .set("accept-encoding", "gzip, deflate")
                .set("user-agent", "TraeClient/TTNet")
                .set("x-ide-token", &account.jwt)
                .set("x-app-id", APP_ID)
                .set("x-app-version", "default")
                .set("x-app-version-code", IDE_VERSION_CODE)
                .set("x-ide-version", IDE_VERSION)
                .set("x-ide-version-code", IDE_VERSION_CODE)
                .set("x-ide-version-type", "stable")
                .set("x-device-type", "windows")
                .set("x-device-brand", "H610E-B")
                .set("x-device-cpu", "Intel")
                .set("x-device-id", &account.device_id)
                .set("x-machine-id", &account.machine_id)
                .set("x-os-version", "Windows 10 Pro")
                .set("request-traffic-type", "prod")
                .set("package-type", "stable_cn")
                .set("x-lgw-req-sdk-type", "3")
                .set("x-lscbd-aid", "787976")
                .set("x-lscbd-platform", "windows")
                .set("x-ss-dp", "787976")
                .set("app-version", IDE_VERSION)
                .set("x-custom-trace-id", &trace_short)
                .set("x-request-id", &request_id)
                .set("referer", &referer)
                .send_json(body.clone());
            match response {
                Ok(response) if (200..300).contains(&response.status()) => break response,
                Ok(response) => {
                    let status = response.status();
                    let text = response.into_string().unwrap_or_default();
                    let kind = classify_video_error(status, &text);
                    state.pool.note_error(&account.uid, kind);
                    if retryable_account_error(kind) {
                        if let Some(next) = state.pool.pick_excluding_for(&tried, ResourceKind::Work) {
                            tried.insert(next.uid.clone());
                            account = next;
                            continue;
                        }
                    }
                    update_task(&task_id, |task| {
                        task.status = "failed".into();
                        task.error = Some(format!(
                            "Seedance 上游 HTTP {status}: {}",
                            text.chars().take(240).collect::<String>()
                        ));
                    });
                    return;
                }
                Err(ureq::Error::Status(status, response)) => {
                    let text = response.into_string().unwrap_or_default();
                    let kind = classify_video_error(status, &text);
                    state.pool.note_error(&account.uid, kind);
                    if retryable_account_error(kind) {
                        if let Some(next) = state.pool.pick_excluding_for(&tried, ResourceKind::Work) {
                            tried.insert(next.uid.clone());
                            account = next;
                            continue;
                        }
                    }
                    update_task(&task_id, |task| {
                        task.status = "failed".into();
                        task.error = Some(format!(
                            "Seedance 上游 HTTP {status}: {}",
                            text.chars().take(240).collect::<String>()
                        ));
                    });
                    return;
                }
                Err(error) => {
                    state.pool.note_error(&account.uid, ErrKind::Server);
                    mark_native_worker_unknown(
                        &task_id,
                        format!("Seedance 上游连接结果未知: {error}"),
                    );
                    return;
                }
            }
        };
        update_task(&task_id, |task| task.status = "processing".into());
        let mut event_name = String::new();
        let mut event_data = String::new();
        let reader = BufReader::new(response.into_reader());
        let mut terminal = false;
        for line in reader.lines().flatten() {
            if line.trim().is_empty() {
                if !event_data.is_empty() {
                    if event_name == "error" {
                        // SSE 错误通常已经进入原生任务阶段，不能安全重放；仍要把
                        // 额度/会话错误写入账号状态，避免下一次继续命中同一账号。
                        let kind = classify_video_error(0, &event_data);
                        state.pool.note_error(&account.uid, kind);
                    }
                    match parse_sse_event(&event_name, &event_data, &task_id) {
                        Ok(true) => terminal = true,
                        Ok(false) => {}
                        Err(error) => {
                            mark_native_worker_unknown(&task_id, error);
                            terminal = true;
                        }
                    }
                }
                event_name.clear();
                event_data.clear();
                if terminal { break; }
            } else if let Some(value) = line.strip_prefix("event:") {
                event_name = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !event_data.is_empty() { event_data.push('\n'); }
                event_data.push_str(value.trim_start());
            }
        }
        if !terminal && !event_data.is_empty() {
            if event_name == "error" {
                let kind = classify_video_error(0, &event_data);
                state.pool.note_error(&account.uid, kind);
            }
            match parse_sse_event(&event_name, &event_data, &task_id) {
                Ok(_) => {}
                Err(error) => mark_native_worker_unknown(&task_id, error),
            }
        }
        let task_status = get(&task_id).map(|task| task.status).unwrap_or_default();
        if task_status == "completed" {
            if let Some(uri) = get(&task_id).and_then(|task| task.resource_uri) {
                if let Ok(url) = resolve_resource_url(&account, &uri) {
                    update_task(&task_id, |task| task.video_url = Some(url));
                }
            }
            // 生成完成后将上游地址缓存为本地受鉴权资源。失败时保留上游 URL，
            // 让客户端仍可立即取回视频，同时在任务中给出缓存失败原因。
            if let Some(url) = get(&task_id).and_then(|task| task.video_url) {
                match super::video_store::download_from_url(&state.data_dir, &task_id, &url) {
                    Ok((_path, _size)) => update_task(&task_id, |task| {
                        task.content_url = super::video_store::content_url(&task.id).ok();
                        task.artifact_error = None;
                    }),
                    Err(error) => update_task(&task_id, |task| {
                        task.artifact_error = Some(error.chars().take(240).collect());
                    }),
                }
            }
            state.pool.note_success(&account.uid);
        } else if task_status == "processing" {
            mark_native_worker_unknown(
                &task_id,
                "Seedance SSE 在 done 事件前结束，等待上游核查",
            );
            state.pool.note_error(&account.uid, super::ErrKind::Server);
        }
    });
}

pub fn get(task_id: &str) -> Option<VideoTask> {
    tasks()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(task_id)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_urls_allow_public_https_only_by_default() {
        assert!(validate_reference_url("https://cdn.example.com/frame.png", false).is_ok());
        assert!(validate_reference_url("https://localhost/frame.png", false).is_err());
        assert!(validate_reference_url("https://127.0.0.1/frame.png", false).is_err());
        assert!(validate_reference_url("https://192.168.1.5/frame.png", false).is_err());
        assert!(validate_reference_url("https://10.0.0.5/frame.png", false).is_err());
        assert!(validate_reference_url("https://192.0.2.1/frame.png", false).is_err());
        assert!(validate_reference_url("https://[::ffff:127.0.0.1]/frame.png", false).is_err());
        assert!(validate_reference_url("https://[fc00::1]/frame.png", false).is_err());
        assert!(validate_reference_url("https://asset.lan/frame.png", false).is_err());
        assert!(validate_reference_url("file:///C:/secret.png", false).is_err());
        assert!(validate_reference_url("data:image/png;base64,AAAA", false).is_err());
        assert!(validate_reference_url("ftp://cdn.example.com/frame.png", false).is_err());
        assert!(validate_reference_url("http://cdn.example.com/frame.png", false).is_err());
        assert!(validate_reference_url("http://cdn.example.com/frame.png", true).is_ok());
    }

    #[test]
    fn validates_prompt_and_defaults_model() {
        let (model, prompt) = validate_request(&serde_json::json!({"prompt":"  cat  "})).unwrap();
        assert_eq!(model, "seedance");
        assert_eq!(prompt, "cat");
        assert!(validate_request(&serde_json::json!({"prompt":"  "})).is_err());
    }

    #[test]
    fn billing_candidate_requires_task_reference_and_credit_unit() {
        let candidate = extract_billing_candidate(
            r#"{"task_id":"video-1","usage":{"credits":12.5,"unit":"credits"}}"#,
            "video-1",
        )
        .unwrap();
        assert_eq!(candidate.status, BillingStatus::Unverified);
        assert_eq!(candidate.actual_credits, None);
        assert_eq!(candidate.unit.as_deref(), Some("credits"));
        assert_eq!(candidate.task_ref.as_deref(), Some("video-1"));
    }

    #[test]
    fn unverified_usage_shape_is_not_a_verified_billing_receipt() {
        let candidate = extract_billing_candidate(
            r#"{"task_id":"video-1","usage":{"credits":12.5,"unit":"credits"}}"#,
            "video-1",
        )
        .unwrap();

        assert_eq!(candidate.status, BillingStatus::Unverified);
        assert_eq!(candidate.actual_credits, None);
    }

    #[test]
    fn balance_delta_duration_token_cost_and_unrelated_numbers_are_not_billing() {
        for data in [
            r#"{"video_duration":5,"credits":12.5}"#,
            r#"{"usage":{"total_tokens":999,"cost":"12.50","currency":"CNY"}}"#,
            r#"{"balance_before":100,"balance_after":80}"#,
        ] {
            assert_eq!(
                extract_billing_candidate(data, "video-1")
                    .unwrap()
                    .status,
                BillingStatus::Unverified
            );
        }
    }

    #[test]
    fn invalid_or_negative_receipts_are_exposed_as_unverified_without_failing_the_video_task() {
        let candidate = extract_billing_candidate(
            r#"{"task_id":"video-1","usage":{"credits":-1,"unit":"credits"}}"#,
            "video-1",
        )
        .unwrap();
        assert_eq!(candidate.status, BillingStatus::Unverified);
        assert!(candidate.actual_credits.is_none());
    }

    #[test]
    fn exponent_nan_and_over_precision_values_stay_unverified() {
        for data in [
            r#"{"task_id":"video-1","usage":{"credits":1e2,"unit":"credits"}}"#,
            r#"{"task_id":"video-1","usage":{"credits":"NaN","unit":"credits"}}"#,
            r#"{"task_id":"video-1","usage":{"credits":1.1234567,"unit":"credits"}}"#,
        ] {
            let candidate = extract_billing_candidate(data, "video-1").unwrap();
            assert_eq!(candidate.status, BillingStatus::Unverified);
            assert!(candidate.actual_credits.is_none());
        }
    }

    #[test]
    fn pending_task_is_queryable() {
        let task = create_pending("seedance".into(), "test".into());
        assert_eq!(task.status, "queued");
        assert_eq!(get(&task.id).unwrap().id, task.id);
    }

    #[test]
    fn validates_video_shape_and_rejects_invalid_values() {
        let ok = serde_json::json!({"prompt":"cat", "duration": 8, "resolution":"720p", "ratio":"9:16"});
        assert!(validate_request(&ok).is_ok());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "duration": 1})).is_err());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "resolution":"2k"})).is_err());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "ratio":"2:1"})).is_err());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "image_urls":"not-array"})).is_err());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "image_asset_ids":["asset-1"]})).is_ok());
        assert!(validate_request(&serde_json::json!({"prompt":"cat", "image_paths":["C:\\cat.png"]})).is_err());
    }

    #[test]
    fn native_body_does_not_leak_internal_asset_ids() {
        let body = build_request_body(&serde_json::json!({
            "prompt": "cat",
            "image_asset_ids": ["asset-1"],
            "video_asset_ids": ["asset-2"],
        }));
        assert!(body.get("image_asset_ids").is_none());
        assert!(body.get("video_asset_ids").is_none());
    }

    #[test]
    fn native_request_uses_trae_resource_uris_for_uploaded_assets() {
        let input = serde_json::json!({
            "prompt": "cat",
            "image_asset_ids": ["asset-1"],
            "video_asset_ids": ["asset-2"]
        });
        let body = build_native_request_with_uris(
            &input,
            &["tos-cn-i-test/image/frame.png".to_string()],
            &["tos-cn-i-test/video/reference.mp4".to_string()],
        )
        .unwrap();
        assert_eq!(
            body.get("image_urls").unwrap(),
            &serde_json::json!(["tos-cn-i-test/image/frame.png"])
        );
        assert_eq!(
            body.get("video_urls").unwrap(),
            &serde_json::json!(["tos-cn-i-test/video/reference.mp4"])
        );
        assert!(body.get("image_asset_ids").is_none());
        assert!(body.get("video_asset_ids").is_none());
    }

    #[test]
    fn native_request_rejects_public_urls_that_would_fail_upstream_signing() {
        let input = serde_json::json!({
            "prompt": "cat",
            "image_urls": ["https://www.gemstory.cn/v1/assets/a/content?token=x"]
        });
        let error = build_native_request_with_uris(&input, &[], &[]).unwrap_err();
        assert!(error.contains("/v1/assets"));
    }

    #[test]
    fn validate_request_allows_native_resource_uri_but_rejects_public_url() {
        assert!(validate_request(&serde_json::json!({
            "prompt": "cat",
            "image_urls": ["tos-cn-i-test/image/frame.png"]
        }))
        .is_ok());
        let error = validate_request(&serde_json::json!({
            "prompt": "cat",
            "image_urls": ["https://example.com/frame.png"]
        }))
        .unwrap_err();
        assert!(error.contains("image_asset_ids"));
    }

    #[test]
    fn idempotency_returns_same_task() {
        let task = create_pending("seedance".into(), "same".into());
        let key = scoped_idempotency_key("key-a", "test-idempotency");
        remember_idempotent(&key, &task.id);
        assert_eq!(find_idempotent(&key).unwrap().id, task.id);
        assert!(find_idempotent(&scoped_idempotency_key("key-b", "test-idempotency")).is_none());
    }

    #[test]
    fn concurrent_idempotent_creates_return_one_task_id() {
        let key = scoped_idempotency_key(
            "key-concurrent",
            &format!("test-atomic-{}", rand::random::<u64>()),
        );
        let workers = 8;
        let start = std::sync::Arc::new(std::sync::Barrier::new(workers));
        let handles = (0..workers)
            .map(|_| {
                let key = key.clone();
                let start = start.clone();
                thread::spawn(move || {
                    start.wait();
                    match create_pending_with_idempotency(
                        "seedance".into(),
                        "concurrent idempotency".into(),
                        "key-concurrent",
                        Some(&key),
                    ) {
                        IdempotentCreateResult::Existing(task)
                        | IdempotentCreateResult::Created(task) => task.id,
                    }
                })
            })
            .collect::<Vec<_>>();
        let task_ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<HashSet<_>>();

        assert_eq!(task_ids.len(), 1);
    }

    #[test]
    fn result_then_done_requires_a_resource() {
        let task = create_pending("seedance".into(), "result".into());
        assert!(!parse_sse_event("result", r#"{"uri":"tos://video-1","video_duration":4}"#, &task.id).unwrap());
        assert!(parse_sse_event("done", "{}", &task.id).unwrap());
        let completed = get(&task.id).unwrap();
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.resource_uri.as_deref(), Some("tos://video-1"));
        assert_eq!(completed.video_duration, Some(4.0));
    }

    #[test]
    fn result_event_updates_billing_without_exposing_raw_payload() {
        let task = create_pending("seedance".into(), "billing".into());
        let data = serde_json::json!({
            "task_id": task.id,
            "uri": "tos://video-billing",
            "usage": {"credits": 1.25, "unit": "credits", "account": "private"}
        })
        .to_string();
        assert!(!parse_sse_event("result", &data, &task.id).unwrap());
        let updated = get(&task.id).unwrap();
        assert_eq!(updated.billing.status, BillingStatus::Unverified);
        assert_eq!(updated.billing.actual_credits, None);
        assert_eq!(updated.billing.task_ref.as_deref(), Some(task.id.as_str()));
        assert!(serde_json::to_string(&updated)
            .unwrap()
            .contains("upstream_candidate"));
        assert!(!serde_json::to_string(&updated).unwrap().contains("private"));
    }

    #[test]
    fn done_without_resource_is_not_success() {
        let task = create_pending("seedance".into(), "empty".into());
        assert!(parse_sse_event("done", "{}", &task.id).unwrap());
        let failed = get(&task.id).unwrap();
        assert_eq!(failed.status, "failed");
        assert!(failed.error.as_deref().unwrap_or("").contains("未返回视频资源"));
    }

    #[test]
    fn video_credit_http_errors_are_account_specific() {
        assert_eq!(
            classify_video_error(400, r#"{"code":0,"message":"积分不足，请切换账号"}"#),
            ErrKind::HardCredit
        );
        assert_eq!(
            classify_video_error(401, "unauthorized"),
            ErrKind::SessionDead
        );
        assert!(retryable_account_error(ErrKind::HardCredit));
        assert!(!retryable_account_error(ErrKind::Server));
    }

    #[test]
    fn task_visibility_is_scoped_to_owner_key() {
        let task = create_pending_for("seedance".into(), "private".into(), "key-a");
        assert!(visible_to(&task, "key-a"));
        assert!(!visible_to(&task, "key-b"));
        let anonymous = create_pending("seedance".into(), "anonymous".into());
        assert!(visible_to(&anonymous, ""));
        assert!(!visible_to(&anonymous, "key-a"));
    }

    #[test]
    fn persisted_task_roundtrip_keeps_owner_and_artifact_fields() {
        let billing = extract_billing_candidate(
            r#"{"task_id":"video-test","usage":{"credits":12.5,"unit":"credits"}}"#,
            "video-test",
        )
        .unwrap();
        let task = VideoTask {
            id: "video-test".into(),
            object: "video".into(),
            model: "seedance".into(),
            status: "completed".into(),
            prompt: "a quiet lake".into(),
            created_at: 1,
            updated_at: 2,
            error: None,
            transport: "trae_work_native_sse".into(),
            video_url: Some("https://example.invalid/video.mp4".into()),
            resource_uri: Some("tos://video".into()),
            video_duration: Some(4.0),
            content_url: Some("/v1/videos/video-test/content".into()),
            artifact_error: None,
            billing: billing.clone(),
            request_key: Some("idem-test".into()),
            owner_key_id: "key-a".into(),
        };
        let restored = restored(persisted(&task));
        assert_eq!(restored.owner_key_id, "key-a");
        assert_eq!(restored.content_url.as_deref(), Some("/v1/videos/video-test/content"));
        assert_eq!(restored.video_url, task.video_url);
        assert_eq!(restored.billing, billing);
        let public = serde_json::to_value(&task).unwrap();
        assert_eq!(public["billing"]["status"], "unverified");
        assert!(public["billing"]["actual_credits"].is_null());
    }

    #[test]
    fn video_job_permit_survives_http_submission_until_terminal_update() {
        let limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 4,
                max_video_jobs: 1,
                asset_uploads_per_minute: 4,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 4,
            },
        );
        let permit = limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .unwrap();
        let task = create_pending_for("seedance".into(), "permit lifetime".into(), "video-key");
        let task_id = task.id.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (terminal_tx, terminal_rx) = std::sync::mpsc::channel();

        let worker = thread::spawn(move || {
            let _permit = permit;
            entered_tx.send(()).unwrap();
            terminal_rx.recv().unwrap();
            update_task(&task_id, |task| task.status = "completed".into());
        });
        entered_rx.recv().unwrap();
        assert!(limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .is_err());
        terminal_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(get(&task.id).unwrap().status, "completed");
        assert!(limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .is_ok());
    }

    #[test]
    fn retained_video_job_permit_is_released_when_task_reaches_terminal_state() {
        let limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 4,
                max_video_jobs: 1,
                asset_uploads_per_minute: 4,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 4,
            },
        );
        let task_id = format!("video-permit-{}-{}", std::process::id(), rand::random::<u64>());
        let permit = limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .unwrap();
        assert!(retain_job_permit(&task_id, permit));
        assert!(limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .is_err());

        update_task(&task_id, |task| task.status = "completed".into());
        release_job_permit(&task_id);
        assert!(limiter
            .acquire_video_job("video-key", &super::super::api_keys::KeyLimits::default())
            .is_ok());
    }

    fn restore_test_dir(label: &str) -> std::path::PathBuf {
        let dir = std::path::PathBuf::from(format!(
            r"D:\gpt\aiwork-video-restore-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        dir
    }

    fn write_restore_key(data_dir: &std::path::Path, key_id: &str, max_video_jobs: usize) {
        super::super::api_keys::save(
            data_dir,
            &super::super::api_keys::ApiKeysFile {
                keys: vec![super::super::api_keys::ApiKeyEntry {
                    id: key_id.to_string(),
                    name: "restore test".into(),
                    key: format!("restore-test-{}", rand::random::<u64>()),
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
                    limits: super::super::api_keys::KeyLimits {
                        max_video_jobs: Some(max_video_jobs),
                        ..super::super::api_keys::KeyLimits::default()
                    },
                    capabilities: vec![super::super::api_keys::CAPABILITY_VIDEO.into()],
                }],
                auth_disabled: false,
            },
        );
    }

    fn restore_record(id: &str, status: &str, owner_key_id: &str) -> (String, String, String) {
        (id.to_string(), status.to_string(), owner_key_id.to_string())
    }

    fn cleanup_restore_permits(task_ids: &[&str]) {
        for task_id in task_ids {
            release_job_permit(task_id);
        }
    }

    #[test]
    fn restores_nonterminal_video_jobs_with_current_or_default_limits() {
        let data_dir = restore_test_dir("nonterminal");
        let known_key = "restore-known-key";
        write_restore_key(&data_dir, known_key, 1);
        let known_task = format!("restore-known-{}", rand::random::<u64>());
        let anonymous_task = format!("restore-anonymous-{}", rand::random::<u64>());
        let unknown_task = format!("restore-unknown-{}", rand::random::<u64>());
        let limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 8,
                max_video_jobs: 5,
                asset_uploads_per_minute: 8,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 8,
            },
        );

        assert_eq!(
            restore_job_permits_for_tasks(
                &data_dir,
                &limiter,
                vec![
                    restore_record(&known_task, "queued", known_key),
                    restore_record(&anonymous_task, "running", "anonymous"),
                    restore_record(&unknown_task, "created", "missing-key"),
                ],
            ),
            3
        );

        let known_limits = super::super::api_keys::KeyLimits {
            max_video_jobs: Some(1),
            ..super::super::api_keys::KeyLimits::default()
        };
        assert!(limiter.acquire_video_job(known_key, &known_limits).is_err());
        let anonymous_extra = limiter
            .acquire_video_job("anonymous", &super::super::api_keys::KeyLimits::default())
            .expect("anonymous restore should use default limits");
        let unknown_extra = limiter
            .acquire_video_job("missing-key", &super::super::api_keys::KeyLimits::default())
            .expect("unknown owner restore should use default limits");
        drop(anonymous_extra);
        drop(unknown_extra);

        cleanup_restore_permits(&[&known_task, &anonymous_task, &unknown_task]);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn terminal_video_jobs_do_not_restore_a_job_permit() {
        let data_dir = restore_test_dir("terminal");
        let known_key = "restore-terminal-key";
        write_restore_key(&data_dir, known_key, 1);
        let task_id = format!("restore-terminal-{}", rand::random::<u64>());
        let limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 8,
                max_video_jobs: 1,
                asset_uploads_per_minute: 8,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 8,
            },
        );

        assert_eq!(
            restore_job_permits_for_tasks(
                &data_dir,
                &limiter,
                vec![restore_record(&task_id, "completed", known_key)],
            ),
            0
        );
        let permit = limiter
            .acquire_video_job(known_key, &super::super::api_keys::KeyLimits {
                max_video_jobs: Some(1),
                ..super::super::api_keys::KeyLimits::default()
            })
            .expect("terminal task must not consume a video permit");
        drop(permit);

        cleanup_restore_permits(&[&task_id]);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn repeated_video_job_restore_does_not_duplicate_in_process_permits() {
        let data_dir = restore_test_dir("repeat");
        let task_id = format!("restore-repeat-{}", rand::random::<u64>());
        let limiter = super::super::limits::RateLimiter::with_config(
            super::super::limits::LimitConfig {
                max_inflight: 8,
                max_video_jobs: 4,
                asset_uploads_per_minute: 8,
                asset_bytes_per_hour: 1024,
                video_submissions_per_minute: 8,
            },
        );
        let records = vec![restore_record(&task_id, "queued", "anonymous")];

        assert_eq!(restore_job_permits_for_tasks(&data_dir, &limiter, records.clone()), 1);
        assert_eq!(restore_job_permits_for_tasks(&data_dir, &limiter, records), 0);

        let extra_a = limiter
            .acquire_video_job("extra-a", &super::super::api_keys::KeyLimits::default())
            .unwrap();
        let extra_b = limiter
            .acquire_video_job("extra-b", &super::super::api_keys::KeyLimits::default())
            .unwrap();
        let extra_c = limiter
            .acquire_video_job("extra-c", &super::super::api_keys::KeyLimits::default())
            .expect("recovery must not consume a second permit for the same task");
        drop(extra_a);
        drop(extra_b);
        drop(extra_c);

        cleanup_restore_permits(&[&task_id]);
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
