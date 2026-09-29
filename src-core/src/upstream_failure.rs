//! Public provider failure details: explicit fields only, never raw response bodies.
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamFailure {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
}

impl UpstreamFailure {
    pub fn from_body(body: &str, status: Option<u16>) -> Self {
        let mut detail = match serde_json::from_str::<Value>(body) {
            Ok(value) => Self::from_value(&value),
            Err(_) => Self::from_plain_or_json(body),
        };
        if status.is_some() { detail.http_status = status.filter(|s| (400..600).contains(s)); }
        detail
    }

    pub fn from_value(value: &Value) -> Self {
        if let Some(text) = value.as_str() { return Self::from_plain_or_json(text); }
        // New bridge payloads preserve these fields; old tasks only have error.
        let error = value.get("upstream_error").filter(|v| v.is_object())
            .or_else(|| value.get("error").filter(|v| v.is_object())).unwrap_or(value);
        let message = error.get("message").and_then(Value::as_str)
            .or_else(|| error.get("error").and_then(Value::as_str));
        let code = error.get("code").or_else(|| error.get("error_code")).and_then(|v| {
            v.as_str().map(str::to_owned).or_else(|| v.as_i64().map(|n| n.to_string()))
        }).filter(|s| !s.is_empty() && s.len() <= 80
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
            && public_text(s).as_deref() == Some(s));
        let http_status = error.get("http_status").or_else(|| error.get("status_code"))
            .and_then(Value::as_u64).filter(|s| (400..600).contains(s)).map(|s| s as u16);
        if code.is_none() && http_status.is_none() {
            if let Some(message) = message { return Self::from_plain_or_json(message); }
        }
        Self { code, message: message.and_then(public_text), http_status }
    }

    fn from_plain_or_json(text: &str) -> Self {
        // The old HTTP adapter stored a textual prefix around a JSON response.
        if let Some(rest) = text.strip_prefix("Seedance 上游 HTTP ") {
            if let Some((status, body)) = rest.split_once(':') {
                if let Ok(status) = status.parse::<u16>() { return Self::from_body(body.trim(), Some(status)); }
            }
        }
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            if value.is_object() { return Self::from_value(&value); }
            // Do not recurse through quoted strings or dump arrays/HTML.
            return Self::default();
        }
        Self { message: public_text(text), ..Self::default() }
    }

    pub fn description(&self) -> String {
        let mut parts = Vec::new();
        if let Some(status) = self.http_status { parts.push(format!("HTTP {status}")); }
        if let Some(code) = &self.code { parts.push(format!("错误码 {code}")); }
        parts.push(self.message.clone().unwrap_or_else(|| "上游未提供可公开的具体错误说明".into()));
        parts.join("；")
    }
}

// Fail closed on structured dumps/HTML. Extracting only message/code above avoids
// accidentally forwarding request bodies, headers, accounts or stack traces.
fn public_text(text: &str) -> Option<String> {
    let text = text.trim();
    if text.is_empty() || text.starts_with(['<', '{'])
        || (text.starts_with('[') && serde_json::from_str::<Value>(text).is_ok_and(|v|v.is_array())) { return None; }
    // Bound work before scanning; do not allow control characters to forge lines.
    let mut safe: String = text.chars().take(4096)
        .map(|c| if c.is_control() { ' ' } else { c }).collect();
    // Keep ordinary error words, but suppress credentials/identity fields and
    // their values. Structured secret fields are never copied in the first place.
    for marker in ["authorization", "proxy-authorization", "cookie", "set-cookie",
        "api_key", "api-key", "apikey", "access_token", "refresh_token", "token",
        "password", "secret", "ticket", "account_id", "account", "uid", "email",
        "device_id", "session_id", "jwt", "账号", "账户", "密码"] {
        let mut offset = 0;
        loop {
            let lower = safe.to_ascii_lowercase();
            let Some(n) = lower[offset..].find(marker) else { break };
            let start = offset+n;
            let after = start+marker.len();
            let boundary = start == 0 || !safe[..start].chars().next_back().is_some_and(|c| c.is_ascii_alphanumeric() || c=='_');
            let tail = &safe[after..];
            let trimmed = tail.trim_start_matches([' ', '"', '\'']);
            if !boundary || !trimmed.starts_with([':', '=']) { offset=after; continue; }
            let end = safe[after..].char_indices().find(|(_,c)| matches!(c, ';' | ',' | '，' | '；'))
                .map(|(i,_)|after+i).unwrap_or(safe.len());
            safe.replace_range(start..end, "[已隐藏]");
            offset=start+"[已隐藏]".len();
        }
    }
    for prefix in ["http://", "https://", "tos://", "file://", "data:", "bearer ",
        "aw_live_", "sk-", "eyj", "c:\\", "d:\\", "e:\\", "/home/", "/var/", "/opt/", "/tmp/", "/users/"] {
        let mut offset=0;
        loop {
            let lower=safe.to_ascii_lowercase();
            let Some(n)=lower[offset..].find(prefix) else { break };
            let start=offset+n;
            let end=safe[start+prefix.len()..].char_indices()
                .find(|(_,c)| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '；'))
                .map(|(i,_)|start+prefix.len()+i).unwrap_or(safe.len());
            safe.replace_range(start..end, "[已隐藏]");offset=start+"[已隐藏]".len();
        }
    }
    // Opaque identifiers, email addresses, hostnames and IPs may appear without a label.
    let mut out=String::new();
    let mut token=String::new();
    let flush=|token:&mut String,out:&mut String| {
        let account_number=token.len()>=10 && token.bytes().all(|b|b.is_ascii_digit());
        let opaque=account_number || (token.len()>=24 && token.bytes().all(|b|b.is_ascii_alphanumeric() || b"_-.".contains(&b)));
        let address=token.contains('@') || token.split('.').count()>=3
            || (token.contains('.') && token.chars().any(|c|c.is_ascii_alphabetic())
                && !token.ends_with('.'));
        if opaque || address {out.push_str("[已隐藏]");} else {out.push_str(token);}
        token.clear();
    };
    for c in safe.chars() {
        if c.is_ascii_alphanumeric() || "_-.@".contains(c) { token.push(c); }
        else {flush(&mut token,&mut out);out.push(c);}
    }
    flush(&mut token,&mut out);
    // Avoid rendering error prose as active HTML / Markdown links.
    let out=out.replace('<',"＜").replace('>',"＞").replace('`',"'");
    Some(out.chars().take(1000).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn upstream_failure_keeps_structured_reason_not_response_secrets() {
        let detail=UpstreamFailure::from_body(r#"{"error":{"code":"INVALID_DURATION","message":"Video duration exceeds 15 seconds","token":"secret"},"account":"private"}"#,Some(422));
        assert_eq!(detail.code.as_deref(),Some("INVALID_DURATION"));
        assert_eq!(detail.message.as_deref(),Some("Video duration exceeds 15 seconds"));
        assert_eq!(detail.http_status,Some(422));
        assert!(!serde_json::to_string(&detail).unwrap().contains("secret"));
        assert_eq!(UpstreamFailure::from_value(&json!({"error":"video security check failed"})).message.as_deref(),Some("video security check failed"));
    }
    #[test]
    fn upstream_failure_redacts_labeled_and_unlabeled_credentials() {
        let detail=UpstreamFailure::from_body("duration invalid; Authorization: Bearer short-secret; token=private-token; uid=123456; https://private.example/?ticket=secret; aw_live_abcdefghijklmnop; 10.0.0.1; test@example.com; eyJhbGciOiJIUzI1NiJ9.payload.sig",None);
        let message=detail.message.unwrap();
        assert!(message.starts_with("duration invalid"));
        for secret in ["short-secret","private-token","123456","private.example","abcdefghijklmnop","10.0.0.1","test@example.com","eyJhb"] {assert!(!message.contains(secret),"{message}");}
        let plain=UpstreamFailure::from_body("Request denied for account 1501295980065147 at /opt/private/credentials",None).message.unwrap();
        assert!(!plain.contains("1501295980065147"));
        assert!(!plain.contains("/opt/private"));
    }
    #[test]
    fn upstream_failure_unknown_or_malformed_body_does_not_invent_a_reason() {
        for body in ["", "<html>server account=private</html>", r#"{"token":"secret"}"#, "[{\"message\":\"private\"}]"] {
            let detail=UpstreamFailure::from_body(body,Some(503));
            assert_eq!(detail.message,None);assert_eq!(detail.code,None);
            assert_eq!(detail.http_status,Some(503));
        }
        let legacy=UpstreamFailure::from_body("Seedance 上游 HTTP 400: {\"error\":{\"code\":\"BAD_MEDIA\",\"message\":\"Invalid reference video\"}}",None);
        assert_eq!(legacy.code.as_deref(),Some("BAD_MEDIA"));assert_eq!(legacy.http_status,Some(400));
    }
    #[test]
    fn redaction_marker_survives_bridge_roundtrip() {
        let first=UpstreamFailure::from_body("Reference too long; token=private-token",None);
        let bridged=serde_json::json!({"upstream_error":first});
        let second=UpstreamFailure::from_value(&bridged);
        assert_eq!(second.message.as_deref(),Some("Reference too long; [已隐藏]"));
    }
}
