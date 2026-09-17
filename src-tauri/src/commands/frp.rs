//! FRP 应用层隧道。
//!
//! frpc 运行在 AI Work 主机上，主动连接局域网中转机或腾讯云上的 frps，
//! 将本机 API 网关转发到远端。这里只负责管理本地 frpc 进程；frps/Nginx
//! 仍由部署端独立维护，避免把网络环境和服务器地址写死在应用里。

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;
use tauri::State;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use crate::fs_utils;
use crate::state::AppState;

static CHILD: OnceLock<Mutex<Option<Child>>> = OnceLock::new();
static MONITOR_STOP: OnceLock<Mutex<Option<Arc<AtomicBool>>>> = OnceLock::new();
static MONITOR: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();

fn child_slot() -> &'static Mutex<Option<Child>> {
    CHILD.get_or_init(|| Mutex::new(None))
}

fn monitor_stop_slot() -> &'static Mutex<Option<Arc<AtomicBool>>> {
    MONITOR_STOP.get_or_init(|| Mutex::new(None))
}

fn monitor_slot() -> &'static Mutex<Option<JoinHandle<()>>> {
    MONITOR.get_or_init(|| Mutex::new(None))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrpConfig {
    /// frpc 可执行文件路径；为空时从 PATH 查找 `frpc`。
    #[serde(default)]
    pub binary_path: String,
    /// frps 地址，可以是局域网中转机或公网服务器。
    #[serde(default)]
    pub server_addr: String,
    #[serde(default = "default_server_port")]
    pub server_port: u16,
    #[serde(default = "default_proxy_name")]
    pub proxy_name: String,
    #[serde(default = "default_local_host")]
    pub local_host: String,
    #[serde(default = "default_local_port")]
    pub local_port: u16,
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    /// 仅保存到应用私有 data 目录，不写入日志；FRP token 认证必须启用。
    #[serde(default)]
    pub auth_token: String,
    #[serde(default = "default_true")]
    pub tls_enable: bool,
    #[serde(default)]
    pub auto_reconnect: bool,
}

fn default_server_port() -> u16 { 7000 }
fn default_proxy_name() -> String { "aiwork-gateway".into() }
fn default_local_host() -> String { "127.0.0.1".into() }
fn default_local_port() -> u16 { 7864 }
fn default_remote_port() -> u16 { 17864 }
fn default_true() -> bool { true }

impl Default for FrpConfig {
    fn default() -> Self {
        Self {
            binary_path: String::new(),
            server_addr: String::new(),
            server_port: default_server_port(),
            proxy_name: default_proxy_name(),
            local_host: default_local_host(),
            local_port: default_local_port(),
            remote_port: default_remote_port(),
            auth_token: String::new(),
            tls_enable: true,
            auto_reconnect: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct FrpStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub config: FrpConfig,
    pub config_path: String,
    pub message: String,
}

fn config_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("frp").join("frpc.toml.json")
}

fn runtime_config_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("frp").join("frpc.toml")
}

fn normalize(mut config: FrpConfig) -> Result<FrpConfig, String> {
    config.binary_path = config.binary_path.trim().to_string();
    config.server_addr = config.server_addr.trim().to_string();
    config.proxy_name = config.proxy_name.trim().to_string();
    config.local_host = config.local_host.trim().to_string();
    config.auth_token = config.auth_token.trim().to_string();
    if config.server_addr.is_empty() {
        return Err("FRP 需要填写 frps 地址（局域网中转机或服务器地址）".into());
    }
    if config.auth_token.is_empty() {
        return Err("FRP 必须填写 token，不能启用匿名隧道".into());
    }
    if config.proxy_name.is_empty()
        || config.proxy_name.len() > 80
        || !config.proxy_name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err("FRP proxy 名称只能包含字母、数字、点、短横线和下划线".into());
    }
    if config.server_port == 0 || config.local_port == 0 || config.remote_port == 0 {
        return Err("FRP 控制端口、本地端口、远程端口必须为 1-65535".into());
    }
    if config.local_host.is_empty() {
        config.local_host = default_local_host();
    }
    if !config.binary_path.is_empty() && !Path::new(&config.binary_path).is_file() {
        return Err("frpc 路径不存在或不是文件；留空可从 PATH 查找 frpc".into());
    }
    Ok(config)
}

fn toml_quote(value: &str) -> String {
    // 配置值只允许作为字符串写入 TOML；控制字符和引号必须转义。
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

fn render_frpc(config: &FrpConfig) -> String {
    let tls = if config.tls_enable { "true" } else { "false" };
    format!(
        "# AI Work Assistant generated frpc config. Do not commit this file.\n\
serverAddr = {server_addr}\n\
serverPort = {server_port}\n\
auth.method = \"token\"\n\
auth.token = {token}\n\
transport.tls.enable = {tls}\n\
\n[[proxies]]\n\
name = {name}\n\
type = \"tcp\"\n\
localIP = {local_host}\n\
localPort = {local_port}\n\
remotePort = {remote_port}\n",
        server_addr = toml_quote(&config.server_addr),
        server_port = config.server_port,
        token = toml_quote(&config.auth_token),
        tls = tls,
        name = toml_quote(&config.proxy_name),
        local_host = toml_quote(&config.local_host),
        local_port = config.local_port,
        remote_port = config.remote_port,
    )
}

fn write_runtime_config(state: &AppState, config: &FrpConfig) -> Result<PathBuf, String> {
    let path = runtime_config_path(state);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 FRP 配置目录失败：{e}"))?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, render_frpc(config)).map_err(|e| format!("写入 FRP 配置失败：{e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("替换 FRP 配置失败：{e}"))?;
    Ok(path)
}

fn spawn_frpc(binary: &str, runtime_path: &Path) -> Result<Child, String> {
    let executable = if binary.trim().is_empty() { "frpc" } else { binary.trim() };
    let mut command = Command::new(executable);
    command
        .arg("-c")
        .arg(runtime_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command.spawn().map_err(|error| format!("启动 frpc 失败：{error}；请检查 frpc 路径或 PATH"))
}

fn load_config(state: &AppState) -> FrpConfig {
    fs_utils::read_json(&config_path(state))
}

#[tauri::command]
pub fn gateway_frp_get(state: State<'_, AppState>) -> FrpConfig {
    load_config(&state)
}

#[tauri::command]
pub fn gateway_frp_save(state: State<'_, AppState>, config: FrpConfig) -> Result<FrpConfig, String> {
    let normalized = normalize(config)?;
    if let Some(parent) = config_path(&state).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 FRP 数据目录失败：{e}"))?;
    }
    fs_utils::write_json(&config_path(&state), &normalized)?;
    Ok(normalized)
}

#[tauri::command]
pub fn gateway_frp_status(state: State<'_, AppState>) -> FrpStatus {
    let config = load_config(&state);
    let cfg_path = runtime_config_path(&state);
    let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = slot.as_mut() {
        match child.try_wait() {
            Ok(None) => FrpStatus {
                running: true,
                pid: Some(child.id()),
                config,
                config_path: cfg_path.display().to_string(),
                message: "frpc 运行中".into(),
            },
            Ok(Some(status)) => {
                *slot = None;
                FrpStatus { running: false, pid: None, config, config_path: cfg_path.display().to_string(), message: format!("frpc 已退出（{status}）") }
            }
            Err(error) => FrpStatus { running: false, pid: None, config, config_path: cfg_path.display().to_string(), message: format!("读取 frpc 状态失败：{error}") },
        }
    } else {
        FrpStatus { running: false, pid: None, config, config_path: cfg_path.display().to_string(), message: "frpc 未启动".into() }
    }
}

#[tauri::command]
pub fn gateway_frp_start(state: State<'_, AppState>, config: FrpConfig) -> Result<FrpStatus, String> {
    let config = normalize(config)?;
    let _ = gateway_frp_stop();
    {
        let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return Err("frpc 已在运行".into());
            }
            *slot = None;
        }
    }
    if let Some(parent) = config_path(&state).parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("创建 FRP 数据目录失败：{e}"))?;
    }
    fs_utils::write_json(&config_path(&state), &config)?;
    let runtime_path = write_runtime_config(&state, &config)?;
    let child = spawn_frpc(&config.binary_path, &runtime_path)?;
    let pid = child.id();
    *child_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(child);

    let stop = Arc::new(AtomicBool::new(false));
    *monitor_stop_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());
    if config.auto_reconnect {
        let cfg = config.clone();
        let path = runtime_path.clone();
        let handle = std::thread::spawn(move || {
            let mut delay = Duration::from_secs(2);
            loop {
                if stop.load(Ordering::Relaxed) { break; }
                let exited = {
                    let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
                    match slot.as_mut() {
                        Some(child) => match child.try_wait() {
                            Ok(None) => false,
                            Ok(Some(_)) | Err(_) => { *slot = None; true }
                        },
                        None => true,
                    }
                };
                if exited && !stop.load(Ordering::Relaxed) {
                    std::thread::sleep(delay);
                    if stop.load(Ordering::Relaxed) { break; }
                    match spawn_frpc(&cfg.binary_path, &path) {
                        Ok(child) => {
                            *child_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
                            delay = Duration::from_secs(2);
                        }
                        Err(_) => {
                            delay = (delay * 2).min(Duration::from_secs(60));
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        });
        *monitor_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
    }

    Ok(FrpStatus {
        running: true,
        pid: Some(pid),
        config,
        config_path: runtime_path.display().to_string(),
        message: "frpc 已启动；请从中转机或其他网段检查 Nginx/health".into(),
    })
}

#[tauri::command]
pub fn gateway_frp_stop() -> Result<(), String> {
    if let Some(stop) = monitor_stop_slot().lock().unwrap_or_else(|e| e.into_inner()).take() {
        stop.store(true, Ordering::Relaxed);
    }
    let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(mut child) = slot.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    drop(slot);
    if let Some(handle) = monitor_slot().lock().unwrap_or_else(|e| e.into_inner()).take() {
        let _ = handle.join();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_contains_no_unescaped_token_delimiters() {
        let mut c = FrpConfig::default();
        c.server_addr = "192.168.0.10".into();
        c.auth_token = "a\"b\\c".into();
        let out = render_frpc(&c);
        assert!(out.contains("serverAddr = \"192.168.0.10\""));
        assert!(out.contains("auth.token = \"a\\\"b\\\\c\""));
        assert!(out.contains("localPort = 7864"));
    }

    #[test]
    fn validation_rejects_anonymous_and_bad_proxy_name() {
        let mut c = FrpConfig::default();
        c.server_addr = "127.0.0.1".into();
        assert!(normalize(c.clone()).is_err());
        c.auth_token = "token".into();
        c.proxy_name = "bad/name".into();
        assert!(normalize(c).is_err());
    }
}
