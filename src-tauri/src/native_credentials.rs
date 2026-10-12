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
