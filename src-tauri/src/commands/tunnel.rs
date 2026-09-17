//! 腾讯云/任意 SSH 服务器反向隧道。
//!
//! 这是显式启动的本机能力：配置只保存连接参数和 PEM 路径，私钥不读取、不复制、
//! 不上传。OpenSSH 使用 `-R` 将远端端口转发到本机 API 网关。

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
pub struct TunnelConfig {
    pub host: String,
    pub username: String,
    pub key_path: String,
    #[serde(default = "default_remote_port")]
    pub remote_port: u16,
    #[serde(default = "default_ssh_port")]
    pub ssh_port: u16,
    #[serde(default = "default_local_host")]
    pub local_host: String,
    #[serde(default = "default_local_port")]
    pub local_port: u16,
    #[serde(default = "default_remote_bind")]
    pub remote_bind: String,
    /// 断线后自动重连；默认关闭，避免用户未确认主机指纹时后台反复连接。
    #[serde(default)]
    pub auto_reconnect: bool,
    /// 可选 SHA256 主机指纹（例如 SHA256:...）；填写后启动前会用 ssh-keyscan 核对。
    #[serde(default)]
    pub host_key_fingerprint: String,
}

fn default_remote_port() -> u16 { 7864 }
fn default_ssh_port() -> u16 { 22 }
fn default_local_host() -> String { "127.0.0.1".into() }
fn default_local_port() -> u16 { 7864 }
fn default_remote_bind() -> String { "127.0.0.1".into() }

impl Default for TunnelConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            username: String::new(),
            key_path: String::new(),
            remote_port: default_remote_port(),
            ssh_port: default_ssh_port(),
            local_host: default_local_host(),
            local_port: default_local_port(),
            remote_bind: default_remote_bind(),
            auto_reconnect: false,
            host_key_fingerprint: String::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelStatus {
    pub running: bool,
    pub pid: Option<u32>,
    pub config: TunnelConfig,
    pub message: String,
}

fn config_path(state: &AppState) -> PathBuf {
    state.data_dir.join("data").join("api_gateway_tunnel.json")
}

fn normalize(mut config: TunnelConfig) -> Result<TunnelConfig, String> {
    config.host = config.host.trim().to_string();
    config.username = config.username.trim().to_string();
    config.key_path = config.key_path.trim().to_string();
    config.local_host = config.local_host.trim().to_string();
    config.remote_bind = config.remote_bind.trim().to_string();
    config.host_key_fingerprint = config.host_key_fingerprint.trim().to_string();
    if config.host.is_empty() || config.username.is_empty() || config.key_path.is_empty() {
        return Err("SSH 隧道需要填写服务器地址、用户名和 PEM 路径".into());
    }
    if config.remote_port == 0 || config.local_port == 0 || config.ssh_port == 0 {
        return Err("SSH、远端、本地端口必须为 1-65535".into());
    }
    if config.local_host.is_empty() { config.local_host = default_local_host(); }
    if config.remote_bind.is_empty() { config.remote_bind = default_remote_bind(); }
    let key = PathBuf::from(&config.key_path);
    if !key.is_file() {
        return Err("PEM 路径不存在或不是文件".into());
    }
    Ok(config)
}

fn spawn_ssh(config: &TunnelConfig) -> Result<Child, String> {
    let target = format!("{}@{}", config.username, config.host);
    let remote = format!("{}:{}:{}:{}", config.remote_bind, config.remote_port, config.local_host, config.local_port);
    let mut command = Command::new("ssh");
    command
        .arg("-N")
        .arg("-T")
        .arg("-o").arg("BatchMode=yes")
        .arg("-o").arg("ExitOnForwardFailure=yes")
        .arg("-o").arg("StrictHostKeyChecking=yes")
        .arg("-o").arg("ServerAliveInterval=30")
        .arg("-o").arg("ServerAliveCountMax=3")
        .arg("-p").arg(config.ssh_port.to_string())
        .arg("-i").arg(&config.key_path)
        .arg("-R").arg(remote)
        .arg(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command.spawn().map_err(|error| format!("启动 OpenSSH 失败：{error}"))
}

fn verify_fingerprint(config: &TunnelConfig) -> Result<(), String> {
    let expected = config.host_key_fingerprint.trim();
    if expected.is_empty() {
        return Ok(());
    }
    let port = config.ssh_port.to_string();
    let scan = Command::new("ssh-keyscan")
        .args(["-T", "5", "-p", &port, &config.host])
        .output()
        .map_err(|error| format!("无法运行 ssh-keyscan：{error}"))?;
    if !scan.status.success() || scan.stdout.is_empty() {
        return Err("无法读取 SSH 主机公钥，未建立隧道；请确认地址/端口并手动核对指纹".into());
    }
    let keygen = Command::new("ssh-keygen")
        .args(["-lf", "-", "-E", "sha256"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| format!("无法运行 ssh-keygen：{error}"))?;
    let mut keygen = keygen;
    if let Some(mut stdin) = keygen.stdin.take() {
        use std::io::Write;
        stdin.write_all(&scan.stdout).map_err(|error| format!("写入主机公钥失败：{error}"))?;
    }
    let output = keygen.wait_with_output().map_err(|error| format!("读取主机指纹失败：{error}"))?;
    let fingerprints = String::from_utf8_lossy(&output.stdout);
    if fingerprints.lines().any(|line| line.contains(expected)) {
        Ok(())
    } else {
        Err("SSH 主机指纹不匹配，已拒绝连接；请核对 host_key_fingerprint 或服务器 host key".into())
    }
}

fn load_config(state: &AppState) -> TunnelConfig {
    fs_utils::read_json(&config_path(state))
}

#[tauri::command]
pub fn gateway_tunnel_get(state: State<'_, AppState>) -> TunnelConfig {
    load_config(&state)
}

#[tauri::command]
pub fn gateway_tunnel_save(state: State<'_, AppState>, config: TunnelConfig) -> Result<TunnelConfig, String> {
    let normalized = normalize(config)?;
    fs_utils::write_json(&config_path(&state), &normalized)?;
    Ok(normalized)
}

#[tauri::command]
pub fn gateway_tunnel_status(state: State<'_, AppState>) -> TunnelStatus {
    let config = load_config(&state);
    let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(child) = slot.as_mut() {
        match child.try_wait() {
            Ok(None) => TunnelStatus {
                running: true,
                pid: Some(child.id()),
                config,
                message: "SSH 反向隧道运行中".into(),
            },
            Ok(Some(status)) => {
                *slot = None;
                TunnelStatus { running: false, pid: None, config, message: format!("SSH 进程已退出（{status}）") }
            }
            Err(error) => TunnelStatus { running: false, pid: None, config, message: format!("读取 SSH 状态失败：{error}") },
        }
    } else {
        TunnelStatus { running: false, pid: None, config, message: "SSH 反向隧道未启动".into() }
    }
}

#[tauri::command]
pub fn gateway_tunnel_start(state: State<'_, AppState>, config: TunnelConfig) -> Result<TunnelStatus, String> {
    let config = normalize(config)?;
    verify_fingerprint(&config)?;
    // 清理上一次监控线程/子进程，避免重复启动形成多个反向转发。
    let _ = gateway_tunnel_stop();
    {
        let mut slot = child_slot().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(child) = slot.as_mut() {
            if child.try_wait().ok().flatten().is_none() {
                return Err("SSH 反向隧道已经在运行".into());
            }
            *slot = None;
        }
    }
    fs_utils::write_json(&config_path(&state), &config)?;
    let child = spawn_ssh(&config)?;
    let pid = child.id();
    *child_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    let stop = Arc::new(AtomicBool::new(false));
    *monitor_stop_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(stop.clone());
    if config.auto_reconnect {
        let cfg = config.clone();
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
                    match spawn_ssh(&cfg) {
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
    Ok(TunnelStatus { running: true, pid: Some(pid), config, message: "SSH 反向隧道已启动".into() })
}

#[tauri::command]
pub fn gateway_tunnel_stop() -> Result<(), String> {
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
