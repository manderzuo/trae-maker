use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use aiwork_core::{AssetState, CoreAsset, CoreStore, CreateAssetInput, Principal};
use base64::{engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD}, Engine as _};
use chrono::Utc;
use rand::{rngs::OsRng, RngCore};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

pub const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_TTL_MS: i64 = 30 * 60 * 1000;

#[derive(Debug, Deserialize)]
pub struct AssetUploadRequest {
    pub filename: String,
    #[serde(default)]
    pub mime_type: Option<String>,
    pub data_base64: String,
}

#[derive(Debug)]
pub struct ParsedAssetUpload {
    pub filename: String,
    pub declared_mime: Option<String>,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct StoredAsset {
    pub record: CoreAsset,
    pub public_token: String,
    pub storage_path: PathBuf,
}

#[derive(Debug)]
pub struct StoredAssetBytes {
    pub record: CoreAsset,
    pub bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct PublicAssetBytes {
    pub record: CoreAsset,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum AssetError {
    #[error("invalid asset: {0}")]
    Invalid(String),
    #[error("asset not found")]
    NotFound,
    #[error("asset storage error: {0}")]
    Storage(String),
    #[error("asset upload rate limit exceeded")]
    RateLimited { retry_after_seconds: u64 },
}

impl AssetError {
    pub fn public_code(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid_asset",
            Self::NotFound => "asset_not_found",
            Self::Storage(_) => "asset_not_found",
            Self::RateLimited { .. } => "rate_limited",
        }
    }
}

#[derive(Debug)]
struct AssetWindow {
    minute_started_ms: i64,
    minute_count: u64,
    hour_started_ms: i64,
    hour_bytes: usize,
    inflight: usize,
}

#[derive(Debug)]
pub struct AssetLimiter {
    windows: Mutex<HashMap<String, AssetWindow>>,
    max_inflight: usize,
    max_per_minute: u64,
    max_bytes_per_hour: usize,
}

pub struct AssetPermit {
    limiter: Arc<AssetLimiter>,
    key_id: String,
}

impl AssetLimiter {
    pub fn new(max_inflight: usize, max_per_minute: u64, max_bytes_per_hour: usize) -> Self {
        Self {
            windows: Mutex::new(HashMap::new()),
            max_inflight: max_inflight.max(1),
            max_per_minute: max_per_minute.max(1),
            max_bytes_per_hour: max_bytes_per_hour.max(1),
        }
    }

    pub fn from_env() -> Self {
        Self::new(
            env_usize("STARLINK_ASSET_MAX_INFLIGHT_PER_KEY", 4),
            env_u64("STARLINK_ASSET_UPLOADS_PER_MINUTE", 30),
            env_usize("STARLINK_ASSET_BYTES_PER_HOUR", 256 * 1024 * 1024),
        )
    }

    pub fn acquire(self: &Arc<Self>, key_id: &str, bytes: usize) -> Result<AssetPermit, AssetError> {
        let now = Utc::now().timestamp_millis();
        let mut windows = self.windows.lock().expect("asset limiter mutex poisoned");
        let window = windows.entry(key_id.to_owned()).or_insert(AssetWindow {
            minute_started_ms: now,
            minute_count: 0,
            hour_started_ms: now,
            hour_bytes: 0,
            inflight: 0,
        });
        if now.saturating_sub(window.minute_started_ms) >= 60_000 || now < window.minute_started_ms {
            window.minute_started_ms = now;
            window.minute_count = 0;
        }
        if now.saturating_sub(window.hour_started_ms) >= 60 * 60 * 1000 || now < window.hour_started_ms {
            window.hour_started_ms = now;
            window.hour_bytes = 0;
        }
        if window.inflight >= self.max_inflight {
            return Err(AssetError::RateLimited { retry_after_seconds: 1 });
        }
        if window.minute_count >= self.max_per_minute {
            return Err(AssetError::RateLimited { retry_after_seconds: remaining_seconds(window.minute_started_ms, now, 60_000) });
        }
        if bytes > self.max_bytes_per_hour.saturating_sub(window.hour_bytes) {
            return Err(AssetError::RateLimited { retry_after_seconds: remaining_seconds(window.hour_started_ms, now, 60 * 60 * 1000) });
        }
        window.inflight += 1;
        window.minute_count += 1;
        window.hour_bytes += bytes;
        Ok(AssetPermit { limiter: Arc::clone(self), key_id: key_id.to_owned() })
    }
}

impl Drop for AssetPermit {
    fn drop(&mut self) {
        if let Ok(mut windows) = self.limiter.windows.lock() {
            if let Some(window) = windows.get_mut(&self.key_id) {
                window.inflight = window.inflight.saturating_sub(1);
            }
        }
    }
}

pub fn parse_upload(body: &[u8]) -> Result<ParsedAssetUpload, AssetError> {
    let input: AssetUploadRequest = serde_json::from_slice(body)
        .map_err(|error| AssetError::Invalid(format!("请求体必须是有效 JSON: {error}")))?;
    let filename = validate_filename(&input.filename)?;
    let (data_mime, encoded) = parse_data_url(input.data_base64.trim())?;
    let declared_mime = input
        .mime_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    if let (Some(from_url), Some(from_body)) = (data_mime.as_deref(), declared_mime.as_deref()) {
        if from_url != from_body {
            return Err(AssetError::Invalid("data URL 与 mime_type 不一致".into()));
        }
    }
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|error| AssetError::Invalid(format!("data_base64 无效: {error}")))?;
    if bytes.is_empty() {
        return Err(AssetError::Invalid("素材文件不能为空".into()));
    }
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(AssetError::Invalid(format!(
            "素材超过 {} MiB 限制",
            MAX_ASSET_BYTES / (1024 * 1024)
        )));
    }
    let (detected_mime, _) = detect_format(&bytes)
        .ok_or_else(|| AssetError::Invalid("不支持的素材格式，仅支持 PNG/JPEG/GIF/WebP/MP4/WebM".into()))?;
    let declared = declared_mime.or(data_mime);
    if let Some(declared) = declared.as_deref() {
        if declared != detected_mime {
            return Err(AssetError::Invalid(format!(
                "素材类型与文件内容不匹配（声明 {declared}，实际 {detected_mime}）"
            )));
        }
    }
    Ok(ParsedAssetUpload {
        filename,
        declared_mime: Some(detected_mime.to_string()),
        bytes,
    })
}

pub fn write_asset(
    data_dir: &Path,
    principal: &Principal,
    parsed: ParsedAssetUpload,
) -> Result<StoredAsset, AssetError> {
    if principal.user_id.trim().is_empty() {
        return Err(AssetError::Invalid("素材必须绑定 Core 用户".into()));
    }
    let (mime_type, extension) = detect_format(&parsed.bytes)
        .ok_or_else(|| AssetError::Invalid("不支持的素材格式".into()))?;
    let id = random_id("asset");
    let token = random_token();
    let created_at_ms = Utc::now().timestamp_millis();
    let expires_at_ms = created_at_ms.saturating_add(DEFAULT_TTL_MS);
    let storage_ref = format!("assets/{id}.{extension}");
    let storage_path = storage_path(data_dir, &storage_ref)?;
    write_atomically(&storage_path, &parsed.bytes)?;
    let sha256 = format!("{:x}", Sha256::digest(&parsed.bytes));
    let record = CoreAsset {
        id,
        user_id: principal.user_id.clone(),
        filename: parsed.filename,
        mime_type: mime_type.to_string(),
        extension: extension.to_string(),
        size: parsed.bytes.len() as i64,
        sha256,
        storage_ref,
        content_token_digest: Sha256::digest(token.as_bytes()).to_vec(),
        created_at_ms,
        expires_at_ms,
        state: AssetState::Active,
    };
    Ok(StoredAsset { record, public_token: token, storage_path })
}

pub fn persist_asset(store: &CoreStore, principal: &Principal, stored: &StoredAsset) -> Result<CoreAsset, AssetError> {
    let input = CreateAssetInput {
        id: stored.record.id.clone(),
        filename: stored.record.filename.clone(),
        mime_type: stored.record.mime_type.clone(),
        extension: stored.record.extension.clone(),
        size: stored.record.size,
        sha256: stored.record.sha256.clone(),
        storage_ref: stored.record.storage_ref.clone(),
        content_token_digest: stored.record.content_token_digest.clone(),
        created_at_ms: stored.record.created_at_ms,
        expires_at_ms: stored.record.expires_at_ms,
    };
    match store.create_asset(principal, input) {
        Ok(record) => Ok(record),
        Err(error) => {
            let _ = fs::remove_file(&stored.storage_path);
            Err(AssetError::Storage(error.to_string()))
        }
    }
}

pub fn read_owned(
    store: &CoreStore,
    data_dir: &Path,
    principal: &Principal,
    asset_id: &str,
) -> Result<StoredAssetBytes, AssetError> {
    let record = store
        .asset_for_user(principal, asset_id)
        .map_err(|_| AssetError::NotFound)?
        .ok_or(AssetError::NotFound)?;
    let bytes = read_record_bytes(data_dir, &record)?;
    Ok(StoredAssetBytes { record, bytes })
}

pub fn read_public(
    store: &CoreStore,
    data_dir: &Path,
    asset_id: &str,
    token: &str,
) -> Result<PublicAssetBytes, AssetError> {
    if token.trim().is_empty() || token.len() > 256 {
        return Err(AssetError::NotFound);
    }
    let digest = Sha256::digest(token.as_bytes());
    let record = store
        .asset_by_content_token(asset_id, digest.as_ref(), Utc::now().timestamp_millis())
        .map_err(|_| AssetError::NotFound)?
        .ok_or(AssetError::NotFound)?;
    let bytes = read_record_bytes(data_dir, &record)?;
    Ok(PublicAssetBytes { record, bytes })
}

pub fn content_url(config: &crate::config::RouterConfig, asset_id: &str, token: &str) -> String {
    let base = std::env::var("STARLINK_ROUTER_PUBLIC_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| format!("http://{}:{}", config.host, config.port));
    format!(
        "{}/v1/assets/{}/content?token={}",
        base.trim_end_matches('/'),
        asset_id,
        token
    )
}

fn parse_data_url(value: &str) -> Result<(Option<String>, &str), AssetError> {
    if let Some((header, data)) = value.strip_prefix("data:").and_then(|value| value.split_once(',')) {
        let mut parts = header.split(';');
        let mime = parts.next().filter(|part| !part.trim().is_empty()).map(|part| part.trim().to_ascii_lowercase());
        if !parts.any(|part| part.eq_ignore_ascii_case("base64")) {
            return Err(AssetError::Invalid("data URL 必须使用 base64 编码".into()));
        }
        return Ok((mime, data.trim()));
    }
    Ok((None, value))
}

fn validate_filename(value: &str) -> Result<String, AssetError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 128 || value == "." || value == ".." {
        return Err(AssetError::Invalid("filename 无效".into()));
    }
    if value.contains('/') || value.contains('\\') || value.contains("..") || value.chars().any(char::is_control) {
        return Err(AssetError::Invalid("filename 不能包含路径字符".into()));
    }
    Ok(value.to_string())
}

fn detect_format(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { return Some(("image/png", "png")); }
    if bytes.starts_with(b"\xff\xd8\xff") { return Some(("image/jpeg", "jpg")); }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { return Some(("image/gif", "gif")); }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" { return Some(("image/webp", "webp")); }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" { return Some(("video/mp4", "mp4")); }
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) { return Some(("video/webm", "webm")); }
    None
}

fn asset_dir(data_dir: &Path) -> PathBuf { data_dir.join("data").join("assets") }

fn storage_path(data_dir: &Path, storage_ref: &str) -> Result<PathBuf, AssetError> {
    let name = storage_ref.strip_prefix("assets/").unwrap_or("");
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") || name.chars().any(char::is_whitespace) {
        return Err(AssetError::Invalid("素材存储引用无效".into()));
    }
    Ok(asset_dir(data_dir).join(name))
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), AssetError> {
    let dir = path.parent().ok_or_else(|| AssetError::Storage("素材目录无效".into()))?;
    fs::create_dir_all(dir).map_err(|error| AssetError::Storage(error.to_string()))?;
    let partial = path.with_extension(format!("{}.partial", path.extension().and_then(|value| value.to_str()).unwrap_or("asset")));
    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&partial)
            .map_err(|error| AssetError::Storage(error.to_string()))?;
        file.write_all(bytes).map_err(|error| AssetError::Storage(error.to_string()))?;
        file.sync_all().map_err(|error| AssetError::Storage(error.to_string()))?;
        fs::rename(&partial, path).map_err(|error| AssetError::Storage(error.to_string()))
    })();
    if result.is_err() { let _ = fs::remove_file(&partial); }
    result
}

fn read_record_bytes(data_dir: &Path, record: &CoreAsset) -> Result<Vec<u8>, AssetError> {
    let path = storage_path(data_dir, &record.storage_ref).map_err(|_| AssetError::NotFound)?;
    let root = asset_dir(data_dir).canonicalize().map_err(|_| AssetError::NotFound)?;
    let canonical = fs::canonicalize(&path).map_err(|_| AssetError::NotFound)?;
    if !canonical.starts_with(&root) { return Err(AssetError::NotFound); }
    let bytes = fs::read(canonical).map_err(|_| AssetError::NotFound)?;
    if bytes.len() as i64 != record.size || format!("{:x}", Sha256::digest(&bytes)) != record.sha256 {
        return Err(AssetError::NotFound);
    }
    Ok(bytes)
}

fn random_token() -> String {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn random_id(kind: &str) -> String {
    let mut bytes = [0_u8; 16];
    OsRng.fill_bytes(&mut bytes);
    format!("{kind}_{}", URL_SAFE_NO_PAD.encode(bytes))
}

pub fn response(error: AssetError) -> axum::response::Response {
    use axum::{http::StatusCode, response::IntoResponse, Json};
    let retry_after = match &error { AssetError::RateLimited { retry_after_seconds } => Some(*retry_after_seconds), _ => None };
    let status = match error { AssetError::Invalid(_) => StatusCode::BAD_REQUEST, AssetError::NotFound => StatusCode::NOT_FOUND, AssetError::Storage(_) => StatusCode::BAD_GATEWAY, AssetError::RateLimited { .. } => StatusCode::TOO_MANY_REQUESTS };
    let mut response = (status, Json(serde_json::json!({"error": {"type": "asset_error", "code": error.public_code(), "message": error.to_string()}}))).into_response();
    if let Some(seconds) = retry_after { response.headers_mut().insert("retry-after", seconds.to_string().parse().unwrap()); }
    response
}

#[cfg(test)]
mod tests {
    use super::{AssetError, AssetLimiter};

    #[test]
    fn limiter_releases_inflight_and_enforces_windows() {
        let limiter = std::sync::Arc::new(AssetLimiter::new(1, 2, 10));
        let first = limiter.acquire("key-1", 8).unwrap();
        assert!(matches!(limiter.acquire("key-1", 1), Err(AssetError::RateLimited { .. })));
        drop(first);
        let second = limiter.acquire("key-1", 2).unwrap();
        assert!(matches!(limiter.acquire("key-1", 1), Err(AssetError::RateLimited { .. })));
        drop(second);
        let third = limiter.acquire("key-1", 1);
        assert!(matches!(third, Err(AssetError::RateLimited { .. })));
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name).ok().and_then(|value| value.trim().parse().ok()).unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|value| value.trim().parse().ok()).unwrap_or(default)
}

fn remaining_seconds(started_ms: i64, now_ms: i64, window_ms: i64) -> u64 {
    let remaining = window_ms.saturating_sub(now_ms.saturating_sub(started_ms));
    ((remaining + 999) / 1000).max(1) as u64
}
