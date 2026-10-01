//! Scheduled entry point: no WebView, tray, proxy, or video API startup.
use crate::{commands::checkin, state::AppState};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Default, Serialize, Deserialize)]
pub struct LastRun {
    pub started_at: String,
    pub finished_at: String,
    pub status: String,
    pub ok: usize,
    pub already: usize,
    pub failed: usize,
    pub message: String,
    #[serde(default)]
    pub credential_accounts: usize,
}

pub fn run(state: &AppState, not_before: Option<&str>) -> i32 {
    if let Some(time) = not_before {
        if crate::commands::misc::validate_hhmm(time).is_err() {
            return 2;
        }
        if chrono::Local::now().format("%H:%M").to_string().as_str() < time {
            return 0; // Logon before the daily time is not a catch-up run.
        }
    }
    let started_at = crate::fs_utils::now_ts();
    let _report_guard = match ProcessGuard::acquire(&state.data_dir) {
        Ok(guard) => guard,
        Err(reason) => {
            crate::fs_utils::app_log(&state.data_dir, &format!("定时签到未重复执行: {reason}"));
            return 1;
        }
    };
    // Hydrate from the same vault used by interactive checkin. Never print or
    // persist the credentials themselves, only an aggregate readiness count.
    let credential_accounts = crate::vault::load_accounts(state)
        .accounts
        .iter()
        .filter(|a| !a.jwt.trim().is_empty())
        .count();
    let running = LastRun {
        started_at: started_at.clone(),
        status: "running".into(),
        credential_accounts,
        ..Default::default()
    };
    if crate::fs_utils::write_json(&state.path("daily_checkin_last_run.json"), &running).is_err() {
        return 1;
    }
    let settings = state.settings();
    let (tx, _rx) = std::sync::mpsc::channel();
    let result = checkin::run_checkin_worker(
        &checkin::WorkerUi(None),
        state,
        checkin::CheckinOpts {
            scope: "all".into(),
            user_ids: None,
            skip_checked_in: settings.checkin_skip_checked,
            skip_expired: settings.checkin_skip_expired,
        },
        false,
        tx,
    );
    let mut report = LastRun {
        started_at,
        finished_at: crate::fs_utils::now_ts(),
        credential_accounts,
        ..Default::default()
    };
    let exit = match result {
        Ok(summary) => {
            report.ok = summary.ok;
            report.already = summary.already;
            report.failed = summary.failed;
            report.status = if summary.failed > 0 {
                "failed"
            } else {
                "completed"
            }
            .into();
            report.message = summary.message;
            i32::from(summary.failed > 0)
        }
        Err(reason) => {
            report.status = "failed".into();
            report.message = reason;
            1
        }
    };
    if crate::fs_utils::write_json(&state.path("daily_checkin_last_run.json"), &report).is_err() {
        return 1;
    }
    crate::fs_utils::app_log(
        &state.data_dir,
        &format!(
            "定时签到结束: {} · 成功 {} · 已签 {} · 失败 {} · {}",
            report.status, report.ok, report.already, report.failed, report.message
        ),
    );
    exit
}

/// Windows named mutex: abandoned owners recover automatically after a crash.
pub struct ProcessGuard(isize, std::marker::PhantomData<std::rc::Rc<()>>);
impl ProcessGuard {
    pub fn acquire(data_dir: &Path) -> Result<Self, String> {
        use sha2::{Digest, Sha256};
        use windows_sys::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let directory = data_dir.canonicalize().map_err(|_| "签到数据目录不可用")?;
        let digest = format!(
            "{:x}",
            Sha256::digest(directory.to_string_lossy().to_lowercase().as_bytes())
        );
        let name: Vec<u16> = format!("Local\\AIWorkCheckin_{digest}")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
            if handle.is_null() {
                return Err("无法创建签到互斥锁".into());
            }
            let wait = WaitForSingleObject(handle, 0);
            if wait == 0 || wait == 0x80 {
                Ok(Self(handle as isize, std::marker::PhantomData))
            } else {
                windows_sys::Win32::Foundation::CloseHandle(handle);
                Err("已有签到任务正在进行中，本次未重复执行".into())
            }
        }
    }
}
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::System::Threading::ReleaseMutex(self.0 as _);
            windows_sys::Win32::Foundation::CloseHandle(self.0 as _);
        }
    }
}

pub fn describe(report: &LastRun) -> String {
    let status = match report.status.as_str() {
        "completed" => "已完成",
        "failed" => "失败或部分失败",
        "running" => "执行中或上次未正常结束",
        _ => "结果记录不可用",
    };
    format!(
        "最近定时签到：{status}\n开始：{}\n结束：{}\n成功 {} · 已签到 {} · 失败 {}\n{}",
        report.started_at,
        report.finished_at,
        report.ok,
        report.already,
        report.failed,
        report.message
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aiwork-daily-checkin-{}", rand::random::<u64>()));
        std::fs::create_dir_all(dir.join("data")).unwrap();
        std::fs::create_dir_all(dir.join("conf")).unwrap();
        std::fs::write(
            dir.join("conf/app_settings.json"),
            r#"{"checkin_skip_checked":true,"checkin_skip_expired":true,"retry":0}"#,
        )
        .unwrap();
        dir
    }
    #[test]
    fn cross_thread_mutex_blocks_duplicate_then_recovers_when_owner_releases() {
        let dir = directory();
        let held = ProcessGuard::acquire(&dir).unwrap();
        let other = dir.clone();
        assert!(
            std::thread::spawn(move || ProcessGuard::acquire(&other).is_err())
                .join()
                .unwrap()
        );
        drop(held);
        assert!(ProcessGuard::acquire(&dir).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn startup_cleanup_preserves_credentials_of_an_active_worker() {
        let dir = directory();
        let temporary = dir.join("trae_checkin_accounts_active.json");
        std::fs::write(&temporary, "{}").unwrap();
        let held = ProcessGuard::acquire(&dir).unwrap();
        let other = dir.clone();
        std::thread::spawn(move || {
            let state = AppState {
                data_dir: other.clone(),
                python_dir: other,
                python_exe: "must-not-execute.exe".into(),
                jwt_refresh_lock: std::sync::Mutex::new(()),
            };
            crate::vault::cleanup_temp_accounts(&state);
        })
        .join()
        .unwrap();
        assert!(
            temporary.exists(),
            "startup must not delete an active worker's credentials"
        );
        drop(held);
        let state = AppState {
            data_dir: dir.clone(),
            python_dir: dir.clone(),
            python_exe: "must-not-execute.exe".into(),
            jwt_refresh_lock: std::sync::Mutex::new(()),
        };
        crate::vault::cleanup_temp_accounts(&state);
        assert!(
            !temporary.exists(),
            "abandoned credentials should still be cleaned"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn scheduled_missing_credentials_is_failure_not_false_success() {
        let dir = directory();
        std::fs::write(
            dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"test","UserID":"test-user","jwt":""}]}"#,
        )
        .unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            python_dir: dir.clone(),
            python_exe: "must-not-execute.exe".into(),
            jwt_refresh_lock: std::sync::Mutex::new(()),
        };
        assert_eq!(run(&state, None), 1);
        let report: LastRun =
            crate::fs_utils::read_json(&dir.join("data/daily_checkin_last_run.json"));
        assert_eq!(report.status, "failed");
        assert_eq!(report.failed, 1);
        assert!(report.message.contains("凭据"));
        assert!(
            !std::fs::read_to_string(dir.join("data/checkin_accounts.json"))
                .unwrap()
                .contains("Cloud-IDE-JWT")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn already_signed_accounts_are_not_submitted_again() {
        let dir = directory();
        std::fs::write(
            dir.join("data/checkin_accounts.json"),
            r#"{"accounts":[{"name":"test","UserID":"test-user","jwt":""}]}"#,
        )
        .unwrap();
        crate::fs_utils::write_json(&dir.join("data/checkin_summary.json"),&serde_json::json!({"time":chrono::Local::now().to_rfc3339(),"results":[{"user_id":"test-user","ok":true,"action":"claim_ok"}]})).unwrap();
        let state = AppState {
            data_dir: dir.clone(),
            python_dir: dir.clone(),
            python_exe: "must-not-execute.exe".into(),
            jwt_refresh_lock: std::sync::Mutex::new(()),
        };
        assert_eq!(run(&state, None), 0);
        let report: LastRun =
            crate::fs_utils::read_json(&dir.join("data/daily_checkin_last_run.json"));
        assert_eq!(report.status, "completed");
        assert_eq!(report.already, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
