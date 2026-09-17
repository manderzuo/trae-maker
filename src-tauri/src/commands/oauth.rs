use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tauri::State;

use crate::fs_utils;
use crate::jwt;
use crate::models::RawAccount;
use crate::state::AppState;

/// 最近签发的 OAuth state（CSRF 防护）：oauth_get_login_url 签发时记录，
/// oauth_parse_callback 在回调携带 state 且本进程签发过时强校验一致性
static LAST_OAUTH_STATE: Mutex<Option<String>> = Mutex::new(None);
/// 普通浏览器 OAuth 的本机回环回调。只保存一次性 URL，不保存 Token。
static LAST_OAUTH_CALLBACK: Mutex<Option<String>> = Mutex::new(None);
/// 普通浏览器 OAuth 的原生 PKCE 请求，供回调到达后完成 AuthCode 交换。
static LAST_NATIVE_REQUEST: Mutex<Option<NativeOAuthRequest>> = Mutex::new(None);
static OAUTH_CALLBACK_LISTENER_ACTIVE: AtomicBool = AtomicBool::new(false);

/// OAuth 常量
const OAUTH_CLIENT_ID: &str = "en1oxy7wnw8j9n";
const OAUTH_CLIENT_SECRET: &str = "-";
const OAUTH_REDIRECT_URI: &str = "http://127.0.0.1:17388/authorize";

/// OAuth 回调解析结果
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct OAuthCallbackInfo {
    pub refresh_token: String,
    pub access_token: Option<String>,
    pub user_id: Option<String>,
    pub user_name: Option<String>,
    pub avatar: Option<String>,
    #[serde(default)]
    pub auth_code: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub login_trace_id: Option<String>,
}

/// OAuth 登录 URL 响应
#[derive(Serialize)]
pub struct OAuthLoginUrl {
    pub url: String,
    pub state: String,
    pub redirect_uri: String,
    /// 是否已在本机成功绑定回环监听；false 时仍可手动粘贴回调。
    pub auto_callback: bool,
}

/// OAuth 登录完成后的账号信息
#[derive(Serialize)]
pub struct OAuthLoginResult {
    pub user_id: String,
    pub name: String,
    pub jwt: String,
    pub refresh_token: String,
    pub has_refresh_token: bool,
    #[serde(default)]
    pub token_exp_timestamp: Option<i64>,
    #[serde(default)]
    pub refresh_exp_timestamp: Option<i64>,
    #[serde(default)]
    pub credential_source: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
struct NativeDeviceInfo {
    #[serde(rename = "DeviceID")]
    device_id: String,
    #[serde(rename = "MachineID")]
    machine_id: String,
    #[serde(rename = "PlatformCode")]
    platform_code: String,
    #[serde(rename = "DeviceType")]
    device_type: String,
    #[serde(rename = "DeviceName")]
    device_name: String,
    #[serde(rename = "DeviceModel")]
    device_model: String,
    #[serde(rename = "ClientVersion")]
    client_version: String,
    #[serde(rename = "DevicePublicKey")]
    device_public_key: String,
    #[serde(rename = "DeviceBrand")]
    device_brand: String,
    #[serde(rename = "DeviceCPU")]
    device_cpu: String,
    #[serde(rename = "OSInfo")]
    os_info: String,
    #[serde(rename = "OSVersion")]
    os_version: String,
}

#[derive(Clone, Debug)]
struct NativeOAuthRequest {
    state: String,
    verifier: String,
    trace_id: String,
    device: NativeDeviceInfo,
    callback_port: u16,
    url: String,
}

#[derive(Clone, Debug)]
struct NativeCallback {
    auth_code: String,
    user_name: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct NativeTokenExchange {
    pub(crate) jwt: String,
    pub(crate) refresh_token: String,
    pub(crate) user_id: String,
    pub(crate) name: Option<String>,
    pub(crate) token_exp_timestamp: Option<i64>,
    pub(crate) refresh_exp_timestamp: Option<i64>,
}

/// 短请求 Agent（项目未启用 ureq 的 proxy-from-env feature，Agent 默认直连）
fn short_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(120))
        .max_idle_connections(20)
        .max_idle_connections_per_host(20)
        .build()
}

fn storage_json_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|appdata| {
        PathBuf::from(appdata)
            .join("TRAE SOLO CN")
            .join("User")
            .join("globalStorage")
            .join("storage.json")
    })
}

fn storage_string(storage: &Value, key: &str) -> Option<String> {
    storage
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn extract_log_field(line: &str, field: &str) -> Option<String> {
    let marker = format!(r#""{field}":""#);
    let start = line.find(&marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find(r#"","#).or_else(|| rest.find(r#""}"#))?;
    Some(
        rest[..end]
            .replace(r#"\n"#, "\n")
            .replace(r#"\""#, r#"""#),
    )
}

/// 读取原生 Trae 最近一次 OAuth 请求中记录的设备描述。
/// DevicePublicKey 是公钥，可安全复用；私钥从不读取或落盘。
fn latest_native_device_fields() -> HashMap<String, String> {
    let Some(appdata) = std::env::var_os("APPDATA") else {
        return HashMap::new();
    };
    let root = PathBuf::from(appdata).join("TRAE SOLO CN").join("logs");
    let Ok(entries) = std::fs::read_dir(root) else {
        return HashMap::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    dirs.sort_by(|a, b| b.file_name().cmp(&a.file_name()));

    for dir in dirs {
        let path = dir.join("main.log");
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().rev() {
            if !line.contains(r#""DevicePublicKey":"#) {
                continue;
            }
            let mut fields = HashMap::new();
            for key in [
                "DevicePublicKey",
                "DeviceBrand",
                "DeviceCPU",
                "DeviceModel",
                "DeviceName",
                "OSVersion",
                "ClientVersion",
            ] {
                if let Some(value) = extract_log_field(line, key) {
                    fields.insert(key.to_string(), value);
                }
            }
            if !fields.is_empty() {
                return fields;
            }
        }
    }
    HashMap::new()
}

fn native_device_info() -> NativeDeviceInfo {
    let storage = storage_json_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let log_fields = latest_native_device_fields();

    let machine_id = storage_string(&storage, "telemetry.machineId")
        .filter(|value| value.len() >= 32)
        .unwrap_or_else(|| random_hex(64));
    let device_id = storage
        .as_object()
        .and_then(|object| {
            object.keys().find_map(|key| {
                key.strip_prefix("iCubeAuthInfo://icube-dc:aha-")
                    .map(|tail| format!("aha-{tail}"))
            })
        })
        .filter(|value| value.len() >= 12)
        .unwrap_or_else(|| format!("aha-{}", random_hex(32)));
    let computer_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".into());
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| "Windows CPU".into());

    NativeDeviceInfo {
        device_id,
        machine_id,
        platform_code: "SOLO_PC".into(),
        device_type: "PC".into(),
        device_name: log_fields
            .get("DeviceName")
            .cloned()
            .unwrap_or(computer_name),
        device_model: log_fields
            .get("DeviceModel")
            .cloned()
            .unwrap_or_else(|| "Windows PC".into()),
        client_version: log_fields
            .get("ClientVersion")
            .cloned()
            .unwrap_or_else(|| "0.1.65".into()),
        device_public_key: log_fields
            .get("DevicePublicKey")
            .cloned()
            .unwrap_or_default(),
        device_brand: log_fields
            .get("DeviceBrand")
            .cloned()
            .unwrap_or_else(|| "Windows".into()),
        device_cpu: log_fields
            .get("DeviceCPU")
            .cloned()
            .unwrap_or(cpu),
        os_info: "windows".into(),
        os_version: log_fields
            .get("OSVersion")
            .cloned()
            .unwrap_or_else(|| "Windows".into()),
    }
}

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn trace_id() -> String {
    format!(
        "{}-{}-4{}-a{}-{}",
        random_hex(8),
        random_hex(4),
        random_hex(3),
        random_hex(3),
        random_hex(12)
    )
}

fn query_param(key: &str, value: &str) -> String {
    format!(
        "{}={}",
        urlencoding::encode(key),
        urlencoding::encode(value)
    )
}

fn build_native_oauth_request(callback_port: u16) -> NativeOAuthRequest {
    let state = random_hex(32);
    // random_hex 本身使用 Windows BCryptGenRandom；将其作为 verifier 的
    // 字节源再做一次 URL-safe 编码，满足 PKCE 的高熵要求。
    let verifier = base64url(random_hex(96).as_bytes());
    let challenge = base64url(&Sha256::digest(verifier.as_bytes()));
    let trace_id = trace_id();
    let device = native_device_info();
    let mut params = vec![
        query_param("login_version", "1"),
        query_param("auth_from", "solo"),
        query_param("login_channel", "native_ide"),
        query_param("plugin_version", &device.client_version),
        query_param("auth_type", "local"),
        query_param("client_id", OAUTH_CLIENT_ID),
        query_param("redirect", "0"),
        query_param("login_trace_id", &trace_id),
        query_param(
            "auth_callback_url",
            &format!("http://127.0.0.1:{callback_port}/authorize"),
        ),
        query_param("machine_id", &device.machine_id),
        query_param("device_id", &device.device_id),
        query_param("x_device_id", &device.device_id),
        query_param("x_machine_id", &device.machine_id),
        query_param("x_device_brand", &device.device_brand),
        query_param("x_device_type", "windows"),
        query_param("x_os_version", &device.os_version),
        query_param("x_env", ""),
        query_param("x_app_version", &device.client_version),
        query_param("x_app_type", "stable"),
        query_param("code_challenge", &challenge),
        query_param("code_challenge_method", "S256"),
        query_param("hide_saas_login", "true"),
        query_param("channel_name", "common"),
        query_param("click_id", "CN-Setup-x64"),
    ];
    // 该参数不是原生页面的必需项，但便于助手在回调阶段做额外关联。
    params.push(query_param("state", &state));
    NativeOAuthRequest {
        state,
        verifier,
        trace_id,
        device,
        callback_port,
        url: format!("https://www.trae.cn/authorization?{}", params.join("&")),
    }
}

/// 生成随机 hex 字符串。
/// 熵源：OS CSPRNG（Windows BCryptGenRandom 系统首选 RNG）。旧 LCG 以时间戳作种子，
/// 输出可预测，不适合 OAuth state / machine_id 等安全场景（审查 P2）；BCrypt 失败时
/// 保留 LCG 兜底（仅影响随机性，不中断流程）。
pub(crate) fn random_hex(len: usize) -> String {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCryptGenRandom, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        };
        let mut bytes = vec![0u8; len.div_ceil(2)];
        let halg: windows_sys::Win32::Security::Cryptography::BCRYPT_ALG_HANDLE =
            unsafe { std::mem::zeroed() };
        // STATUS_SUCCESS == 0
        let status = unsafe {
            BCryptGenRandom(halg, bytes.as_mut_ptr(), bytes.len() as u32, BCRYPT_USE_SYSTEM_PREFERRED_RNG)
        };
        if status == 0 {
            let mut out = String::with_capacity(len);
            for b in bytes {
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
                if out.len() >= len {
                    break;
                }
                out.push(char::from_digit((b & 0xF) as u32, 16).unwrap_or('0'));
            }
            return out;
        }
    }
    // 兜底：旧 LCG（仅非 Windows 或 BCrypt 调用失败时）
    use std::time::{SystemTime, UNIX_EPOCH};
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(42);
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        // 简单 LCG
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let nibble = ((seed >> 32) & 0xF) as u8;
        out.push(if nibble < 10 {
            (b'0' + nibble) as char
        } else {
            (b'a' + nibble - 10) as char
        });
    }
    out
}

fn parse_query_params(target: &str) -> HashMap<String, String> {
    let Some(query) = target.split_once('?').map(|(_, query)| query) else {
        return HashMap::new();
    };
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let key = urlencoding::decode(key)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| key.to_string());
            let value = urlencoding::decode(value)
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| value.to_string());
            (key, value)
        })
        .collect()
}

fn json_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|number| i64::try_from(number).ok()))
            .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
    })
}

fn json_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|value| !value.trim().is_empty())
}

fn parse_native_callback(
    target: &str,
    expected_state: &str,
    expected_trace_id: &str,
) -> Result<NativeCallback, String> {
    let params = parse_query_params(target);
    if let Some(state) = params.get("state") {
        if state != expected_state {
            return Err("OAuth state 校验失败：BitBrowser 回调不是本次登录请求".into());
        }
    }
    if let Some(trace_id) = params.get("loginTraceID") {
        if trace_id != expected_trace_id {
            return Err("OAuth loginTraceID 校验失败：回调不是本次原生登录请求".into());
        }
    }

    let info_raw = params
        .get("authCodeInfo")
        .ok_or_else(|| "原生 Trae 回调中缺少 authCodeInfo".to_string())?;
    let info: Value = serde_json::from_str(info_raw)
        .map_err(|_| "原生 Trae 回调中的 authCodeInfo 格式无效".to_string())?;
    let auth_code = json_string(info.get("AuthCode"))
        .ok_or_else(|| "原生 Trae 回调中缺少 AuthCode".to_string())?;
    if auth_code.len() > 512 {
        return Err("原生 Trae 回调 AuthCode 长度异常".into());
    }

    let user_name = params.get("userInfo").and_then(|raw| {
        let value = serde_json::from_str::<Value>(raw).ok()?;
        ["ScreenName", "UserName", "nickname", "name"]
            .iter()
            .find_map(|key| json_string(value.get(*key)))
    });
    Ok(NativeCallback {
        auth_code,
        user_name,
    })
}

fn read_http_target(stream: &mut TcpStream) -> Option<String> {
    let mut buffer = [0u8; 8192];
    let mut received = Vec::new();
    loop {
        match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(size) => {
                received.extend_from_slice(&buffer[..size]);
                if received.windows(4).any(|window| window == b"\r\n\r\n")
                    || received.len() >= 64 * 1024
                {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(_) => return None,
        }
    }
    let request = String::from_utf8_lossy(&received);
    request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .map(str::to_string)
}

fn send_callback_response(stream: &mut TcpStream, status: &str, body: &str) {
    let body_bytes = body.as_bytes();
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body_bytes.len(), body
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

/// 在固定回环端口接收普通浏览器 OAuth 回调。
///
/// 监听器只读取 HTTP 请求行并保存回调 URL，绝不把查询参数写入日志；
/// `oauth_poll_callback` 取走后即清空，避免回调被重复消费。
fn start_oauth_callback_listener() -> bool {
    *LAST_OAUTH_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) = None;
    if OAUTH_CALLBACK_LISTENER_ACTIVE.swap(true, Ordering::SeqCst) {
        return true;
    }
    let listener = match TcpListener::bind(("127.0.0.1", 17388)) {
        Ok(listener) => listener,
        Err(error) => {
            OAUTH_CALLBACK_LISTENER_ACTIVE.store(false, Ordering::SeqCst);
            // 端口被其他本机进程占用时保留手动粘贴回退，不记录 URL 或 Token。
            eprintln!("OAuth 回环监听未启动: {error}");
            return false;
        }
    };
    std::thread::spawn(move || {
        let _ = listener.set_nonblocking(true);
        let deadline = Instant::now() + Duration::from_secs(600);
        while Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let Some(target) = read_http_target(&mut stream) else {
                        send_callback_response(&mut stream, "400 Bad Request", "无效回调");
                        continue;
                    };
                    if !target.starts_with("/authorize") {
                        send_callback_response(&mut stream, "404 Not Found", "not found");
                        continue;
                    }
                    if parse_query_params(&target).is_empty() {
                        send_callback_response(&mut stream, "200 OK", "等待授权回调");
                        continue;
                    }
                    let callback = format!("http://127.0.0.1:17388{target}");
                    *LAST_OAUTH_CALLBACK.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(callback);
                    send_callback_response(
                        &mut stream,
                        "200 OK",
                        "授权成功，可以关闭此页面并返回 AI Work 助手。",
                    );
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                }
                Err(_) => break,
            }
        }
        OAUTH_CALLBACK_LISTENER_ACTIVE.store(false, Ordering::SeqCst);
    });
    true
}

fn wait_native_callback_timeout(
    listener: TcpListener,
    callback_port: u16,
    expected_state: &str,
    expected_trace_id: &str,
    timeout: Duration,
) -> Result<NativeCallback, String> {
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("设置 OAuth 回调监听失败：{error}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                let Some(target) = read_http_target(&mut stream) else {
                    send_callback_response(&mut stream, "400 Bad Request", "无效回调");
                    continue;
                };
                if !target.starts_with("/authorize") {
                    send_callback_response(&mut stream, "404 Not Found", "not found");
                    continue;
                }
                let params = parse_query_params(&target);
                if params.is_empty() {
                    // Chromium 可能先请求一次无参数的回调路径，不能将其当作登录完成。
                    send_callback_response(&mut stream, "200 OK", "等待授权回调");
                    continue;
                }
                match parse_native_callback(&target, expected_state, expected_trace_id) {
                    Ok(callback) => {
                        send_callback_response(
                            &mut stream,
                            "200 OK",
                            "授权成功，可以关闭此页面。",
                        );
                        return Ok(callback);
                    }
                    Err(error) => {
                        last_error = error;
                        send_callback_response(&mut stream, "400 Bad Request", "授权回调无效");
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => return Err(format!("读取 OAuth 回调失败：{error}")),
        }
    }
    if last_error.is_empty() {
        Err(format!("等待 Trae OAuth 回调超时（端口 {callback_port}）"))
    } else {
        Err(format!("等待 Trae OAuth 回调超时：{last_error}"))
    }
}

fn exchange_native_auth_code(
    callback: &NativeCallback,
    request: &NativeOAuthRequest,
    cloudide_token: Option<String>,
) -> Result<NativeTokenExchange, String> {
    let mut request_builder = short_agent()
        .post("https://api.trae.cn/trae/api/v3/oauth/ExchangeToken")
        .set("content-type", "application/json")
        .set("accept", "application/json");
    if let Some(token) = cloudide_token.filter(|token| !token.trim().is_empty()) {
        request_builder = request_builder.set("x-cloudide-token", &token);
    }
    let ide_version = request.device.client_version.clone();
    let response = request_builder
        .send_json(ureq::json!({
            "ClientID": OAUTH_CLIENT_ID,
            "AuthCode": callback.auth_code,
            "CodeVerifier": request.verifier,
            "DeviceInfo": request.device.clone(),
            "IDEVersion": ide_version,
        }))
        .map_err(|error| format!("原生 Trae ExchangeToken 请求失败：{error}"))?;
    let body: Value = response
        .into_json()
        .map_err(|error| format!("解析原生 Trae ExchangeToken 响应失败：{error}"))?;
    if let Some(error) = body
        .get("ResponseMetadata")
        .and_then(|metadata| metadata.get("Error"))
    {
        let code = json_string(error.get("Code")).unwrap_or_else(|| "unknown".into());
        let message = json_string(error.get("Message")).unwrap_or_else(|| "未知错误".into());
        return Err(format!("原生 Trae ExchangeToken 失败（{code}）：{message}"));
    }
    let result = body
        .get("Result")
        .or_else(|| body.get("result"))
        .or_else(|| body.get("data"))
        .ok_or_else(|| "原生 Trae ExchangeToken 响应缺少 Result".to_string())?;
    let jwt = json_string(result.get("UserJwt"))
        .or_else(|| json_string(result.get("Token")))
        .map(|token| normalise_access_token(&token))
        .filter(|token| !token.is_empty())
        .ok_or_else(|| "原生 Trae ExchangeToken 响应缺少 UserJwt/Token".to_string())?;
    let refresh_token = json_string(result.get("RefreshToken"))
        .or_else(|| json_string(result.get("refresh_token")))
        .ok_or_else(|| "原生 Trae ExchangeToken 响应缺少 RefreshToken".to_string())?;
    let jwt_info = crate::jwt::parse(&jwt);
    let user_id = jwt_info
        .user_id
        .or_else(|| json_string(result.get("UserID")))
        .ok_or_else(|| "无法从原生 Trae Token 获取 UserID".to_string())?;
    let token_exp_timestamp = jwt_info.exp_timestamp.or_else(|| {
        json_i64(result.get("TokenExpireAt")).map(|millis| millis / 1000)
    });
    let refresh_exp_timestamp = json_i64(result.get("RefreshExpireAt")).map(|millis| millis / 1000);
    Ok(NativeTokenExchange {
        jwt,
        refresh_token,
        user_id,
        name: callback.user_name.clone(),
        token_exp_timestamp,
        refresh_exp_timestamp,
    })
}

fn normalise_access_token(value: &str) -> String {
    let token = value.trim();
    token
        .strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| token.strip_prefix("Bearer "))
        .unwrap_or(token)
        .trim()
        .to_string()
}

pub(crate) fn persist_native_oauth_account(
    state: &AppState,
    exchange: NativeTokenExchange,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    let mut name = account_name
        .filter(|name| !name.trim().is_empty())
        .or(exchange.name.clone())
        .unwrap_or_default();
    if name.trim().is_empty() {
        name = get_user_info(&exchange.jwt)
            .map(|(_, user_name)| user_name)
            .unwrap_or_default();
    }
    if name.trim().is_empty() {
        let head: String = exchange.user_id.chars().take(8).collect();
        name = format!("账号_{head}");
    }

    let mut accounts = crate::vault::load_accounts(state);
    let mut updated = false;
    if let Some(account) = accounts
        .accounts
        .iter_mut()
        .find(|account| account.user_id.as_deref() == Some(exchange.user_id.as_str()))
    {
        account.jwt = exchange.jwt.clone();
        account.refresh_token = Some(exchange.refresh_token.clone());
        account.credential_source = Some("native_oauth".into());
        if account.name.trim().is_empty() || account.name.starts_with("Trae Work-…") {
            account.name = name.clone();
        }
        account.updated_at = Some(fs_utils::now_iso());
        updated = true;
    } else {
        accounts.accounts.push(RawAccount {
            name: name.clone(),
            user_id: Some(exchange.user_id.clone()),
            jwt: exchange.jwt.clone(),
            refresh_token: Some(exchange.refresh_token.clone()),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
            dc_id: None,
            credential_source: Some("native_oauth".into()),
        });
        if let Some(group) = group_id {
            let mut groups: crate::models::GroupsFile =
                fs_utils::read_json(&state.path("groups.json"));
            groups.membership.insert(exchange.user_id.clone(), group);
            fs_utils::write_json(&state.path("groups.json"), &groups)?;
        }
    }
    crate::vault::save_accounts(state, &mut accounts)?;
    // OAuth 接管拿到 refresh_token 时也立即生成原生 TRAE Work CN 快照，
    // 使“BitBrowser 登录 → 账号入池 → 原生切换”不再需要手动登录/保存快照。
    if let Err(error) = crate::native_snapshot::ensure_native_snapshot(
        state,
        &exchange.user_id,
        &exchange.jwt,
        Some(&exchange.refresh_token),
        &name,
    ) {
        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 账号原生快照生成失败: uid={} error={error}", exchange.user_id),
        );
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!(
            "BitBrowser OAuth 原生凭据{}: uid={}",
            if updated { "更新" } else { "接管" },
            exchange.user_id
        ),
    );
    Ok(OAuthLoginResult {
        user_id: exchange.user_id,
        name,
        jwt: exchange.jwt,
        refresh_token: exchange.refresh_token,
        has_refresh_token: true,
        token_exp_timestamp: exchange.token_exp_timestamp,
        refresh_exp_timestamp: exchange.refresh_exp_timestamp,
        credential_source: Some("native_oauth".into()),
    })
}

/// 在已登录的 BitBrowser profile 中做一次短时原生 OAuth 握手，获取 refresh_token。
///
/// 该函数只用于首次 BitBrowser 导入的“升级”步骤；正常续期不再调用 BitBrowser，
/// 而是走本机 refresh_token ExchangeToken。已登录页面通常会自动回调，若页面需要
/// 人工确认则在短超时后返回错误，调用方仍可保留已捕获的短期 JWT。
pub(crate) fn native_oauth_exchange_for_bitbrowser(
    state: &AppState,
    profile_id: &str,
    timeout: Duration,
) -> Result<NativeTokenExchange, String> {
    let base = crate::commands::bitbrowser::local_api_base(state)?;
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("创建 OAuth 回调监听失败：{error}"))?;
    let callback_port = listener
        .local_addr()
        .map_err(|error| format!("读取 OAuth 回调端口失败：{error}"))?
        .port();
    let request = build_native_oauth_request(callback_port);
    let cloudide_token = crate::commands::bitbrowser::capture_profile_token_by_id(&base, profile_id);
    crate::commands::bitbrowser::navigate_profile_url(&base, profile_id, &request.url)?;
    let callback = wait_native_callback_timeout(
        listener,
        request.callback_port,
        &request.state,
        &request.trace_id,
        timeout,
    )?;
    exchange_native_auth_code(&callback, &request, cloudide_token)
}

/// 通过 BitBrowser 完成 Trae 原生 OAuth 登录，并自动接管长效凭据。
///
/// BitBrowser 只负责展示授权页；回调、AuthCode 交换和凭据持久化均在本机完成。
/// 不读取密码，不把 Token 返回给前端，也不修改 BitBrowser profile 文件。
#[tauri::command(async)]
pub fn oauth_login_bitbrowser(
    state: State<AppState>,
    window_id: String,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    let profile_id = window_id.trim();
    if profile_id.is_empty()
        || profile_id.len() > 200
        || !profile_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err("无效的 BitBrowser profile id".into());
    }
    fs_utils::app_log(
        &state.data_dir,
        &format!("BitBrowser OAuth 原生登录开始: profile={profile_id}"),
    );
    let exchange = native_oauth_exchange_for_bitbrowser(&state, profile_id, Duration::from_secs(300))?;
    let result = persist_native_oauth_account(&state, exchange, account_name, group_id)?;
    fs_utils::app_log(
        &state.data_dir,
        &format!("BitBrowser OAuth 原生登录完成: uid={}", result.user_id),
    );
    Ok(result)
}

/// 生成 OAuth 登录 URL
#[tauri::command]
pub fn oauth_get_login_url() -> OAuthLoginUrl {
    // 普通浏览器复用与原生 Trae 相同的 PKCE 握手，避免回调只有 code
    // 而旧逻辑要求 refreshToken 导致登录链路中断。
    let request = build_native_oauth_request(17388);
    let state = request.state.clone();
    *LAST_NATIVE_REQUEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(request.clone());
    let auto_callback = start_oauth_callback_listener();

    OAuthLoginUrl {
        url: request.url,
        // 记录最近签发的 state 供回调校验（CSRF）
        state: {
            *LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner()) = Some(state.clone());
            state
        },
        redirect_uri: OAUTH_REDIRECT_URI.to_string(),
        auto_callback,
    }
}

/// 取出普通浏览器 OAuth 回环监听器收到的一次性回调 URL。
#[tauri::command]
pub fn oauth_poll_callback() -> Option<String> {
    LAST_OAUTH_CALLBACK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

/// 解析 OAuth 回调 URL
#[tauri::command]
pub fn oauth_parse_callback(callback_url: String) -> Result<OAuthCallbackInfo, String> {
    // 支持旧版直接携带 refreshToken，以及原生 PKCE 携带 authCodeInfo。
    let params = parse_query_params(&callback_url);
    if params.is_empty() {
        return Err("回调 URL 中缺少查询参数".into());
    }
    let refresh_token = params
        .get("refreshToken")
        .or_else(|| params.get("refresh_token"))
        .cloned()
        .unwrap_or_default();

    let auth_code = params.get("authCodeInfo").and_then(|raw| {
        let value = serde_json::from_str::<Value>(raw).ok()?;
        json_string(value.get("AuthCode")).or_else(|| json_string(value.get("auth_code")))
    });
    if refresh_token.is_empty() && auth_code.is_none() {
        return Err("回调 URL 中缺少 refreshToken 或 authCodeInfo 参数".into());
    }

    let access_token = params
        .get("accessToken")
        .or_else(|| params.get("access_token"))
        .cloned();

    let user_id = params
        .get("userId")
        .or_else(|| params.get("user_id"))
        .or_else(|| params.get("UserID"))
        .cloned();

    let user_name = params
        .get("userName")
        .or_else(|| params.get("user_name"))
        .or_else(|| params.get("nickname"))
        .cloned();

    let user_name = user_name.or_else(|| {
        params.get("userInfo").and_then(|raw| {
            let value = serde_json::from_str::<Value>(raw).ok()?;
            ["ScreenName", "UserName", "nickname", "name"]
                .iter()
                .find_map(|key| json_string(value.get(*key)))
        })
    });

    let avatar = params.get("avatar").cloned();

    // CSRF 校验（审查 P2）：回调携带 state 且本进程签发过 state 时，两者必须一致；
    // 不一致的回调 URL 可能来自伪造/重放，直接拒绝。回调不带 state（旧流程/第三方拼 URL）
    // 或本进程从未签发过（如重启后直接粘贴回调）时保持宽容，不阻断正常登录
    if let Some(cb_state) = params.get("state") {
        let issued = LAST_OAUTH_STATE.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(expected) = issued {
            if !expected.is_empty() && cb_state != &expected {
                return Err("OAuth state 校验失败：回调 URL 与本机发起的登录请求不匹配（可能为伪造或重放），已拒绝".into());
            }
        }
    }

    Ok(OAuthCallbackInfo {
        refresh_token,
        access_token,
        user_id,
        user_name,
        avatar,
        auth_code,
        state: params.get("state").cloned(),
        login_trace_id: params.get("loginTraceID").cloned(),
    })
}

/// ExchangeToken：用 refresh_token 换取 access_token
fn exchange_token(refresh_token: &str) -> Result<(String, Option<String>), String> {
    let resp = short_agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken")
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({
            "ClientID": OAUTH_CLIENT_ID,
            "RefreshToken": refresh_token,
            "ClientSecret": OAUTH_CLIENT_SECRET,
            "UserID": ""
        }))
        .map_err(|e| format!("ExchangeToken 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let code = body
        .get("code")
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
        })
        .or_else(|| {
            body.get("data")
                .and_then(|data| data.get("code"))
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
                })
        })
        .or_else(|| {
            body.get("Result")
                .and_then(|result| result.get("code").or_else(|| result.get("Code")))
                .and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|text| text.parse::<i64>().ok()))
                })
        })
        .unwrap_or_else(|| {
            crate::fs_utils::dig(
                &body,
                &[
                    "access_token",
                    "token",
                    "accessToken",
                    "AccessToken",
                    "UserJwt",
                    "Token",
                ],
            )
                .and_then(|value| value.as_str())
                .map(|_| 0)
                .unwrap_or(-1)
        });
    if code != 0 {
        let msg = body
            .get("message")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("data").and_then(|data| data.get("message")).and_then(|v| v.as_str()))
            .unwrap_or("未知错误");
        return Err(format!("ExchangeToken 失败 (code={}): {}", code, msg));
    }

    let access_token = crate::fs_utils::dig(
        &body,
        &[
            "access_token",
            "token",
            "accessToken",
            "AccessToken",
            "UserJwt",
            "Token",
        ],
    )
    .and_then(|v| v.as_str())
    .ok_or("响应中缺少 access_token/UserJwt")?;

    let new_refresh_token = crate::fs_utils::dig(&body, &["refresh_token", "refreshToken", "RefreshToken"])
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok((access_token.to_string(), new_refresh_token))
}

/// GetUserInfo：获取用户信息
fn get_user_info(access_token: &str) -> Result<(String, String), String> {
    let auth = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.to_string()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    let resp = short_agent()
        .post("https://api.trae.com.cn/cloudide/api/v3/trae/GetUserInfo")
        .set("authorization", &auth)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .send_json(ureq::json!({}))
        .map_err(|e| format!("GetUserInfo 请求失败: {}", e))?;

    let body: serde_json::Value =
        resp.into_json().map_err(|e| format!("解析响应失败: {}", e))?;

    let data = body.get("data").or(body.get("result")).ok_or("响应中缺少 data 字段")?;

    let user_id = data
        .get("user_id")
        .or_else(|| data.get("UserID"))
        .or_else(|| data.get("userId"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let user_name = data
        .get("name")
        .or_else(|| data.get("user_name"))
        .or_else(|| data.get("userName"))
        .or_else(|| data.get("nickname"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    Ok((user_id, user_name))
}

/// OAuth 登录闭环：解析回调 → 换取 accessToken → 获取用户信息 → 保存账号
// async：内含最多两次 120s 超时的串行网络请求（exchange_token/get_user_info），同步命令会冻结 UI（审查修复）
#[tauri::command(async)]
pub fn oauth_login(
    state: State<AppState>,
    callback_url: String,
    account_name: Option<String>,
    group_id: Option<String>,
) -> Result<OAuthLoginResult, String> {
    // 1. 解析回调 URL
    let callback_info = oauth_parse_callback(callback_url.clone())?;

    // 新版本机回环回调携带 authCodeInfo：用同一份 PKCE 请求完成原生交换，
    // 这样正常浏览器登录与 BitBrowser 首次接管使用完全一致的凭据格式。
    if callback_info.auth_code.is_some() {
        let request = LAST_NATIVE_REQUEST
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .ok_or_else(|| "OAuth 原生请求已过期，请重新点击打开登录页".to_string())?;
        let callback = parse_native_callback(&callback_url, &request.state, &request.trace_id)?;
        let exchange = exchange_native_auth_code(&callback, &request, None)?;
        return persist_native_oauth_account(&state, exchange, account_name, group_id);
    }

    // 2. 如果回调中没有 accessToken，则用 refresh_token 换取
    let (access_token, new_refresh_token) = if let Some(ref at) = callback_info.access_token {
        (at.clone(), None)
    } else {
        exchange_token(&callback_info.refresh_token)?
    };

    // 3. 规范化 JWT 格式
    let jwt = if access_token.starts_with("Cloud-IDE-JWT ") {
        access_token.clone()
    } else {
        format!("Cloud-IDE-JWT {}", access_token)
    };

    // 4. 解析 JWT 获取 user_id
    let jwt_info = jwt::parse(&jwt);
    let user_id = callback_info
        .user_id
        .clone()
        .or_else(|| jwt_info.user_id.clone())
        .ok_or_else(|| "无法从回调或 JWT 中获取 user_id".to_string())?;

    // 5. 尝试获取用户名
    let name = account_name
        .or(callback_info.user_name.clone())
        .or_else(|| {
            // 尝试调用 GetUserInfo
            get_user_info(&jwt)
                .map(|(uid, uname)| if uname.is_empty() { uid } else { uname })
                .ok()
        })
        .unwrap_or_else(|| {
            // 按字符截取（字节切片在多字节 UTF-8 边界处会 panic）
            let head: String = user_id.chars().take(8).collect();
            format!("账号_{head}")
        });

    // 6. 确定最终的 refresh_token（优先使用 ExchangeToken 返回的新 token）
    let final_refresh_token = new_refresh_token
        .unwrap_or_else(|| callback_info.refresh_token.clone());

    // 7. 检查账号是否已存在
    let mut accounts = crate::vault::load_accounts(&state);
    if accounts
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(&user_id))
    {
        // 已存在：更新 JWT 和 refresh_token
        let acct = accounts
            .accounts
            .iter_mut()
            .find(|a| a.user_id.as_deref() == Some(&user_id))
            .unwrap();
        acct.jwt = jwt.clone();
        acct.refresh_token = Some(final_refresh_token.clone());
        acct.updated_at = Some(fs_utils::now_iso());
        crate::vault::save_accounts(&state, &mut accounts)?;

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：更新已有账号 [{}] jwt + refresh_token", name),
        );
    } else {
        // 新账号
        accounts.accounts.push(RawAccount {
            name: name.clone(),
            user_id: Some(user_id.clone()),
            jwt: jwt.clone(),
            refresh_token: Some(final_refresh_token.clone()),
            added_at: Some(fs_utils::now_iso()),
            updated_at: Some(fs_utils::now_iso()),
            dc_id: None,
            credential_source: None,
        });
        crate::vault::save_accounts(&state, &mut accounts)?;

        // 设置分组
        if let Some(g) = group_id {
            let mut groups: crate::models::GroupsFile =
                fs_utils::read_json(&state.path("groups.json"));
            groups.membership.insert(user_id.clone(), g);
            fs_utils::write_json(&state.path("groups.json"), &groups)?;
        }

        fs_utils::app_log(
            &state.data_dir,
            &format!("OAuth 登录：新增账号 [{}] user_id={}", name, user_id),
        );
    }

    let token_exp_timestamp = jwt::parse(&jwt).exp_timestamp;
    Ok(OAuthLoginResult {
        user_id: user_id.clone(),
        name,
        jwt,
        refresh_token: final_refresh_token,
        has_refresh_token: true,
        token_exp_timestamp,
        refresh_exp_timestamp: None,
        credential_source: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_callback_decodes_auth_code_and_user_name() {
        let info = urlencoding::encode(
            r#"{"AuthCode":"abc123","ExpireAt":1770000000000,"ExpireDuration":600000}"#,
        );
        let user = urlencoding::encode(r#"{"ScreenName":"测试账号"}"#);
        let target = format!(
            "/authorize?authCodeInfo={info}&loginTraceID=trace-1&userInfo={user}&state=state-1"
        );
        let callback = parse_native_callback(&target, "state-1", "trace-1").unwrap();
        assert_eq!(callback.auth_code, "abc123");
        assert_eq!(callback.user_name.as_deref(), Some("测试账号"));
    }

    #[test]
    fn native_callback_rejects_mismatched_state() {
        let info = urlencoding::encode(r#"{"AuthCode":"abc123"}"#);
        let target = format!("/authorize?authCodeInfo={info}&state=other");
        let error = parse_native_callback(&target, "expected", "trace").unwrap_err();
        assert!(error.contains("state"));
    }

    #[test]
    fn native_oauth_request_has_pkce_and_loopback_callback() {
        let request = build_native_oauth_request(45678);
        assert!(request.url.starts_with("https://www.trae.cn/authorization?"));
        assert!(request.url.contains("code_challenge_method=S256"));
        assert!(request.url.contains("auth_callback_url=http%3A%2F%2F127.0.0.1%3A45678%2Fauthorize"));
        assert!(request.verifier.len() >= 43);
        assert_eq!(request.callback_port, 45678);
    }
}
