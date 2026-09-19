//! 参考图/参考视频资源暂存层。
//!
//! 资源只由 API Key 所有者可见，文件落在可配置的本地目录，不写入日志。
//! 本模块先负责安全接收和生命周期管理；Trae 原生上传适配器在确认上游
//! 协议后读取 `read_owned`，不会把本地路径直接转发给上游。

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// 单个输入素材最大 32 MiB；视频参考素材后续如需更大值，改为独立策略。
pub const MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
/// 默认临时资源保留 30 分钟，完成任务后可由清理任务提前删除。
pub const DEFAULT_TTL_SECS: u64 = 30 * 60;
const MIN_TTL_SECS: u64 = 5 * 60;
const MAX_TTL_SECS: u64 = 2 * 60 * 60;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssetRecord {
    pub id: String,
    pub owner_key_id: String,
    pub filename: String,
    pub mime_type: String,
    pub extension: String,
    pub size: u64,
    pub sha256: String,
    pub created_at: u64,
    pub expires_at: u64,
    /// 不返回给 API 客户端；仅用于生成短时公网内容链接。
    #[serde(default)]
    pub(crate) public_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct AssetIndex {
    version: u32,
    assets: Vec<AssetRecord>,
}

fn index_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn parse_ttl_secs(raw: Option<&str>) -> u64 {
    raw.and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
        .clamp(MIN_TTL_SECS, MAX_TTL_SECS)
}

fn asset_ttl_secs() -> u64 {
    parse_ttl_secs(std::env::var("AIWORK_ASSET_TTL_SECS").ok().as_deref())
}

fn allow_insecure_asset_base() -> bool {
    matches!(
        std::env::var("AIWORK_ALLOW_INSECURE_ASSET_BASE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1" | "true" | "TRUE" | "yes" | "YES")
    )
}

fn private_or_reserved_host(host: &str) -> bool {
    let ip_host = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_host.parse::<IpAddr>() {
        return match ip {
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
                value.to_ipv4_mapped().is_some_and(|mapped| {
                    private_or_reserved_host(&mapped.to_string())
                }) || value.is_unspecified()
                    || value.is_loopback()
                    || value.is_unique_local()
                    || value.is_unicast_link_local()
                    || value.is_multicast()
            }
        };
    }
    let host_lower = ip_host.to_ascii_lowercase();
    host_lower == "localhost"
        || host_lower.ends_with(".localhost")
        || host_lower.ends_with(".local")
        || host_lower.ends_with(".lan")
        || host_lower.ends_with(".internal")
}

fn validate_public_base_url(raw: &str, allow_insecure: bool) -> Result<String, String> {
    let base = raw.trim().trim_end_matches('/');
    if base.is_empty() || base.chars().any(char::is_whitespace) {
        return Err("AIWORK_ASSET_PUBLIC_BASE_URL 必须是无查询参数的 http(s) 地址".into());
    }
    let parsed = url::Url::parse(base)
        .map_err(|_| "AIWORK_ASSET_PUBLIC_BASE_URL 必须是 http(s) 地址".to_string())?;
    let scheme_lower = parsed.scheme().to_ascii_lowercase();
    let host = parsed
        .host_str()
        .ok_or_else(|| "AIWORK_ASSET_PUBLIC_BASE_URL 缺少主机名".to_string())?;
    if !matches!(scheme_lower.as_str(), "http" | "https")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err("AIWORK_ASSET_PUBLIC_BASE_URL 必须是 http(s) 地址".into());
    }
    if scheme_lower == "http" && !allow_insecure {
        return Err("公网素材基址必须使用 HTTPS；仅测试时设置 AIWORK_ALLOW_INSECURE_ASSET_BASE=true".into());
    }
    if private_or_reserved_host(host) && !(scheme_lower == "http" && allow_insecure) {
        return Err("素材公开基址不能指向回环、私网或保留主机".into());
    }
    Ok(base.to_string())
}

/// 运行时资源目录：部署环境可将素材放到独立磁盘；默认跟随应用数据目录。
pub fn storage_dir(data_dir: &Path) -> PathBuf {
    std::env::var_os("AIWORK_ASSET_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| data_dir.join("data").join("assets"))
}

fn index_path(data_dir: &Path) -> PathBuf {
    data_dir.join("data").join("assets.json")
}

fn safe_asset_id(id: &str) -> Result<&str, String> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 160
        || !id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'))
    {
        return Err("素材 ID 无效".into());
    }
    Ok(id)
}

fn file_path(data_dir: &Path, record: &AssetRecord) -> Result<PathBuf, String> {
    let id = safe_asset_id(&record.id)?;
    let ext = record.extension.trim_start_matches('.');
    if ext.is_empty()
        || ext.len() > 8
        || !ext.bytes().all(|c| c.is_ascii_alphanumeric())
    {
        return Err("素材扩展名无效".into());
    }
    Ok(storage_dir(data_dir).join(format!("{id}.{ext}")))
}

fn public_base_url(data_dir: &Path) -> Result<String, String> {
    // 环境变量优先，便于容器/云服务器注入；桌面端也可从网关设置页保存，
    // 这样三个部署场景都使用同一套解析逻辑。
    let raw = std::env::var("AIWORK_ASSET_PUBLIC_BASE_URL")
        .ok()
        .or_else(|| {
            let configured = crate::api_server::gateway_settings::load(data_dir)
                .asset_public_base_url;
            (!configured.trim().is_empty()).then_some(configured)
        })
        .ok_or_else(|| "未配置素材公开基址，不能把素材交给上游读取（设置 AIWORK_ASSET_PUBLIC_BASE_URL 或网关设置页）".to_string())?;
    validate_public_base_url(&raw, allow_insecure_asset_base())
}

fn detect_format(bytes: &[u8]) -> Option<(&'static str, &'static str)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(("image/png", "png"));
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(("image/jpeg", "jpg"));
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(("image/gif", "gif"));
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(("image/webp", "webp"));
    }
    // MP4/MOV 的 ftyp box；只用于参考视频暂存，不宣称所有容器都支持。
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        return Some(("video/mp4", "mp4"));
    }
    // WebM/Matroska EBML 头。
    if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        return Some(("video/webm", "webm"));
    }
    None
}

fn safe_filename(filename: &str, extension: &str) -> String {
    let base = Path::new(filename)
        .file_name()
        .and_then(|v| v.to_str())
        .unwrap_or("")
        .trim();
    let mut out: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ' ') {
                c
            } else {
                '_'
            }
        })
        .take(96)
        .collect();
    if out.is_empty() || out == "." || out == ".." {
        out = format!("upload.{extension}");
    }
    out
}

fn load_index(data_dir: &Path) -> AssetIndex {
    let mut index: AssetIndex = crate::fs_utils::read_json(&index_path(data_dir));
    if index.version == 0 {
        index.version = 1;
    }
    index
}

fn save_index(data_dir: &Path, index: &AssetIndex) -> Result<(), String> {
    let path = index_path(data_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建素材索引目录失败: {e}"))?;
    }
    crate::fs_utils::write_json(&path, index)
}

fn purge_expired_locked(data_dir: &Path, index: &mut AssetIndex, now: u64) {
    let mut kept = Vec::with_capacity(index.assets.len());
    for record in index.assets.drain(..) {
        if record.expires_at > now {
            kept.push(record);
        } else if let Ok(path) = file_path(data_dir, &record) {
            let _ = fs::remove_file(path);
        }
    }
    index.assets = kept;
}

fn write_record_file(data_dir: &Path, record: &AssetRecord, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = file_path(data_dir, record)?;
    if path.exists() {
        return Err("素材文件已存在，拒绝覆盖".into());
    }
    let dir = storage_dir(data_dir);
    fs::create_dir_all(&dir).map_err(|e| format!("创建素材目录失败: {e}"))?;
    let partial = path.with_extension(format!("{}.part", record.extension));
    {
        let mut file = File::create(&partial).map_err(|e| format!("创建素材临时文件失败: {e}"))?;
        file.write_all(bytes).map_err(|e| format!("写入素材失败: {e}"))?;
        file.sync_all().map_err(|e| format!("刷新素材失败: {e}"))?;
    }
    if let Err(error) = fs::rename(&partial, &path) {
        let _ = fs::remove_file(&partial);
        return Err(format!("提交素材失败: {error}"));
    }
    Ok(path)
}

fn validate_upload_bytes(
    filename: &str,
    declared_mime: Option<&str>,
    bytes: &[u8],
) -> Result<(&'static str, &'static str), String> {
    if bytes.is_empty() {
        return Err("素材文件为空".into());
    }
    if bytes.len() > MAX_ASSET_BYTES {
        return Err(format!("素材超过 {} MiB 限制", MAX_ASSET_BYTES / (1024 * 1024)));
    }
    let Some((detected_mime, extension)) = detect_format(bytes) else {
        return Err("不支持的素材格式，仅支持 PNG/JPEG/WebP/GIF/MP4/WebM".into());
    };
    if let Some(mime) = declared_mime.map(str::trim).filter(|value| !value.is_empty()) {
        if mime.to_ascii_lowercase() != detected_mime {
            return Err(format!("素材类型与文件内容不匹配（声明 {mime}，实际 {detected_mime}）"));
        }
    }
    if filename.trim().is_empty() || filename.len() > 128 {
        return Err("filename 无效".into());
    }
    Ok((detected_mime, extension))
}

/// Core enforce 专用的文件写入：只落盘和返回元数据，不更新 legacy JSON 索引。
pub fn write_core_asset(
    data_dir: &Path,
    user_id: &str,
    filename: &str,
    declared_mime: Option<&str>,
    bytes: &[u8],
) -> Result<(AssetRecord, String), String> {
    if user_id.trim().is_empty() {
        return Err("素材上传必须绑定 Core 用户".into());
    }
    let (detected_mime, extension) = validate_upload_bytes(filename, declared_mime, bytes)?;
    let _guard = index_lock().lock().unwrap_or_else(|error| error.into_inner());
    let now = now_secs();
    let record = AssetRecord {
        id: format!("asset-{now}-{}", crate::commands::oauth::random_hex(12)),
        owner_key_id: user_id.trim().to_string(),
        filename: safe_filename(filename, extension),
        mime_type: detected_mime.to_string(),
        extension: extension.to_string(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        created_at: now,
        expires_at: now.saturating_add(asset_ttl_secs()),
        public_token: crate::commands::oauth::random_hex(32),
    };
    let path = write_record_file(data_dir, &record, bytes)?;
    let storage_ref = format!("assets/{}.{}", record.id, record.extension);
    if path.file_name().and_then(|name| name.to_str()) != Some(storage_ref.trim_start_matches("assets/")) {
        let _ = fs::remove_file(path);
        return Err("生成素材存储引用失败".into());
    }
    Ok((record, storage_ref))
}

pub fn content_token_digest(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

pub fn public_content_url(data_dir: &Path, asset_id: &str, token: &str) -> Result<String, String> {
    let base = public_base_url(data_dir)?;
    Ok(format!("{base}/assets/{asset_id}/content?token={token}"))
}

/// Core 资产读取不读取 JSON 索引；storage_ref、大小和摘要均在 SQLite 中受约束。
pub fn read_core(
    data_dir: &Path,
    storage_ref: &str,
    expected_size: i64,
    expected_sha256: &str,
) -> Result<Vec<u8>, String> {
    let name = storage_ref.strip_prefix("assets/").unwrap_or("");
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.chars().any(char::is_whitespace)
    {
        return Err("素材存储引用无效".into());
    }
    if expected_size <= 0 || expected_sha256.len() != 64 {
        return Err("素材元数据无效".into());
    }
    let root = storage_dir(data_dir)
        .canonicalize()
        .map_err(|e| format!("素材目录不可用: {e}"))?;
    let path = root.join(name);
    let canonical = fs::canonicalize(&path).map_err(|e| format!("素材文件不存在: {e}"))?;
    if !canonical.starts_with(&root) {
        return Err("素材路径越界".into());
    }
    let bytes = fs::read(&canonical).map_err(|e| format!("读取素材失败: {e}"))?;
    if bytes.len() as i64 != expected_size
        || format!("{:x}", Sha256::digest(&bytes)) != expected_sha256
    {
        return Err("素材文件摘要或大小不匹配".into());
    }
    Ok(bytes)
}

pub fn remove_core_asset(data_dir: &Path, storage_ref: &str) -> Result<(), String> {
    let name = storage_ref.strip_prefix("assets/").unwrap_or("");
    if name.is_empty()
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.chars().any(char::is_whitespace)
    {
        return Err("素材存储引用无效".into());
    }
    let root = storage_dir(data_dir);
    let path = root.join(name);
    if path.exists() {
        let canonical_root = root
            .canonicalize()
            .map_err(|e| format!("素材目录不可用: {e}"))?;
        let canonical_path = path
            .canonicalize()
            .map_err(|e| format!("素材文件不可用: {e}"))?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err("素材路径越界".into());
        }
        fs::remove_file(canonical_path).map_err(|e| format!("删除素材失败: {e}"))?;
    }
    Ok(())
}

/// 保存上传素材并返回不含文件路径的元数据。
pub fn create(
    data_dir: &Path,
    owner_key_id: &str,
    filename: &str,
    declared_mime: Option<&str>,
    bytes: &[u8],
) -> Result<AssetRecord, String> {
    let (detected_mime, extension) = validate_upload_bytes(filename, declared_mime, bytes)?;

    let owner = owner_key_id.trim();
    if owner.is_empty() {
        return Err("素材上传必须绑定 API Key".into());
    }

    let _guard = index_lock().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut index = load_index(data_dir);
    purge_expired_locked(data_dir, &mut index, now);
    let id = format!("asset-{now}-{}", crate::commands::oauth::random_hex(12));
    let record = AssetRecord {
        id,
        owner_key_id: owner.to_string(),
        filename: safe_filename(filename, extension),
        mime_type: detected_mime.to_string(),
        extension: extension.to_string(),
        size: bytes.len() as u64,
        sha256: format!("{:x}", Sha256::digest(bytes)),
        created_at: now,
        expires_at: now.saturating_add(asset_ttl_secs()),
        public_token: crate::commands::oauth::random_hex(32),
    };
    let path = write_record_file(data_dir, &record, bytes)?;
    index.assets.push(record.clone());
    if let Err(error) = save_index(data_dir, &index) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(record)
}

/// 读取素材元数据并校验所有权，不读取文件内容。用于生成短时链接。
pub fn find_owned(
    data_dir: &Path,
    owner_key_id: &str,
    asset_id: &str,
) -> Result<AssetRecord, String> {
    let _guard = index_lock().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut index = load_index(data_dir);
    purge_expired_locked(data_dir, &mut index, now);
    let record = index
        .assets
        .iter()
        .find(|item| item.id == asset_id.trim() && item.owner_key_id == owner_key_id.trim())
        .cloned()
        .ok_or_else(|| "素材不存在、已过期或无权访问".to_string())?;
    Ok(record)
}

/// 生成短时公网内容地址。只有显式配置环境变量或网关设置页时可用；URL 中的
/// 随机 token 不是 API Key，过期后由清理任务删除对应文件。
pub fn public_url_for_owned(
    data_dir: &Path,
    owner_key_id: &str,
    asset_id: &str,
) -> Result<String, String> {
    let record = find_owned(data_dir, owner_key_id, asset_id)?;
    let base = public_base_url(data_dir)?;
    Ok(format!(
        "{base}/assets/{}/content?token={}",
        record.id, record.public_token
    ))
}

/// 通过短时 token 读取素材，供 Trae 上游从公网回取。token 不与 API Key 共用。
pub fn read_public(
    data_dir: &Path,
    asset_id: &str,
    token: &str,
) -> Result<(AssetRecord, Vec<u8>), String> {
    let _guard = index_lock().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut index = load_index(data_dir);
    purge_expired_locked(data_dir, &mut index, now);
    let record = index
        .assets
        .iter()
        .find(|item| {
            item.id == asset_id.trim()
                && !item.public_token.is_empty()
                && item.public_token == token.trim()
        })
        .cloned()
        .ok_or_else(|| "素材不存在、已过期或链接无效".to_string())?;
    let path = file_path(data_dir, &record)?;
    let bytes = fs::read(path).map_err(|e| format!("读取素材失败: {e}"))?;
    Ok((record, bytes))
}

/// 读取当前 API Key 所拥有的素材；供 Trae 原生上传适配器使用。
pub fn read_owned(
    data_dir: &Path,
    owner_key_id: &str,
    asset_id: &str,
) -> Result<(AssetRecord, Vec<u8>), String> {
    let _guard = index_lock().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut index = load_index(data_dir);
    purge_expired_locked(data_dir, &mut index, now);
    let record = index
        .assets
        .iter()
        .find(|item| item.id == asset_id.trim() && item.owner_key_id == owner_key_id.trim())
        .cloned()
        .ok_or_else(|| "素材不存在、已过期或无权访问".to_string())?;
    let path = file_path(data_dir, &record)?;
    let bytes = fs::read(path).map_err(|e| format!("读取素材失败: {e}"))?;
    Ok((record, bytes))
}

/// 清理过期素材；返回删除的文件数。
pub fn cleanup(data_dir: &Path) -> usize {
    let _guard = index_lock().lock().unwrap_or_else(|e| e.into_inner());
    let now = now_secs();
    let mut index = load_index(data_dir);
    let before = index.assets.len();
    purge_expired_locked(data_dir, &mut index, now);
    if index.assets.len() != before {
        let _ = save_index(data_dir, &index);
    }
    before.saturating_sub(index.assets.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_base_url_requires_https_by_default() {
        assert!(validate_public_base_url("https://example.test/v1", false).is_ok());
        assert!(validate_public_base_url("http://192.168.0.17/v1", false).is_err());
        assert!(validate_public_base_url("http://127.0.0.1:7864/v1", true).is_ok());
        assert!(validate_public_base_url("https://127.0.0.1/v1", true).is_err());
        assert!(validate_public_base_url("https://[::1]:443/v1", true).is_err());
        assert!(validate_public_base_url("https://asset.lan/v1", false).is_err());
        assert!(validate_public_base_url("https://example.test/v1?token=x", false).is_err());
    }

    #[test]
    fn asset_ttl_defaults_to_thirty_minutes_and_is_bounded() {
        assert_eq!(parse_ttl_secs(None), 30 * 60);
        assert_eq!(parse_ttl_secs(Some("60")), 5 * 60);
        assert_eq!(parse_ttl_secs(Some("7200")), 2 * 60 * 60);
        assert_eq!(parse_ttl_secs(Some("not-a-number")), 30 * 60);
    }

    #[test]
    fn detects_supported_formats_and_rejects_unknown() {
        assert_eq!(detect_format(b"\x89PNG\r\n\x1a\nrest"), Some(("image/png", "png")));
        assert_eq!(detect_format(b"\xff\xd8\xffrest"), Some(("image/jpeg", "jpg")));
        assert_eq!(detect_format(b"unknown"), None);
    }

    #[test]
    fn creates_reads_and_cleans_owned_asset() {
        let root = std::env::temp_dir().join(format!("aiwork_assets_{}", std::process::id()));
        let png = b"\x89PNG\r\n\x1a\nasset";
        std::fs::create_dir_all(root.join("data")).unwrap();
        crate::api_server::gateway_settings::save(
            &root,
            crate::api_server::gateway_settings::GatewaySettings {
                port: 7864,
                default_model: "deepseek-v4-flash".into(),
                listen_host: "127.0.0.1".into(),
                cors_origins: String::new(),
                asset_public_base_url: "https://example.test/v1/".into(),
                core_mode: "off".into(),
                updated_at: 0,
            },
        )
        .unwrap();
        let record = create(&root, "key-a", "..\\ref.png", Some("image/png"), png).unwrap();
        assert_eq!(record.owner_key_id, "key-a");
        assert_eq!(record.filename, "ref.png");
        assert!(!record.public_token.is_empty());
        let public_url = public_url_for_owned(&root, "key-a", &record.id).unwrap();
        assert!(public_url.starts_with("https://example.test/v1/assets/"));
        let (public_record, public_bytes) = read_public(&root, &record.id, &record.public_token).unwrap();
        assert_eq!(public_record.id, record.id);
        assert_eq!(public_bytes, png);
        assert!(read_public(&root, &record.id, "wrong-token").is_err());
        let (_, bytes) = read_owned(&root, "key-a", &record.id).unwrap();
        assert_eq!(bytes, png);
        assert!(read_owned(&root, "key-b", &record.id).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn core_asset_storage_does_not_touch_legacy_index_and_verifies_digest() {
        let root = std::path::PathBuf::from(r"D:\gpt").join(format!(
            "aiwork-assets-core-{}",
            rand::random::<u64>()
        ));
        let png = b"\x89PNG\r\n\x1a\ncore-asset";
        let (record, storage_ref) = write_core_asset(
            &root,
            "user-a",
            "..\\reference.png",
            Some("image/png"),
            png,
        )
        .unwrap();
        assert_eq!(record.owner_key_id, "user-a");
        assert_eq!(storage_ref, format!("assets/{}.png", record.id));
        assert!(!root.join("data").join("assets.json").exists());
        assert_eq!(read_core(&root, &storage_ref, record.size as i64, &record.sha256).unwrap(), png);
        assert!(read_core(&root, "assets/../secret", record.size as i64, &record.sha256).is_err());
        assert!(read_core(&root, &storage_ref, record.size as i64 + 1, &record.sha256).is_err());
        let _ = std::fs::remove_dir_all(root);
    }
}
