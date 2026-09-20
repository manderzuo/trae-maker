use serde_json::Value;
use std::collections::BTreeMap;

use super::assets::AssetRecord;
use super::pool::PickedAccount;
use super::{streaming_agent, APP_ID, AGENT_HOST, IDE_VERSION, IDE_VERSION_CODE, REFERER_BASE};

const RESOURCE_BIZ_TYPE: &str = "remote_resource";
const IMAGE_BIZ_TYPE: &str = "image";
const MAGIC_V2_HEADER: &[u8; 8] = &[0, 0, 0, 0, 27, 198, 174, 134];
const MAGIC_V2_XOR_KEY: &[u8; 37] = &[
    234, 159, 186, 198, 149, 201, 133, 234, 157, 137, 32, 225, 181, 187, 194, 167, 32, 234,
    157, 137, 198, 149, 226, 130, 172, 32, 195, 159, 226, 130, 172, 194, 167, 234, 157,
    137, 33,
];

#[derive(Debug, Clone)]
pub struct NativeUploadError {
    pub message: String,
    pub retryable_account: bool,
}

impl NativeUploadError {
    pub(crate) fn new(message: impl Into<String>, retryable_account: bool) -> Self {
        Self {
            message: message.into(),
            retryable_account,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct UploadPlan {
    upload_host: String,
    auth: String,
    store_uri: String,
    resource_id: Option<String>,
    session_key: Option<String>,
    upload_headers: BTreeMap<String, String>,
}

fn crc32_value(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320u32 & mask);
        }
    }
    !crc
}

fn crc32_hex(bytes: &[u8]) -> String {
    // The current Trae remote-attachment uploader emits the native
    // JavaScript `crc32.toString(16)` representation (no forced padding).
    format!("{:x}", crc32_value(bytes))
}

fn crc32_padded_hex(bytes: &[u8]) -> String {
    // The native image uploader uses `.padStart(8, "0")`.
    format!("{:08x}", crc32_value(bytes))
}

fn build_upload_payload(target: &str) -> Value {
    serde_json::json!({
        "targets": [target],
        "biz_type": RESOURCE_BIZ_TYPE
    })
}

fn build_image_upload_payload(target: &str, width: u32, height: u32) -> Value {
    serde_json::json!({
        "targets": [target],
        "biz_type": IMAGE_BIZ_TYPE,
        "scale_param": {"width": width, "height": height}
    })
}

fn native_upload_method() -> &'static str {
    // The current ai-modules-chat remote attachment uploader uses PUT.
    "PUT"
}

fn build_upload_headers(
    auth: &str,
    crc32: &str,
    upload_headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    // Match the current ai-modules-chat remote attachment uploader. The
    // attachment has already been wrapped with the Magic Number V2 envelope,
    // so TOS receives it as an octet-stream; it does not receive a browser
    // filename or a second MIME header.
    let mut headers = upload_headers.clone();
    headers.insert("Authorization".into(), auth.into());
    headers.insert("Content-CRC32".into(), crc32.into());
    headers.insert("Content-Type".into(), "application/octet-stream".into());
    headers
}

fn build_image_upload_headers(
    auth: &str,
    crc32: &str,
    content_type: &str,
    upload_headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut headers = upload_headers.clone();
    headers.insert("Authorization".into(), auth.into());
    headers.insert("Content-CRC32".into(), crc32.into());
    headers.insert(
        "Content-Type".into(),
        if content_type.trim().is_empty() {
            "application/octet-stream"
        } else {
            content_type.trim()
        }
        .into(),
    );
    headers
}

fn encode_remote_attachment(bytes: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(MAGIC_V2_HEADER.len() + bytes.len());
    encoded.extend_from_slice(MAGIC_V2_HEADER);
    encoded.extend(
        bytes
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ MAGIC_V2_XOR_KEY[index % MAGIC_V2_XOR_KEY.len()]),
    );
    encoded
}

fn native_target_path() -> String {
    format!("{}.trae", crate::commands::oauth::random_hex(32))
}

fn image_target_path(width: u32, height: u32) -> String {
    format!(
        "{}_{}x{}.trae",
        crate::commands::oauth::random_hex(32),
        width,
        height
    )
}

fn build_commit_payload(store_uri: &str, session_key: Option<&str>) -> Value {
    build_commit_payload_for(store_uri, session_key, RESOURCE_BIZ_TYPE)
}

fn build_commit_payload_for(
    store_uri: &str,
    session_key: Option<&str>,
    biz_type: &str,
) -> Value {
    let mut payload = serde_json::json!({
        "oids": [store_uri],
        "biz_type": biz_type
    });
    if let Some(session_key) = session_key.filter(|value| !value.trim().is_empty()) {
        payload["session_key"] = Value::String(session_key.to_string());
    }
    payload
}

fn parse_upload_url_response(value: &Value) -> Result<UploadPlan, NativeUploadError> {
    let data = value.get("data").unwrap_or(value);
    let upload_host = data
        .get("upload_hosts")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.trim_end_matches('/').to_string())
        .ok_or_else(|| NativeUploadError::new("Trae 未返回素材上传地址", false))?;
    let store = data
        .get("store_infos")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .ok_or_else(|| NativeUploadError::new("Trae 未返回素材存储信息", false))?;
    let auth = store
        .get("auth")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| NativeUploadError::new("Trae 未返回素材上传授权", false))?
        .to_string();
    let store_uri = store
        .get("store_uri")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.contains(char::is_whitespace))
        .ok_or_else(|| NativeUploadError::new("Trae 未返回素材 URI", false))?
        .to_string();
    let resource_id = store
        .get("override_resource_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let session_key = data
        .get("session_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let upload_headers = [
        store.get("uploadHeader"),
        store.get("upload_header"),
        store.get("UploadHeader"),
        data.get("uploadHeader"),
        data.get("upload_header"),
        data.get("UploadHeader"),
    ]
    .into_iter()
    .flatten()
    .next()
    .and_then(Value::as_object)
    .map(|headers| {
        headers
            .iter()
            .filter_map(|(name, value)| value.as_str().map(|value| (name.clone(), value.to_string())))
            .collect()
    })
    .unwrap_or_default();
    Ok(UploadPlan {
        upload_host,
        auth,
        store_uri,
        resource_id,
        session_key,
        upload_headers,
    })
}

fn request_error(status: u16, body: &str, context: &str) -> NativeUploadError {
    let lower = body.to_ascii_lowercase();
    let retryable = status == 401
        || status == 403
        || lower.contains("unauthorized")
        || lower.contains("invalid token")
        || lower.contains("session")
        || lower.contains("jwt");
    NativeUploadError::new(
        format!("{context} HTTP {status}: {}", body.chars().take(240).collect::<String>()),
        retryable,
    )
}

fn common_request(account: &PickedAccount, path: &str) -> ureq::Request {
    let url = format!("{AGENT_HOST}{path}");
    let referer = format!("{REFERER_BASE}{path}");
    streaming_agent()
        .post(&url)
        .set("content-type", "application/json")
        .set("accept", "*/*")
        .set("user-agent", "TraeClient/TTNet")
        .set("x-ide-token", &account.jwt)
        .set("x-app-id", APP_ID)
        .set("x-app-version", "default")
        .set("x-app-version-code", IDE_VERSION_CODE)
        .set("x-ide-version", IDE_VERSION)
        .set("x-ide-version-code", IDE_VERSION_CODE)
        .set("x-ide-version-type", "stable")
        .set("x-device-type", "windows")
        .set("x-device-brand", "H610E-B")
        .set("x-device-cpu", "Intel")
        .set("x-device-id", &account.device_id)
        .set("x-machine-id", &account.machine_id)
        .set("x-os-version", "Windows 10 Pro")
        .set("request-traffic-type", "prod")
        .set("package-type", "stable_cn")
        .set("x-lgw-req-sdk-type", "3")
        .set("x-lscbd-aid", "787976")
        .set("x-lscbd-platform", "windows")
        .set("x-ss-dp", "787976")
        .set("app-version", IDE_VERSION)
        .set("referer", &referer)
}

fn request_upload_plan(
    account: &PickedAccount,
    target: &str,
) -> Result<UploadPlan, NativeUploadError> {
    request_upload_plan_with_payload(account, build_upload_payload(target))
}

fn request_upload_plan_with_payload(
    account: &PickedAccount,
    payload: Value,
) -> Result<UploadPlan, NativeUploadError> {
    let response = common_request(account, "/api/ide/v1/get_resource_upload_url").send_json(payload);
    match response {
        Ok(response) => response
            .into_json::<Value>()
            .map_err(|error| NativeUploadError::new(format!("解析 Trae 素材上传响应失败: {error}"), false))
            .and_then(|value| parse_upload_url_response(&value)),
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "获取 Trae 素材上传地址失败"))
        }
        Err(error) => Err(NativeUploadError::new(
            format!("获取 Trae 素材上传地址失败: {error}"),
            false,
        )),
    }
}

fn upload_bytes(plan: &UploadPlan, bytes: &[u8]) -> Result<(), NativeUploadError> {
    let host = plan
        .upload_host
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let url = format!("https://{host}/{}", plan.store_uri.trim_start_matches('/'));
    let headers = build_upload_headers(&plan.auth, &crc32_hex(bytes), &plan.upload_headers);
    let mut request = match native_upload_method() {
        "PUT" => streaming_agent().put(&url),
        _ => unreachable!("unsupported native upload method"),
    };
    for (name, value) in &headers {
        request = request.set(name, value);
    }
    let response = request.send_bytes(bytes);
    match response {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => {
            let status = response.status();
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "上传 Trae 素材失败"))
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "上传 Trae 素材失败"))
        }
        Err(error) => Err(NativeUploadError::new(
            format!("上传 Trae 素材失败: {error}"),
            false,
        )),
    }
}

fn upload_image_bytes(
    plan: &UploadPlan,
    bytes: &[u8],
    content_type: &str,
) -> Result<(), NativeUploadError> {
    let host = plan
        .upload_host
        .trim()
        .trim_end_matches('/')
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let url = format!("https://{host}/{}", plan.store_uri.trim_start_matches('/'));
    let headers = build_image_upload_headers(
        &plan.auth,
        &crc32_padded_hex(bytes),
        content_type,
        &plan.upload_headers,
    );
    let mut request = streaming_agent().put(&url);
    for (name, value) in &headers {
        request = request.set(name, value);
    }
    let response = request.send_bytes(bytes);
    match response {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => {
            let status = response.status();
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "上传 Trae 图片素材失败"))
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "上传 Trae 图片素材失败"))
        }
        Err(error) => Err(NativeUploadError::new(
            format!("上传 Trae 图片素材失败: {error}"),
            false,
        )),
    }
}

fn commit_upload(
    account: &PickedAccount,
    plan: &UploadPlan,
) -> Result<(), NativeUploadError> {
    commit_upload_for(account, plan, RESOURCE_BIZ_TYPE)
}

fn commit_upload_for(
    account: &PickedAccount,
    plan: &UploadPlan,
    biz_type: &str,
) -> Result<(), NativeUploadError> {
    let Some(session_key) = plan.session_key.as_deref() else {
        return Ok(());
    };
    let response = common_request(account, "/api/ide/v1/commit_resource_upload_result")
        .send_json(build_commit_payload_for(&plan.store_uri, Some(session_key), biz_type));
    match response {
        Ok(response) if (200..300).contains(&response.status()) => Ok(()),
        Ok(response) => {
            let status = response.status();
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "提交 Trae 素材失败"))
        }
        Err(ureq::Error::Status(status, response)) => {
            let text = response.into_string().unwrap_or_default();
            Err(request_error(status, &text, "提交 Trae 素材失败"))
        }
        Err(error) => Err(NativeUploadError::new(
            format!("提交 Trae 素材失败: {error}"),
            false,
        )),
    }
}

pub fn upload_asset(
    account: &PickedAccount,
    record: &AssetRecord,
    bytes: &[u8],
) -> Result<String, NativeUploadError> {
    if record.mime_type.to_ascii_lowercase().starts_with("image/") {
        let (width, height) = detect_image_dimensions(bytes).ok_or_else(|| {
            NativeUploadError::new(
                format!("无法识别图片素材 {} 的宽高，拒绝提交给 Trae", record.filename),
                false,
            )
        })?;
        let target = image_target_path(width, height);
        let plan = request_upload_plan_with_payload(
            account,
            build_image_upload_payload(&target, width, height),
        )?;
        upload_image_bytes(&plan, bytes, &record.mime_type)?;
        commit_upload_for(account, &plan, IMAGE_BIZ_TYPE)?;
        return Ok(plan.resource_id.unwrap_or(plan.store_uri));
    }
    let target = native_target_path();
    let plan = request_upload_plan(account, &target)?;
    let encoded = encode_remote_attachment(bytes);
    upload_bytes(&plan, &encoded)?;
    commit_upload(account, &plan)?;
    Ok(plan.store_uri)
}

fn detect_image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() >= 24 && &bytes[12..16] == b"IHDR" {
        let width = u32::from_be_bytes(bytes[16..20].try_into().ok()?);
        let height = u32::from_be_bytes(bytes[20..24].try_into().ok()?);
        return (width > 0 && height > 0).then_some((width, height));
    }
    if (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) && bytes.len() >= 10 {
        let width = u16::from_le_bytes(bytes[6..8].try_into().ok()?) as u32;
        let height = u16::from_le_bytes(bytes[8..10].try_into().ok()?) as u32;
        return (width > 0 && height > 0).then_some((width, height));
    }
    if bytes.len() >= 30 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        if &bytes[12..16] == b"VP8X" && bytes.len() >= 30 {
            let width = 1
                + u32::from(bytes[24])
                + (u32::from(bytes[25]) << 8)
                + (u32::from(bytes[26]) << 16);
            let height = 1
                + u32::from(bytes[27])
                + (u32::from(bytes[28]) << 8)
                + (u32::from(bytes[29]) << 16);
            return (width > 0 && height > 0).then_some((width, height));
        }
    }
    if bytes.len() >= 4 && bytes[..2] == [0xff, 0xd8] {
        let mut index = 2usize;
        while index + 3 < bytes.len() {
            if bytes[index] != 0xff {
                index += 1;
                continue;
            }
            while index < bytes.len() && bytes[index] == 0xff {
                index += 1;
            }
            if index >= bytes.len() {
                break;
            }
            let marker = bytes[index];
            index += 1;
            if marker == 0xd8 || marker == 0xd9 || marker == 0x01 {
                continue;
            }
            if index + 1 >= bytes.len() {
                break;
            }
            let segment_length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
            if segment_length < 2 || index + segment_length > bytes.len() {
                break;
            }
            let is_sof = matches!(
                marker,
                0xc0..=0xc3
                    | 0xc5..=0xc7
                    | 0xc9..=0xcb
                    | 0xcd..=0xcf
            );
            if is_sof && segment_length >= 7 {
                let height = u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]) as u32;
                let width = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
                return (width > 0 && height > 0).then_some((width, height));
            }
            index += segment_length;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_official_hex_format() {
        assert_eq!(crc32_hex(b"123456789"), "cbf43926");
    }

    #[test]
    fn parses_native_upload_response_with_data_wrapper() {
        let value = serde_json::json!({
            "data": {
                "upload_hosts": ["upload.example.com"],
                "store_infos": [{
                    "auth": "signed-upload-auth",
                    "store_uri": "tos-cn-i-test/image/frame.png",
                    "uploadHeader": {
                        "x-storage-mode": "fallback",
                        "Specified-Content-Type": "image/png"
                    }
                }],
                "session_key": "session-key"
            }
        });
        let parsed = parse_upload_url_response(&value).unwrap();
        assert_eq!(parsed.upload_host, "upload.example.com");
        assert_eq!(parsed.auth, "signed-upload-auth");
        assert_eq!(parsed.store_uri, "tos-cn-i-test/image/frame.png");
        assert_eq!(parsed.session_key.as_deref(), Some("session-key"));
        assert_eq!(
            parsed.upload_headers.get("x-storage-mode").map(String::as_str),
            Some("fallback")
        );
        assert_eq!(
            parsed
                .upload_headers
                .get("Specified-Content-Type")
                .map(String::as_str),
            Some("image/png")
        );
    }

    #[test]
    fn builds_native_binary_upload_headers() {
        let headers = build_upload_headers(
            "signed-upload-auth",
            "cbf43926",
            &std::collections::BTreeMap::from([(
                "x-storage-mode".to_string(),
                "fallback".to_string(),
            )]),
        );
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("signed-upload-auth"));
        assert_eq!(headers.get("Content-CRC32").map(String::as_str), Some("cbf43926"));
        assert_eq!(
            headers.get("Content-Type").map(String::as_str),
            Some("application/octet-stream")
        );
        assert!(!headers.contains_key("Content-Disposition"));
        assert!(!headers.contains_key("Specified-Content-Type"));
        assert_eq!(headers.get("x-storage-mode").map(String::as_str), Some("fallback"));
    }

    #[test]
    fn native_upload_uses_put_transport() {
        assert_eq!(native_upload_method(), "PUT");
    }

    #[test]
    fn encodes_remote_attachment_with_magic_number_v2() {
        let encoded = encode_remote_attachment(&[0x00, 0xff, 0x42]);
        assert_eq!(&encoded[..8], &[0, 0, 0, 0, 27, 198, 174, 134]);
        assert_eq!(&encoded[8..], &[234, 96, 248]);
    }

    #[test]
    fn native_target_is_a_trae_resource_name() {
        let target = native_target_path();
        assert!(target.ends_with(".trae"));
        assert_eq!(target.len(), 37);
    }

    #[test]
    fn commit_payload_uses_native_store_uri_and_session_key() {
        let payload = build_commit_payload(
            "tos-cn-i-test/image/frame.png",
            Some("session-key"),
        );
        assert_eq!(
            payload,
            serde_json::json!({
                "oids": ["tos-cn-i-test/image/frame.png"],
                "session_key": "session-key",
                "biz_type": "remote_resource"
            })
        );
    }

    #[test]
    fn image_upload_payload_matches_native_image_contract() {
        let target = image_target_path(182, 221);
        assert!(target.ends_with("_182x221.trae"));
        assert_eq!(target.len(), 32 + 1 + "182x221".len() + ".trae".len());
        assert_eq!(
            build_image_upload_payload(&target, 182, 221),
            serde_json::json!({
                "targets": [target],
                "biz_type": "image",
                "scale_param": {"width": 182, "height": 221}
            })
        );
    }

    #[test]
    fn image_upload_uses_raw_bytes_and_padded_crc32() {
        assert_eq!(crc32_padded_hex(b"123456789"), "cbf43926");
        assert_eq!(crc32_padded_hex(&[0]), "d202ef8d");
        let headers = build_image_upload_headers(
            "signed-upload-auth",
            "0000000a",
            "image/jpeg",
            &std::collections::BTreeMap::new(),
        );
        assert_eq!(headers.get("Authorization").map(String::as_str), Some("signed-upload-auth"));
        assert_eq!(headers.get("Content-CRC32").map(String::as_str), Some("0000000a"));
        assert_eq!(headers.get("Content-Type").map(String::as_str), Some("image/jpeg"));
    }

    #[test]
    fn detects_dimensions_for_native_image_upload() {
        let png = [
            0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, // signature
            0, 0, 0, 13, b'I', b'H', b'D', b'R', // IHDR
            0, 0, 0, 182, 0, 0, 0, 221, // width/height
        ];
        assert_eq!(detect_image_dimensions(&png), Some((182, 221)));
    }
}
