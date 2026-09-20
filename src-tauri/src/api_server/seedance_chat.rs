use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InlineImage {
    pub mime_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SeedanceChatProjection {
    pub video_input: Value,
    pub inline_images: Vec<InlineImage>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SeedanceChatError {
    code: &'static str,
    message: String,
}

impl SeedanceChatError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self { code, message: message.into() }
    }

    pub(crate) fn code(&self) -> &'static str {
        self.code
    }

    #[allow(dead_code)]
    pub(crate) fn message(&self) -> &str {
        &self.message
    }
}

pub(crate) fn is_seedance_model(model: &str) -> bool {
    model.trim().eq_ignore_ascii_case("seedance")
}

pub(crate) fn project_chat_to_video(body: &Value) -> Result<SeedanceChatProjection, SeedanceChatError> {
    let object = body
        .as_object()
        .ok_or_else(|| SeedanceChatError::new("seedance_prompt_required", "Chat 请求必须是 JSON 对象"))?;
    let messages = object
        .get("messages")
        .and_then(Value::as_array)
        .filter(|messages| !messages.is_empty())
        .ok_or_else(|| SeedanceChatError::new("seedance_prompt_required", "messages 必须是非空数组"))?;

    let mut prompt = None;
    let mut inline_images = Vec::new();
    for message in messages.iter().rev() {
        let Some(message_object) = message.as_object() else { continue };
        let role = message_object
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !role.trim().eq_ignore_ascii_case("user") {
            continue;
        }
        let content = message_object.get("content").ok_or_else(|| {
            SeedanceChatError::new("seedance_prompt_required", "最后一条 user 消息缺少 content")
        })?;
        let text = extract_user_content(content, &mut inline_images)?;
        if !text.trim().is_empty() {
            prompt = Some(text.trim().to_string());
        }
        break;
    }
    let prompt = prompt.ok_or_else(|| SeedanceChatError::new(
        "seedance_prompt_required",
        "至少需要一条包含文字提示词的 user 消息",
    ))?;

    let mut video_input = Map::new();
    video_input.insert("model".into(), Value::String("seedance".into()));
    video_input.insert("prompt".into(), Value::String(prompt));
    for field in [
        "duration",
        "resolution",
        "ratio",
        "image_asset_ids",
        "video_asset_ids",
    ] {
        if let Some(value) = object.get(field) {
            video_input.insert(field.into(), value.clone());
        }
    }

    Ok(SeedanceChatProjection {
        video_input: Value::Object(video_input),
        inline_images,
    })
}

fn extract_user_content(
    content: &Value,
    inline_images: &mut Vec<InlineImage>,
) -> Result<String, SeedanceChatError> {
    match content {
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text_parts = Vec::new();
            for part in parts {
                let Some(part_object) = part.as_object() else {
                    return Err(SeedanceChatError::new(
                        "seedance_unsupported_content_part",
                        "Seedance Chat content part 必须是对象",
                    ));
                };
                match part_object.get("type").and_then(Value::as_str).unwrap_or_default() {
                    "text" => {
                        let text = part_object
                            .get("text")
                            .and_then(Value::as_str)
                            .ok_or_else(|| SeedanceChatError::new(
                                "seedance_unsupported_content_part",
                                "text content part 缺少 text 字段",
                            ))?;
                        text_parts.push(text.to_string());
                    }
                    "image_url" => {
                        let url = part_object
                            .get("image_url")
                            .and_then(Value::as_object)
                            .and_then(|image| image.get("url"))
                            .and_then(Value::as_str)
                            .ok_or_else(|| SeedanceChatError::new(
                                "seedance_inline_image_invalid",
                                "image_url content part 缺少 url 字段",
                            ))?;
                        inline_images.push(parse_inline_image(url)?);
                    }
                    other => {
                        return Err(SeedanceChatError::new(
                            "seedance_unsupported_content_part",
                            format!("不支持的 Seedance Chat content part: {other}"),
                        ));
                    }
                }
            }
            Ok(text_parts.join("\n"))
        }
        _ => Err(SeedanceChatError::new(
            "seedance_unsupported_content_part",
            "Seedance Chat content 必须是字符串或 content parts 数组",
        )),
    }
}

fn parse_inline_image(url: &str) -> Result<InlineImage, SeedanceChatError> {
    let Some(rest) = url.strip_prefix("data:") else {
        return Err(SeedanceChatError::new(
            "seedance_inline_image_invalid",
            "参考图只接受 data URL；公网 URL 请先上传到 /v1/assets",
        ));
    };
    let (metadata, encoded) = rest.split_once(',').ok_or_else(|| {
        SeedanceChatError::new("seedance_inline_image_invalid", "data URL 格式无效")
    })?;
    let mime_type = metadata
        .split(';')
        .next()
        .filter(|value| matches!(*value, "image/png" | "image/jpeg" | "image/webp"))
        .ok_or_else(|| {
            SeedanceChatError::new(
                "seedance_inline_image_invalid",
                "参考图仅支持 image/png、image/jpeg 或 image/webp",
            )
        })?;
    if !metadata.split(';').any(|value| value.eq_ignore_ascii_case("base64")) {
        return Err(SeedanceChatError::new(
            "seedance_inline_image_invalid",
            "参考图 data URL 必须使用 base64 编码",
        ));
    }
    let bytes = STANDARD.decode(encoded.trim()).map_err(|_| {
        SeedanceChatError::new("seedance_inline_image_invalid", "参考图 Base64 内容无效")
    })?;
    if bytes.is_empty() {
        return Err(SeedanceChatError::new(
            "seedance_inline_image_invalid",
            "参考图内容不能为空",
        ));
    }
    Ok(InlineImage { mime_type: mime_type.to_string(), bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_seedance_case_insensitively_after_trimming() {
        assert!(is_seedance_model(" Seedance "));
        assert!(is_seedance_model("SEEDANCE"));
        assert!(!is_seedance_model("seedance-pro"));
    }

    #[test]
    fn projects_last_user_message_without_calling_a_text_model() {
        let input = serde_json::json!({
            "model": " Seedance ",
            "messages": [
                {"role": "system", "content": "ignore"},
                {"role": "user", "content": "先前提示"},
                {"role": "assistant", "content": "历史回复"},
                {"role": "user", "content": [
                    {"type": "text", "text": "主体"},
                    {"type": "text", "text": "动作"}
                ]}
            ],
            "duration": 6,
            "resolution": "720p",
            "ratio": "16:9",
            "image_asset_ids": ["asset-image"],
            "video_asset_ids": ["asset-video"]
        });
        let projection = project_chat_to_video(&input).unwrap();
        assert_eq!(projection.video_input["model"], "seedance");
        assert_eq!(projection.video_input["prompt"], "主体\n动作");
        assert_eq!(projection.video_input["duration"], 6);
        assert_eq!(projection.video_input["resolution"], "720p");
        assert_eq!(projection.video_input["ratio"], "16:9");
        assert_eq!(projection.video_input["image_asset_ids"], serde_json::json!(["asset-image"]));
        assert_eq!(projection.video_input["video_asset_ids"], serde_json::json!(["asset-video"]));
    }

    #[test]
    fn rejects_missing_messages_and_empty_user_prompt() {
        assert!(project_chat_to_video(&serde_json::json!({"model": "seedance"})).is_err());
        assert!(project_chat_to_video(&serde_json::json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": "   "}]
        }))
        .is_err());
    }

    #[test]
    fn rejects_unsupported_content_parts_instead_of_dropping_them() {
        let error = project_chat_to_video(&serde_json::json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [{"type": "audio", "audio": {}}]}]
        }))
        .unwrap_err();
        assert_eq!(error.code(), "seedance_unsupported_content_part");
    }

    #[test]
    fn records_inline_images_for_the_asset_stage_without_fetching_them() {
        let input = serde_json::json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "让画面动起来"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AA=="}}
            ]}]
        });
        let projection = project_chat_to_video(&input).unwrap();
        assert_eq!(projection.inline_images.len(), 1);
        assert_eq!(projection.inline_images[0].mime_type, "image/png");
        assert_eq!(projection.video_input.get("image_asset_ids"), None);
    }

    #[test]
    fn rejects_remote_image_urls_at_the_projection_boundary() {
        let error = project_chat_to_video(&serde_json::json!({
            "model": "seedance",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "动画"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png"}}
            ]}]
        }))
        .unwrap_err();
        assert_eq!(error.code(), "seedance_inline_image_invalid");
    }
}
