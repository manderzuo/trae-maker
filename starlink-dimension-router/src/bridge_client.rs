use std::{collections::BTreeMap, io::Read, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct BridgeResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub trait BridgeTransport: Send + Sync {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String>;
}

#[derive(Clone)]
pub struct BridgeClient {
    base_url: String,
    bridge_secret: String,
    transport: Arc<dyn BridgeTransport>,
}

impl BridgeClient {
    pub fn new(base_url: impl Into<String>, bridge_secret: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bridge_secret: bridge_secret.into(),
            transport: Arc::new(HttpBridgeTransport),
        }
    }

    pub fn from_transport(
        base_url: impl Into<String>,
        bridge_secret: impl Into<String>,
        transport: Arc<dyn BridgeTransport>,
    ) -> Self {
        Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            bridge_secret: bridge_secret.into(),
            transport,
        }
    }

    pub fn base_url(&self) -> &str { &self.base_url }

    pub fn test(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/status", &[], None)
    }

    pub fn models(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/models", &[], None)
    }

    pub fn summary(&self) -> Result<Value, String> {
        self.json_request("GET", "/internal/bridge/summary", &[], None)
    }

    pub fn forward(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        incoming_headers: &BTreeMap<String, String>,
        request_id: &str,
    ) -> Result<BridgeResponse, String> {
        let mut headers = safe_headers(incoming_headers);
        // Core 用户 Key 永远不会透传给 AI Work；桥接密钥在这一层覆盖。
        headers.insert("authorization".into(), format!("Bearer {}", self.bridge_secret));
        headers.insert("x-core-request-id".into(), request_id.to_string());
        headers.remove("x-user-id");
        headers.remove("x-api-key");
        let url = format!("{}{}", self.base_url, normalize_path(path));
        self.transport.send(method, &url, &headers, body)
    }

    pub fn upload_asset(
        &self,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
        request_id: &str,
    ) -> Result<String, String> {
        if filename.trim().is_empty() || filename.len() > 128 || filename.contains('/') || filename.contains('\\') {
            return Err("桥接素材 filename 无效".into());
        }
        if mime_type.trim().is_empty() || mime_type.chars().any(char::is_whitespace) {
            return Err("桥接素材 mime_type 无效".into());
        }
        let body = serde_json::to_vec(&serde_json::json!({
            "filename": filename,
            "mime_type": mime_type,
            "data_base64": STANDARD.encode(bytes),
        })).map_err(|error| format!("编码桥接素材失败: {error}"))?;
        let mut headers = BTreeMap::new();
        headers.insert("content-type".into(), "application/json".into());
        let response = self.forward("POST", "/v1/assets", &body, &headers, request_id)?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work 素材桥接返回 HTTP {}", response.status));
        }
        let value: Value = serde_json::from_slice(&response.body)
            .map_err(|error| format!("AI Work 素材桥接响应不是有效 JSON: {error}"))?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .ok_or_else(|| "AI Work 素材桥接响应缺少素材 ID".into())
    }

    fn json_request(&self, method: &str, path: &str, body: &[u8], request_id: Option<&str>) -> Result<Value, String> {
        let mut incoming = BTreeMap::new();
        incoming.insert("accept".into(), "application/json".into());
        let response = self.forward(
            method,
            path,
            body,
            &incoming,
            request_id.unwrap_or("core-control-check"),
        )?;
        if !(200..300).contains(&response.status) {
            return Err(format!("AI Work bridge 返回 HTTP {}", response.status));
        }
        serde_json::from_slice(&response.body).map_err(|e| format!("桥接响应不是有效 JSON: {e}"))
    }
}

fn normalize_path(path: &str) -> String {
    if path.starts_with('/') { path.to_string() } else { format!("/{path}") }
}

fn safe_headers(input: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    input
        .iter()
        .filter(|(key, _)| matches!(key.as_str(), "accept" | "content-type" | "idempotency-key"))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.clone()))
        .collect()
}

struct HttpBridgeTransport;

impl BridgeTransport for HttpBridgeTransport {
    fn send(
        &self,
        method: &str,
        url: &str,
        headers: &BTreeMap<String, String>,
        body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(10))
            .timeout_read(Duration::from_secs(120))
            .timeout_write(Duration::from_secs(30))
            .build();
        let mut request = agent.request(method, url);
        for (key, value) in headers {
            request = request.set(key, value);
        }
        let result = request.send_bytes(body);
        let response = match result {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(error) => return Err(format!("桥接网络请求失败: {error}")),
        };
        let status = response.status() as u16;
        let content_type = response.header("content-type").map(ToString::to_string);
        let mut data = Vec::new();
        response
            .into_reader()
            .take(64 * 1024 * 1024)
            .read_to_end(&mut data)
            .map_err(|e| format!("读取桥接响应失败: {e}"))?;
        let mut response_headers = BTreeMap::new();
        if let Some(value) = content_type {
            response_headers.insert("content-type".into(), value);
        }
        Ok(BridgeResponse { status, headers: response_headers, body: data })
    }
}

#[cfg(test)]
mod tests {
    use super::{BridgeClient, BridgeResponse, BridgeTransport};
    use std::{collections::BTreeMap, sync::{Arc, Mutex}};

    struct RecordingBridge {
        last_headers: Mutex<BTreeMap<String, String>>,
        last_request_id: Mutex<Option<String>>,
        last_url: Mutex<String>,
        last_body: Mutex<Vec<u8>>,
        response_status: Mutex<u16>,
        response_body: Mutex<Vec<u8>>,
    }

    impl Default for RecordingBridge {
        fn default() -> Self {
            Self::responding_with_body(200, br#"{}"#)
        }
    }

    impl RecordingBridge {
        fn responding_with_asset(id: &str) -> Self {
            Self::responding_with_body(200, format!(r#"{{"object":"asset","id":"{id}"}}"#).as_bytes())
        }

        fn responding_with_body(status: u16, body: &[u8]) -> Self {
            Self {
                last_headers: Mutex::new(BTreeMap::new()),
                last_request_id: Mutex::new(None),
                last_url: Mutex::new(String::new()),
                last_body: Mutex::new(Vec::new()),
                response_status: Mutex::new(status),
                response_body: Mutex::new(body.to_vec()),
            }
        }

        fn last_path(&self) -> String {
            self.last_url.lock().unwrap().trim_start_matches("http://bridge").to_string()
        }

        fn last_headers(&self) -> BTreeMap<String, String> {
            self.last_headers.lock().unwrap().clone()
        }

        fn last_body_string(&self) -> String {
            String::from_utf8_lossy(&self.last_body.lock().unwrap()).to_string()
        }
    }

    impl BridgeTransport for RecordingBridge {
        fn send(&self, _method: &str, url: &str, headers: &BTreeMap<String, String>, body: &[u8]) -> Result<BridgeResponse, String> {
            *self.last_headers.lock().unwrap() = headers.clone();
            *self.last_request_id.lock().unwrap() = headers.get("x-core-request-id").cloned();
            *self.last_url.lock().unwrap() = url.to_owned();
            *self.last_body.lock().unwrap() = body.to_vec();
            Ok(BridgeResponse { status: *self.response_status.lock().unwrap(), headers: BTreeMap::new(), body: self.response_body.lock().unwrap().clone() })
        }
    }

    #[test]
    fn forwarding_overwrites_user_authorization_and_request_id() {
        let recording = Arc::new(RecordingBridge::default());
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let mut headers = BTreeMap::new();
        headers.insert("authorization".into(), "Bearer user-key".into());
        headers.insert("x-api-key".into(), "user-key".into());
        headers.insert("x-core-request-id".into(), "client-forged".into());
        client.forward("POST", "/v1/chat/completions", b"{}", &headers, "server-request").unwrap();
        let captured = recording.last_headers.lock().unwrap().clone();
        assert_eq!(captured.get("authorization"), Some(&"Bearer bridge-secret".to_string()));
        assert!(!captured.contains_key("x-api-key"));
        assert_eq!(captured.get("x-core-request-id"), Some(&"server-request".to_string()));
    }

    #[test]
    fn upload_asset_uses_bridge_authorization_and_returns_aiwork_asset_id() {
        let recording = Arc::new(RecordingBridge::responding_with_asset("bridge-asset-1"));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording.clone());
        let id = client.upload_asset("ref.png", "image/png", b"png", "asset-request").unwrap();
        assert_eq!(id, "bridge-asset-1");
        assert_eq!(recording.last_path(), "/v1/assets");
        assert_eq!(recording.last_headers()["authorization"], "Bearer bridge-secret");
        assert_eq!(recording.last_headers()["content-type"], "application/json");
        assert!(!recording.last_body_string().contains("user-key"));
    }

    #[test]
    fn malformed_bridge_asset_response_is_an_error() {
        let recording = Arc::new(RecordingBridge::responding_with_body(200, br#"{"object":"asset"}"#));
        let client = BridgeClient::from_transport("http://bridge", "bridge-secret", recording);
        let error = client.upload_asset("ref.png", "image/png", b"png", "asset-request").unwrap_err();
        assert!(error.contains("素材 ID"));
    }
}
