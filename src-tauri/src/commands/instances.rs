//! Trae Work CN 多实例隔离（F-67）。
//!
//! 每个实例使用独立的 `--user-data-dir`，从而允许用户同时打开多个账号。
//! 默认只创建空目录；只有用户显式勾选 `seed_snapshot` 时才从本机已有账号快照
//! 复制登录态。不会读取、回显或上传凭证，也不会把一个账号的云端会话伪装到另一个 UID。

use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tauri::State;

use crate::fs_utils;
use crate::state::AppState;

const REGISTRY_FILE: &str = "trae_instances.json";
const INSTANCES_DIR: &str = "trae_instances";
const CREATE_NO_WINDOW: u32 = 0x08000000;
const TRAE_WORK_IMAGES: &[&str] = &["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraeInstance {
    pub id: String,
    pub name: String,
    pub account_uid: String,
    pub data_dir: String,
    pub target_app: String,
    pub created_at: i64,
    #[serde(default)]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraeInstanceView {
    pub id: String,
    pub name: String,
    pub account_uid: String,
    pub data_dir: String,
    pub target_app: String,
    pub created_at: i64,
    pub pid: Option<u32>,
    pub running: bool,
    pub data_dir_exists: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct InstanceFile {
    #[serde(default)]
    instances: Vec<TraeInstance>,
}

fn registry_path(state: &AppState) -> PathBuf {
    state.data_path(REGISTRY_FILE)
}

fn instances_root(state: &AppState) -> PathBuf {
    state.data_path(INSTANCES_DIR)
}

fn load_registry(state: &AppState) -> InstanceFile {
    fs_utils::read_json(&registry_path(state))
}

fn save_registry(state: &AppState, file: &InstanceFile) -> Result<(), String> {
    fs_utils::write_json(&registry_path(state), file)
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn validate_instance_id(id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty()
        || id.len() > 64
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("实例 ID 只能包含字母、数字、连字符和下划线（1~64 位）".into());
    }
    Ok(())
}

fn validate_port(port: Option<u16>) -> Result<(), String> {
    if let Some(p) = port {
        if p == 0 {
            return Err("代理端口必须在 1~65535 之间".into());
        }
    }
    Ok(())
}

fn process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags(CREATE_NO_WINDOW)
        .output();
    matches!(out, Ok(o) if tasklist_matches_process(&String::from_utf8_lossy(&o.stdout), pid, TRAE_WORK_IMAGES))
}

fn tasklist_matches_process(stdout: &str, pid: u32, image_names: &[&str]) -> bool {
    let pid_token = format!("\"{pid}\"");
    stdout.lines().any(|line| {
        line.contains(&pid_token)
            && image_names.iter().any(|name| line.starts_with(&format!("\"{name}\"")))
    })
}

fn account_exists(state: &AppState, uid: &str) -> bool {
    crate::vault::load_accounts(state)
        .accounts
        .iter()
        .any(|a| a.user_id.as_deref() == Some(uid))
}

fn instance_view(item: &TraeInstance) -> TraeInstanceView {
    let data_dir = PathBuf::from(&item.data_dir);
    let running = item.pid.map(process_running).unwrap_or(false);
    TraeInstanceView {
        id: item.id.clone(),
        name: item.name.clone(),
        account_uid: item.account_uid.clone(),
        data_dir: item.data_dir.clone(),
        target_app: item.target_app.clone(),
        created_at: item.created_at,
        pid: item.pid,
        running,
        data_dir_exists: data_dir.is_dir(),
    }
}

fn safe_instance_dir(state: &AppState, id: &str) -> PathBuf {
    instances_root(state).join(id)
}

/// 列出已登记实例；不会返回任何凭证内容。
#[tauri::command]
pub fn trae_instances_list(state: State<AppState>) -> Vec<TraeInstanceView> {
    load_registry(&state)
        .instances
        .iter()
        .map(instance_view)
        .collect()
}

/// 创建独立实例目录。默认空目录，用户显式选择后才复制对应 Trae Work 快照。
#[tauri::command]
pub fn trae_instance_create(
    state: State<AppState>,
    instance_id: String,
    name: Option<String>,
    account_uid: String,
    seed_snapshot: bool,
) -> Result<TraeInstanceView, String> {
    let id = instance_id.trim().to_string();
    validate_instance_id(&id)?;
    let uid = account_uid.trim().to_string();
    fs_utils::ensure_uid_safe(&uid)?;
    if !account_exists(&state, &uid) {
        return Err("账号不在账号库中，请先导入或添加账号".into());
    }
    let mut file = load_registry(&state);
    if file.instances.iter().any(|i| i.id == id) {
        return Err(format!("实例 ID 已存在：{id}"));
    }
    let dir = safe_instance_dir(&state, &id);
    if dir.exists() {
        return Err("实例目录已存在但未登记，为避免覆盖数据请先人工处理该目录".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建实例目录失败：{e}"))?;

    if seed_snapshot {
        let snapshot = state.data_path("profiles").join(&uid);
        if !snapshot.is_dir() {
            let _ = std::fs::remove_dir_all(&dir);
            return Err("所选账号没有 Trae Work 快照，请先保存当前登录态，或取消“使用已有快照”".into());
        }
        if let Err(e) = crate::state::copy_dir_recursive(&snapshot, &dir, &[]) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("复制账号快照失败：{e}"));
        }
    }

    let item = TraeInstance {
        id: id.clone(),
        name: name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&id)
            .to_string(),
        account_uid: uid,
        data_dir: dir.to_string_lossy().to_string(),
        target_app: "TraeWork".into(),
        created_at: now_ts(),
        pid: None,
    };
    file.instances.push(item.clone());
    if let Err(e) = save_registry(&state, &file) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(format!("保存实例登记失败：{e}"));
    }
    fs_utils::app_log(&state.data_dir, &format!("已创建 Trae 独立实例: id={id}, seed_snapshot={seed_snapshot}"));
    Ok(instance_view(&item))
}

/// 启动指定实例。实例必须已创建且数据目录位于本应用管理目录内。
#[tauri::command(async)]
pub fn trae_instance_start(
    state: State<AppState>,
    instance_id: String,
    proxy_port: Option<u16>,
) -> Result<TraeInstanceView, String> {
    let id = instance_id.trim().to_string();
    validate_instance_id(&id)?;
    validate_port(proxy_port)?;
    let mut file = load_registry(&state);
    let item = file
        .instances
        .iter_mut()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("未找到实例：{id}"))?;
    if process_running(item.pid.unwrap_or(0)) {
        return Err("该实例已在运行".into());
    }
    let root = instances_root(&state);
    let dir = PathBuf::from(&item.data_dir);
    let root_canon = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let dir_canon = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
    if !dir_canon.starts_with(&root_canon) || !dir.is_dir() {
        return Err("实例数据目录无效，必须位于 AIWorkAssistant/data/trae_instances 下".into());
    }
    let exe = crate::commands::env::detect_target_exe(&state, "TraeWork")?;
    let mut cmd = Command::new(&exe);
    cmd.arg(format!("--user-data-dir={}", dir.display()))
        .arg("--new-window");
    if let Some(port) = proxy_port {
        cmd.arg(format!("--proxy-server=http://127.0.0.1:{port}"));
    }
    let child = cmd
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("启动 Trae 独立实例失败：{e}"))?;
    item.pid = Some(child.id());
    let result = instance_view(item);
    save_registry(&state, &file)?;
    fs_utils::app_log(&state.data_dir, &format!("已启动 Trae 独立实例: id={id}, pid={}", child.id()));
    Ok(result)
}

/// 停止指定实例。仅终止登记的 PID，不删除实例目录；PID 已结束时只清理登记状态。
#[tauri::command(async)]
pub fn trae_instance_stop(state: State<AppState>, instance_id: String) -> Result<TraeInstanceView, String> {
    let id = instance_id.trim().to_string();
    validate_instance_id(&id)?;
    let mut file = load_registry(&state);
    let item = file
        .instances
        .iter_mut()
        .find(|i| i.id == id)
        .ok_or_else(|| format!("未找到实例：{id}"))?;
    if let Some(pid) = item.pid {
        if process_running(pid) {
            let status = Command::new("taskkill")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .map_err(|e| format!("停止实例失败：{e}"))?;
            if !status.success() {
                return Err(format!("停止实例失败（taskkill exit={:?}）", status.code()));
            }
        }
        item.pid = None;
    }
    let result = instance_view(item);
    save_registry(&state, &file)?;
    fs_utils::app_log(&state.data_dir, &format!("已停止 Trae 独立实例: id={id}"));
    Ok(result)
}

/// 删除登记但保留本地数据目录，避免误删登录态/项目数据；可再次登记前需人工清理目录。
#[tauri::command]
pub fn trae_instance_forget(state: State<AppState>, instance_id: String) -> Result<(), String> {
    let id = instance_id.trim().to_string();
    validate_instance_id(&id)?;
    let mut file = load_registry(&state);
    let Some(pos) = file.instances.iter().position(|i| i.id == id) else {
        return Err(format!("未找到实例：{id}"));
    };
    if process_running(file.instances[pos].pid.unwrap_or(0)) {
        return Err("实例仍在运行，请先停止后再移除登记".into());
    }
    file.instances.remove(pos);
    save_registry(&state, &file)?;
    fs_utils::app_log(&state.data_dir, &format!("已移除 Trae 独立实例登记: id={id}（数据目录保留）"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_id_validation_rejects_path_injection() {
        assert!(validate_instance_id("account_a").is_ok());
        assert!(validate_instance_id("../escape").is_err());
        assert!(validate_instance_id("C:\\temp").is_err());
        assert!(validate_instance_id("").is_err());
    }

    #[test]
    fn port_validation_rejects_zero() {
        assert!(validate_port(None).is_ok());
        assert!(validate_port(Some(7864)).is_ok());
        assert!(validate_port(Some(0)).is_err());
    }

    #[test]
    fn process_check_rejects_pid_reuse_by_other_image() {
        let good = "\"TRAE SOLO CN.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"";
        let other = "\"notepad.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"";
        assert!(tasklist_matches_process(good, 1234, TRAE_WORK_IMAGES));
        assert!(!tasklist_matches_process(other, 1234, TRAE_WORK_IMAGES));
    }

    #[test]
    fn instance_root_is_deterministic() {
        let root = PathBuf::from("C:\\data");
        assert!(root.join("trae_instances").join("a").starts_with(&root));
    }
}
