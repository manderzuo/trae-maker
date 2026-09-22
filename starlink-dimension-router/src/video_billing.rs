use aiwork_core::{canonical_json_hash, CoreError, CoreStore, Principal, VideoBillingMode};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoAdmission {
    Paused,
    DiagnosticClaimed,
    Active,
}

#[derive(Debug, thiserror::Error)]
pub enum VideoBillingError {
    #[error("读取视频计费控制失败: {0}")]
    Core(#[from] CoreError),
}

pub fn request_hash(model: &str, body: &Value) -> String {
    hex::encode(canonical_json_hash(&json!({
        "endpoint": "videos",
        "model": model,
        "body": body,
    })))
}

pub fn admit_video_request(
    store: &CoreStore,
    principal: &Principal,
    model: &str,
    body: &Value,
) -> Result<VideoAdmission, VideoBillingError> {
    let control = store.video_billing_control()?;
    match control.mode {
        VideoBillingMode::Paused => Ok(VideoAdmission::Paused),
        VideoBillingMode::Active => Ok(VideoAdmission::Active),
        VideoBillingMode::DiagnosticOnce => {
            let hash = request_hash(model, body);
            if store.claim_video_diagnostic(&principal.key_id, &hash)?.is_some() {
                Ok(VideoAdmission::DiagnosticClaimed)
            } else {
                Ok(VideoAdmission::Paused)
            }
        }
    }
}
