//! F-47 进程管理增强：三级关闭策略
//!
//! 参考社区实现（oss-ecosystem-research §5.3 rotate.rs）的三级关闭思路：
//! 1. **优雅关闭**：taskkill（不带 /F）向主窗口发送 WM_CLOSE，等待最长 3s，
//!    让 Electron 尽量正常落盘（避免强杀导致 leveldb/vscdb 文件锁与数据损坏）；
//!    实测 Electron 收到 WM_CLOSE 后通常 1s 内退出，3s 足够且不拖慢切换体验；
//! 2. **强杀进程树**：taskkill /T /F，等待最长 2s；
//! 3. **人工介入**：仍存活则返回 Err，由前端 toast 提示用户手动关闭。
//!
//! 匹配策略：仅按主程序映像名精确匹配（tasklist/taskkill 的 IMAGENAME），
//! crashpad-helper 等子进程不会独立命中；树杀阶段随主进程一并清理。
//! 所有子进程均以 CREATE_NO_WINDOW（0x08000000）拉起，不闪烁控制台窗口。

use std::os::windows::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

const CREATE_NO_WINDOW: u32 = 0x08000000;

#[cfg(test)]
mod native_tests {
    use super::*;
    #[test]
    fn native_default_shutdown_and_cdp_exclude_independent_instances() {
        let exe = std::path::Path::new("E:/apps/Trae CN.exe");
        assert!(owns_default_process(exe, "E:/apps/Trae CN.exe", "\"E:/apps/Trae CN.exe\" --remote-debugging-port=9333"));
        for command in ["\"E:/apps/Trae CN.exe\" --user-data-dir=E:/isolated", "\"E:/apps/Trae CN.exe\" --user-data-dir E:/isolated", "\"E:/apps/Trae CN.exe\" --type=renderer"] {
            assert!(!owns_default_process(exe, "E:/apps/Trae CN.exe", command));
        }
        assert!(!owns_default_process(exe, "E:/other/Trae CN.exe", "\"E:/other/Trae CN.exe\""));
    }
}

/// 检查任一映像名是否仍在运行（tasklist 精确匹配 IMAGENAME）
pub fn images_running(images: &[&str]) -> Vec<String> {
    let mut alive = Vec::new();
    for img in images {
        let running = Command::new("tasklist")
            .args(["/FI", &format!("IMAGENAME eq {img}"), "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .map(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                s.to_lowercase().contains(&img.to_lowercase())
            })
            .unwrap_or(false);
        if running {
            alive.push(img.to_string());
        }
    }
    alive
}

fn run_taskkill(args: &[&str]) {
    let _ = Command::new("taskkill")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output();
}

/// 三级关闭指定映像名的进程。
/// 返回 Ok(()) 表示全部退出；返回 Err 提示人工介入。
pub fn graceful_kill_images(images: &[&str]) -> Result<(), String> {
    let alive = images_running(images);
    if alive.is_empty() {
        return Ok(());
    }

    // ---- 第一级：优雅关闭（WM_CLOSE），最长等待 3s ----
    for img in &alive {
        // taskkill 不带 /F：向有窗口的进程发送关闭消息
        run_taskkill(&["/IM", img]);
    }
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if images_running(images).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // ---- 第二级：强杀进程树（/T /F），最长等待 2s ----
    for img in &alive {
        run_taskkill(&["/T", "/F", "/IM", img]);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if images_running(images).is_empty() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    // ---- 第三级：人工介入 ----
    let still = images_running(images).join("、");
    Err(format!(
        "进程 {still} 未能自动关闭（优雅关闭与强制结束均失败），请手动关闭后重试"
    ))
}

/// 按应用类别返回候选映像名（与 env.rs 的安装探测保持一致）。
pub fn images_for_app(app_kind: &str) -> Vec<&'static str> {
    let target = crate::native_client::canonical_target(app_kind);
    match target.as_str() {
        // Trae Work（SOLO CN）及其历史版本名
        "TraeWork" => vec!["TRAE SOLO CN.exe", "TRAE SOLO.exe", "Trae.exe"],
        // Trae CN IDE
        "Trae" => vec!["Trae CN.exe"],
        // 豆包桌面版（Chromium 壳，主进程即 Doubao.exe）
        "Doubao" => vec!["Doubao.exe"],
        // WorkBuddy 桌面版（批次3 会话备份/恢复前置关闭）
        "WorkBuddy" => vec!["WorkBuddy.exe"],
        // CodeBuddy 桌面版（安装形态含 CN 后缀，与 trae-switch-bridge.ps1 ProcNames 对齐；
        // 此前缺失导致 Rust 侧 graceful_kill_app 杀不掉 CodeBuddy，与桥行为不一致存竞态）
        "CodeBuddy" => vec!["CodeBuddy.exe", "CodeBuddy CN.exe"],
        _ => vec![],
    }
}

/// 便捷入口：按应用类别三级关闭
pub fn graceful_kill_app(app_kind: &str) -> Result<(), String> {
    if matches!(app_kind, "TraeWork" | "Trae") {
        return graceful_kill_native(&crate::native_client::resolve_for(app_kind, &crate::fs_utils::read_json(&crate::state::resolve_data_dir()?.join("conf/app_settings.json"))));
    }
    let images = images_for_app(app_kind);
    if images.is_empty() {
        return Err(format!("未知的应用类别: {app_kind}"));
    }
    graceful_kill_images(&images)
}

pub(crate) fn owns_default_process(exe: &std::path::Path, actual_exe: &str, command_line: &str) -> bool {
    !command_line.trim().is_empty()
        && exe.to_string_lossy().replace('\\', "/").eq_ignore_ascii_case(&actual_exe.replace('\\', "/"))
        && !command_line.to_ascii_lowercase().contains("--user-data-dir")
        && !command_line.to_ascii_lowercase().contains("--type=")
        && !command_line.to_ascii_lowercase().contains("--type ")
        && !command_line.to_ascii_lowercase().contains("--utility-sub-type")
}

fn native_root_pids(profile: &crate::native_client::NativeClientProfile) -> Result<Vec<u32>, String> {
    let Some(exe) = profile.exe.as_ref() else { return Err("未找到目标 TRAE 客户端路径".into()); };
    let ps = format!("@(Get-CimInstance Win32_Process -Filter \"ExecutablePath='{}'\" | Select-Object ProcessId,ExecutablePath,CommandLine) | ConvertTo-Json -Compress", exe.to_string_lossy().replace('\\', "\\\\").replace('\'', "''"));
    let out = Command::new("powershell").args(["-NoProfile", "-Command", &ps]).creation_flags(CREATE_NO_WINDOW).output().map_err(|e| e.to_string())?;
    if !out.status.success() { return Err("无法确定目标进程归属，请手动关闭客户端".into()); }
    if out.stdout.iter().all(u8::is_ascii_whitespace) { return Ok(vec![]); }
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).map_err(|_| "无法读取目标进程信息")?;
    let rows = value.as_array().cloned().unwrap_or_else(|| if value.is_object() { vec![value] } else { vec![] });
    Ok(rows.iter().filter(|v| owns_default_process(exe, v["ExecutablePath"].as_str().unwrap_or_default(), v["CommandLine"].as_str().unwrap_or_default()))
        .filter_map(|v| v["ProcessId"].as_u64().map(|id| id as u32)).collect())
}

pub(crate) fn graceful_kill_native(profile: &crate::native_client::NativeClientProfile) -> Result<(), String> {
    let pids = native_root_pids(profile)?;
    if pids.is_empty() { return Ok(()); }
    for pid in &pids { run_taskkill(&["/PID", &pid.to_string()]); }
    let alive = || -> Vec<u32> { native_root_pids(profile).unwrap_or_else(|_| pids.clone()).into_iter().filter(|p| pids.contains(p)).collect() };
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline { if alive().is_empty() { return Ok(()); } std::thread::sleep(Duration::from_millis(250)); }
    for pid in alive() { run_taskkill(&["/T", "/F", "/PID", &pid.to_string()]); }
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline { if alive().is_empty() { return Ok(()); } std::thread::sleep(Duration::from_millis(250)); }
    Err("目标 TRAE 默认实例未能关闭，请手动关闭后重试；独立实例已保留".into())
}
