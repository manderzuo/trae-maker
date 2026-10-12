pub fn refresh_client_id(issuer: Option<&str>) -> Result<&str, String> {
    match issuer.filter(|v| !v.trim().is_empty()) {
        None => Ok(crate::native_client::LEGACY_CLIENT_ID),
        Some(crate::native_client::LEGACY_CLIENT_ID) => Ok(crate::native_client::LEGACY_CLIENT_ID),
        Some(crate::native_client::TRAE_CLIENT_ID) => Ok(crate::native_client::TRAE_CLIENT_ID),
        _ => Err("凭据的 OAuth 客户端来源无法确认，请通过 TRAE CN 原生授权重新登录".into()),
    }
}

pub fn validate_refresh_uid(expected: &str, token: &str) -> Result<(), String> {
    let actual = crate::jwt::parse(token).user_id.ok_or("刷新结果缺少可验证的 Cloud-IDE UserID，旧凭据已保留")?;
    if actual != expected { return Err(format!("刷新后 user_id 不匹配: 期望={expected}, 实际={actual}；旧凭据已保留")); }
    Ok(())
}

pub fn device_proof(state: &crate::state::AppState, mut request: serde_json::Value) -> Result<serde_json::Value, String> {
    use std::os::windows::process::CommandExt;
    use std::io::Write;
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = LOCK.lock().map_err(|_| "设备凭据锁不可用")?;
    request["root"] = serde_json::json!(state.data_dir);
    let mut child = std::process::Command::new(&state.python_exe)
        .arg(state.python_dir.join("native_proof.py"))
        .env("AIWORKDATA_DIR", &state.data_dir)
        .env("AIWORK_NATIVE_CLIENT", serde_json::to_string(&crate::native_client::resolve_for("TraeWork", &state.settings())).map_err(|_| "设备身份编码失败")?)
        .env("PYTHONIOENCODING", "utf-8")
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
        .creation_flags(0x08000000).spawn().map_err(|_| "原生设备签名程序无法启动")?;
    child.stdin.take().ok_or("设备签名输入不可用")?.write_all(&serde_json::to_vec(&request).map_err(|_| "设备签名输入编码失败")?).map_err(|_| "设备签名输入失败")?;
    let output = child.wait_with_output().map_err(|_| "设备签名程序未正常结束")?;
    if !output.status.success() { return Err("原生设备凭据不可用，请重新进行原生授权；现有登录凭据已保留".into()); }
    serde_json::from_slice(&output.stdout).map_err(|_| "设备签名结果不可用".into())
}

pub fn export_device(state: &crate::state::AppState, uid: &str) -> Result<Option<serde_json::Value>, String> {
    if crate::fs_utils::ensure_uid_safe(uid).is_err() || !state.data_dir.join("conf/native_oauth_bindings").join(format!("{uid}.json")).is_file() { return Ok(None); }
    device_proof(state, serde_json::json!({"action":"export","uid":uid})).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    #[test]
    fn issuer_keeps_old_tokens_and_rejects_unknown_clients() {
        assert_eq!(refresh_client_id(None).unwrap(), "en1oxy7wnw8j9n");
        assert_eq!(refresh_client_id(Some("en1oxy7wnw8j9n")).unwrap(), "en1oxy7wnw8j9n");
        assert_eq!(refresh_client_id(Some("ono9krqynydwx5")).unwrap(), "ono9krqynydwx5");
        assert!(refresh_client_id(Some("unknown")).is_err());
    }
    #[test]
    fn refresh_must_prove_target_uid_before_overwriting_credentials() {
        let body = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"data":{"id":"123"}}"#);
        let token = format!("e30.{body}.signature");
        assert!(validate_refresh_uid("123", &token).is_ok());
        assert!(validate_refresh_uid("456", &token).is_err());
        assert!(validate_refresh_uid("123", "malformed").is_err());
    }
}
