//! 为 BitBrowser 导入的账号生成可供原生 TRAE Work CN 切换桥使用的快照。
//!
//! BitBrowser 的 CDP 只能读取网页 localStorage，不能直接复制 Chromium 的 Cookie/密钥
//! 到原生 TRAE。因此首次导入时需要把 JWT 写入 TRAE 的 `tc` 加密 storage.json，并补齐
//! switch bridge 要求的 state.vscdb / machineid。凭据只在本进程内存中流转，快照文件位于
//! AI Work 助手自己的数据目录，后续切换时由现有 PowerShell 桥恢复。

use aes::Aes128;
use base64::Engine;
use cbc::Encryptor;
use chrono::{SecondsFormat, Utc};
use rand::RngCore;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha512};
use std::fs;
use std::path::{Path, PathBuf};

use crate::jwt;
use crate::state::AppState;

const JQ: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251,
    124, 227, 57, 130, 155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203,
    84, 123, 148, 50, 166, 194, 35, 61, 238, 76, 149, 11, 66, 250, 195, 78,
    8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109, 139, 209, 37,
];
const WQ: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95,
    96, 81, 127, 169, 25, 181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239,
    160, 224, 59, 77, 174, 42, 245, 176, 200, 235, 187, 60, 131, 83, 153, 97,
    23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33, 12, 125,
];

#[derive(Debug, Clone, Default)]
pub(crate) struct NativeSnapshotOutcome {
    pub created: bool,
    pub updated: bool,
}

fn random_hex(bytes: usize) -> String {
    let mut raw = vec![0u8; bytes];
    rand::thread_rng().fill_bytes(&mut raw);
    raw.iter().map(|b| format!("{b:02x}")).collect()
}

fn derive_key_iv(embedded_key: &[u8]) -> ([u8; 16], [u8; 16]) {
    let salt: [u8; 64] = std::array::from_fn(|i| JQ[i] ^ WQ[i]);
    let first = Sha512::digest(embedded_key);
    let second = Sha512::digest([first.as_slice(), &salt].concat());
    let mut key = [0u8; 16];
    let mut iv = [0u8; 16];
    key.copy_from_slice(&second[..16]);
    iv.copy_from_slice(&second[16..32]);
    (key, iv)
}

/// Trae CN/Work 的 `tc` 格式：header + 随机嵌入密钥 + AES-128-CBC(SHA512(JSON)||JSON)。
fn encrypt_tc(plaintext: &str) -> Result<String, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockEncryptMut, KeyIvInit};
    let mut embedded_key = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut embedded_key);
    let (key, iv) = derive_key_iv(&embedded_key);
    let digest = Sha512::digest(plaintext.as_bytes());
    let mut data = Vec::with_capacity(64 + plaintext.len() + 16);
    data.extend_from_slice(&digest);
    data.extend_from_slice(plaintext.as_bytes());
    let message_len = data.len();
    data.resize(message_len + 16, 0);
    let ciphertext = Encryptor::<Aes128>::new(&key.into(), &iv.into())
        .encrypt_padded_mut::<Pkcs7>(&mut data, message_len)
        .map_err(|e| format!("TRAE tc 加密失败: {e}"))?;
    let mut output = Vec::with_capacity(38 + ciphertext.len());
    output.extend_from_slice(b"tc\x05\x10\x00\x00");
    output.extend_from_slice(&embedded_key);
    output.extend_from_slice(ciphertext);
    Ok(base64::engine::general_purpose::STANDARD.encode(output))
}

/// 解密 Trae 原生 storage.json 的 `tc` 容器，仅在本机内存中返回明文。
/// 调用方不得把返回值写入日志或前端；用于账号发现时只提取 userId/JWT 过期时间。
pub(crate) fn decrypt_tc(encoded: &str) -> Result<String, String> {
    use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
    use cbc::Decryptor;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| e.to_string())?;
    if raw.len() < 54 || &raw[..2] != b"tc" || raw[2] != 5 {
        return Err("invalid tc header".into());
    }
    let (key, iv) = derive_key_iv(&raw[6..38]);
    let mut data = raw[38..].to_vec();
    let plain = Decryptor::<Aes128>::new(&key.into(), &iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut data)
        .map_err(|e| format!("decrypt failed: {e}"))?;
    if plain.len() < 64 || Sha512::digest(&plain[64..]).as_slice() != &plain[..64] {
        return Err("tc integrity check failed".into());
    }
    String::from_utf8(plain[64..].to_vec()).map_err(|e| e.to_string())
}

fn iso_millis(timestamp: i64) -> String {
    chrono::DateTime::<Utc>::from_timestamp(timestamp, 0)
        .unwrap_or_else(Utc::now)
        .to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 原生 TRAE storage.json 的 token 字段保存裸 JWT，不带 API 请求使用的
/// `Cloud-IDE-JWT ` 鉴权前缀。账号库/API 层仍保留带前缀形式，这里只在
/// 写入原生快照时做格式转换。
fn native_token(token: &str) -> String {
    token
        .trim()
        .strip_prefix("Cloud-IDE-JWT ")
        .unwrap_or(token.trim())
        .trim()
        .to_string()
}

fn native_storage_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("TRAE SOLO CN")
            .join("User")
            .join("globalStorage")
            .join("storage.json")
    })
}

fn copy_tree(src: &Path, dst: &Path) -> Result<bool, String> {
    if !src.exists() {
        return Ok(false);
    }
    if src.is_dir() {
        fs::create_dir_all(dst).map_err(|e| format!("创建快照目录失败: {e}"))?;
        for entry in fs::read_dir(src).map_err(|e| format!("读取快照目录失败: {e}"))? {
            let entry = entry.map_err(|e| format!("读取快照项失败: {e}"))?;
            copy_tree(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("创建快照父目录失败: {e}"))?;
        }
        fs::copy(src, dst).map_err(|e| format!("复制快照文件失败: {e}"))?;
    }
    Ok(true)
}

fn write_atomic(path: &Path, content: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp-{}", random_hex(8)));
    fs::write(&tmp, content).map_err(|e| format!("写入临时快照失败: {e}"))?;
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(path);
        fs::rename(&tmp, path).map_err(|e| format!("替换快照文件失败: {error}; {e}"))?;
    }
    Ok(())
}

fn build_auth_info(user_id: &str, token: &str, refresh_token: Option<&str>, name: &str) -> Value {
    let now = Utc::now().timestamp();
    let token = native_token(token);
    let exp = jwt::parse(&token)
        .exp_timestamp
        .filter(|value| *value > now)
        .unwrap_or(now + 14 * 24 * 3600);
    json!({
        "token": token,
        "refreshToken": refresh_token.unwrap_or_default(),
        "expiredAt": iso_millis(exp),
        "refreshExpiredAt": iso_millis(now + 180 * 24 * 3600),
        "tokenReleaseAt": iso_millis(now),
        "userId": user_id,
        "host": "https://api.trae.cn",
        "userRegion": { "region": "CN", "_aiRegion": "CN" },
        "account": {
            "username": name,
            "iss": "", "iat": 0, "organization": "", "work_country": "",
            "email": "", "avatar_url": "", "description": "",
            "scope": "marscode", "loginScope": "trae", "storeCountryCode": "cn",
            "storeCountrySrc": "uid", "storeRegion": "CN", "userTag": "cn"
        }
    })
}

fn storage_with_auth(template: Option<&Path>, user_id: &str, token: &str, refresh_token: Option<&str>, name: &str) -> Result<String, String> {
    let mut storage: Value = template
        .and_then(|path| fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_else(|| Value::Object(Map::new()));
    let object = storage
        .as_object_mut()
        .ok_or_else(|| "TRAE storage.json 不是 JSON 对象".to_string())?;
    object.insert(
        "iCubeAuthInfo://icube.cloudide".into(),
        Value::String(encrypt_tc(&serde_json::to_string(&build_auth_info(user_id, token, refresh_token, name)).map_err(|e| e.to_string())?)?),
    );
    object.insert(
        "iCubeAuthInfo://usertag".into(),
        // usertag 与 cloudide auth 使用相同的 tc 加密容器；明文 JSON 会被
        // Trae 启动时视为损坏并清除整组登录态。
        Value::String(encrypt_tc(&serde_json::to_string(&json!({ user_id: "cn" })).map_err(|e| e.to_string())?)?),
    );
    // 不把当前账号的额度缓存带进目标快照，启动后由 TRAE 重新请求目标账号数据；
    // 保留一个最小 entitlement 占位，避免旧版客户端因缺少该键误判为未初始化。
    object.remove("iCubeServerData://icube.cloudide");
    object.insert(
        "iCubeEntitlementInfo://icube.cloudide".into(),
        Value::String(
            serde_json::to_string(&json!({
                "identityStr": "Free", "identity": 0, "isPayFreshman": false,
                "isSupportCommercialization": true, "hasPackage": false,
                "enableEntitlement": true,
                "detail": { "can_gen_solo_code": false, "fast_request_per": 1,
                    "in_wait": false, "permission": 1, "canGenSoloCode": false,
                    "fastRequestPer": 1, "inWaitlist": false }
            }))
            .map_err(|e| e.to_string())?,
        ),
    );
    Ok(serde_json::to_string_pretty(&storage).map_err(|e| format!("序列化 storage.json 失败: {e}"))?)
}

fn ensure_state_db(path: &Path, template: Option<&Path>) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    if let Some(template) = template {
        if template.exists() {
            fs::copy(template, path).map_err(|e| format!("复制 state.vscdb 失败: {e}"))?;
            return Ok(());
        }
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建 state.vscdb 目录失败: {e}"))?;
    }
    let conn = rusqlite::Connection::open(path).map_err(|e| format!("创建 state.vscdb 失败: {e}"))?;
    conn.execute_batch("CREATE TABLE IF NOT EXISTS ItemTable (key TEXT PRIMARY KEY, value TEXT);")
        .map_err(|e| format!("初始化 state.vscdb 失败: {e}"))?;
    Ok(())
}

/// 首次导入或 BitBrowser 续期后，确保账号拥有可供原生切换桥恢复的快照。
pub(crate) fn ensure_native_snapshot(
    state: &AppState,
    user_id: &str,
    token: &str,
    refresh_token: Option<&str>,
    name: &str,
) -> Result<NativeSnapshotOutcome, String> {
    if user_id.trim().is_empty()
        || user_id.len() > 64
        || !user_id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        return Err("无效的账号 UserID，无法创建原生快照".into());
    }
    let profiles = state.path("profiles");
    fs::create_dir_all(&profiles).map_err(|e| format!("创建账号快照目录失败: {e}"))?;
    let destination = profiles.join(user_id);
    let existed = destination.is_dir();
    let native_root = native_storage_path().map(|path| {
        path.parent().unwrap().parent().unwrap().parent().unwrap().to_path_buf()
    });
    let native_storage = native_storage_path();
    let template_storage = if native_storage.as_ref().is_some_and(|p| p.exists()) {
        native_storage.clone()
    } else {
        Some(destination.join("User").join("globalStorage").join("storage.json"))
    };

    if !existed {
        let staging = profiles.join(format!(".seed-{user_id}-{}", random_hex(8)));
        fs::create_dir_all(&staging).map_err(|e| format!("创建快照临时目录失败: {e}"))?;
        let result = (|| {
            let storage_path = staging.join("User").join("globalStorage").join("storage.json");
            fs::create_dir_all(storage_path.parent().unwrap()).map_err(|e| e.to_string())?;
            let storage = storage_with_auth(template_storage.as_deref(), user_id, token, refresh_token, name)?;
            write_atomic(&storage_path, &storage)?;
            let native_user = native_root.as_ref().map(|p| p.join("User"));
            if let Some(root) = native_user.as_ref() {
                for relative in ["Preferences", "Local State", "Local Storage", "Network", "Session Storage"] {
                    let _ = copy_tree(&root.join(relative), &staging.join("User").join(relative));
                }
            }
            // 首次从 BitBrowser 导入时不复制当前原生账号的 state.vscdb，避免把
            // 另一账号的 UI/会话状态带进新快照；创建兼容 VS Code ItemTable 的空库。
            ensure_state_db(&staging.join("User").join("globalStorage").join("state.vscdb"), None)?;
            let native_data = native_root.as_ref();
            if let Some(root) = native_data {
                let _ = copy_tree(&root.join("aha"), &staging.join("aha"));
                let _ = copy_tree(&root.join("machineid"), &staging.join("machineid"));
                if !staging.join("machineid").exists() {
                    fs::write(staging.join("machineid"), random_hex(32)).map_err(|e| e.to_string())?;
                }
                let _ = copy_tree(&root.join("Partitions").join("trae-webview"), &staging.join("Partitions").join("trae-webview"));
                let _ = copy_tree(&root.join("Partitions").join("icube-web-crawler-shared-session-v1.0"), &staging.join("Partitions").join("icube-web-crawler-shared-session-v1.0"));
            }
            let meta = json!({"schemaVersion": 1, "layout": "icube", "source": "bitbrowser", "uid": user_id, "savedAt": Utc::now().to_rfc3339()});
            write_atomic(&staging.join("meta.json"), &serde_json::to_string_pretty(&meta).map_err(|e| e.to_string())?)?;
            fs::rename(&staging, &destination).map_err(|e| format!("提交账号原生快照失败: {e}"))
        })();
        if result.is_err() { let _ = fs::remove_dir_all(&staging); }
        result?;
        return Ok(NativeSnapshotOutcome { created: true, updated: false });
    }

    let storage_path = destination.join("User").join("globalStorage").join("storage.json");
    fs::create_dir_all(storage_path.parent().unwrap()).map_err(|e| format!("创建快照目录失败: {e}"))?;
    let storage = storage_with_auth(Some(&storage_path), user_id, token, refresh_token, name)?;
    write_atomic(&storage_path, &storage)?;
    let native_data = native_root.as_ref().map(|p| p.join("User").join("globalStorage").join("state.vscdb"));
    ensure_state_db(&destination.join("User").join("globalStorage").join("state.vscdb"), native_data.as_deref())?;
    if !destination.join("machineid").exists() {
        fs::write(destination.join("machineid"), random_hex(32)).map_err(|e| format!("写入快照 machineid 失败: {e}"))?;
    }
    Ok(NativeSnapshotOutcome { created: false, updated: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tc_roundtrip() {
        let encrypted = encrypt_tc(r#"{"userId":"u-1","token":"abc"}"#).unwrap();
        assert_eq!(decrypt_tc(&encrypted).unwrap(), r#"{"userId":"u-1","token":"abc"}"#);
    }

    #[test]
    fn auth_info_has_native_shape() {
        let value = build_auth_info("u-1", "Cloud-IDE-JWT bad-token", None, "demo");
        assert_eq!(value["userId"], "u-1");
        assert_eq!(value["token"], "bad-token");
        assert!(value["expiredAt"].as_str().unwrap().ends_with('Z'));
    }

    #[test]
    fn seeds_minimal_icube_snapshot() {
        let root = std::env::temp_dir().join(format!("aiwork_snapshot_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let native = root.join("TRAE SOLO CN").join("User").join("globalStorage");
        fs::create_dir_all(&native).unwrap();
        fs::write(native.join("storage.json"), r#"{"telemetry.machineId":"test-machine"}"#).unwrap();
        let state = AppState {
            data_dir: root.join("assistant"),
            python_dir: root.clone(),
            python_exe: String::new(),
            jwt_refresh_lock: std::sync::Mutex::new(()),
        };
        fs::create_dir_all(&state.data_dir).unwrap();
        let old_appdata = std::env::var_os("APPDATA");
        std::env::set_var("APPDATA", &root);
        let out = ensure_native_snapshot(&state, "u-test", "not-a-jwt", None, "Demo").unwrap();
        let snapshot = state.path("profiles").join("u-test");
        assert!(out.created && snapshot.join("User/globalStorage/storage.json").exists());
        assert!(snapshot.join("User/globalStorage/state.vscdb").exists());
        let storage: Value = serde_json::from_str(&fs::read_to_string(snapshot.join("User/globalStorage/storage.json")).unwrap()).unwrap();
        let auth = storage["iCubeAuthInfo://icube.cloudide"].as_str().unwrap();
        let decoded: Value = serde_json::from_str(&decrypt_tc(auth).unwrap()).unwrap();
        assert_eq!(decoded["userId"], "u-test");
        assert_eq!(decoded["token"], "not-a-jwt");
        let usertag = storage["iCubeAuthInfo://usertag"].as_str().unwrap();
        let usertag_decoded: Value = serde_json::from_str(&decrypt_tc(usertag).unwrap()).unwrap();
        assert_eq!(usertag_decoded["u-test"], "cn");
        if let Some(value) = old_appdata { std::env::set_var("APPDATA", value); } else { std::env::remove_var("APPDATA"); }
        let _ = fs::remove_dir_all(root);
    }
}
