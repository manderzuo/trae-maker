//! Evidence-bound capabilities for continuation, independent of normal generation.
#[derive(Debug, serde::Serialize, PartialEq)]
pub(crate) struct VideoCapabilities {
    pub tail_reference: bool,
    pub native_first_frame: bool,
    pub native_video_extend: bool,
    pub contract_version: String,
    pub evidence_digest: String,
}

#[derive(serde::Deserialize)]
struct CapabilityConfig {
    provider: String,
    client_version: String,
    plugin_version: String,
    contract_version: String,
    evidence_digest: String,
}

fn unverified() -> VideoCapabilities {
    VideoCapabilities { tail_reference: false, native_first_frame: false,
        native_video_extend: false, contract_version: "not_verified".into(), evidence_digest: String::new() }
}

pub(crate) fn load(data_dir: &std::path::Path) -> Result<VideoCapabilities, String> {
    use sha2::{Digest, Sha256};
    let Some(config_bytes)=read_small(&data_dir.join("video-capabilities.json")) else { return Ok(unverified()); };
    let Ok(config)=serde_json::from_slice::<CapabilityConfig>(&config_bytes) else { return Ok(unverified()); };
    if config.provider!="trae_native" || config.client_version!=super::IDE_VERSION
        || config.plugin_version!="1.0.1" || config.contract_version!="tail-reference-v1" {
        return Ok(unverified());
    }
    let Some(evidence)=read_small(&data_dir.join("video-continuation-evidence.json")) else {return Ok(unverified());};
    if format!("{:x}",Sha256::digest(&evidence))!=config.evidence_digest {return Ok(unverified());}
    let Ok(proof)=serde_json::from_slice::<serde_json::Value>(&evidence) else {return Ok(unverified());};
    let date_valid=proof["verified_at"].as_str().and_then(|s|chrono::NaiveDate::parse_from_str(s,"%Y-%m-%d").ok())
        .is_some_and(|date|date<=chrono::Utc::now().date_naive());
    let tail=date_valid && proof["output_semantics"]=="new_segment"
        && proof["parameter_mapping"]["tail_reference"]=="image_asset_ids"
        && proof["verified_modes"].as_array().is_some_and(|a|a.iter().any(|v|v=="tail_reference"));
    if !tail {return Ok(unverified());}
    // Native modes have no implemented TRAE contract. Configuration cannot
    // turn an unknown/ignored upstream field into a verified native feature.
    Ok(VideoCapabilities {tail_reference:true,native_first_frame:false,native_video_extend:false,
        contract_version:config.contract_version,evidence_digest:config.evidence_digest})
}

fn read_small(path:&std::path::Path)->Option<Vec<u8>> {
    use std::io::Read;
    let file=std::fs::File::open(path).ok()?;
    let mut bytes=Vec::new(); file.take(64*1024+1).read_to_end(&mut bytes).ok()?;
    (bytes.len()<=64*1024).then_some(bytes)
}

pub(crate) async fn get(axum::extract::State(state):axum::extract::State<std::sync::Arc<super::ApiSharedState>>) -> axum::response::Response {
    use axum::response::IntoResponse;
    let dir=state.data_dir.clone();
    match tokio::task::spawn_blocking(move||load(&dir)).await {
        Ok(Ok(capabilities))=>axum::Json(capabilities).into_response(),
        _=>(axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(serde_json::json!({"error":{"code":"capability_read_unavailable"}}))).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    fn fixture() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("video-capabilities-{}-{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
    fn evidence(dir: &std::path::Path, client: &str) {
        let proof = serde_json::json!({"verified_modes":["tail_reference"],"verified_at":"2026-09-28", "parameter_mapping":{"tail_reference":"image_asset_ids"},"output_semantics":"new_segment"});
        let bytes = serde_json::to_vec(&proof).unwrap();
        std::fs::write(dir.join("video-continuation-evidence.json"), &bytes).unwrap();
        let config = serde_json::json!({"provider":"trae_native", "client_version":client,"plugin_version":"1.0.1","contract_version":"tail-reference-v1","evidence_digest":format!("{:x}",Sha256::digest(&bytes))});
        std::fs::write(dir.join("video-capabilities.json"),serde_json::to_vec(&config).unwrap()).unwrap();
    }
    #[test]
    fn capability_unknown_defaults_false() {
        let dir = fixture();
        let result = load(&dir).unwrap();
        assert!(!result.tail_reference && !result.native_first_frame && !result.native_video_extend);
        assert_eq!(result.contract_version, "not_verified");
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn capability_provider_version_mismatch_disables_native() {
        let dir = fixture();
        evidence(&dir, super::super::IDE_VERSION);
        assert!(load(&dir).unwrap().tail_reference);
        evidence(&dir, "unverified-client-upgrade");
        let result=load(&dir).unwrap();
        assert!(!result.tail_reference && !result.native_first_frame && !result.native_video_extend);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn capability_changed_evidence_cannot_enable_continuation() {
        let dir=fixture();
        evidence(&dir, super::super::IDE_VERSION);
        assert!(load(&dir).unwrap().tail_reference);
        std::fs::write(dir.join("video-continuation-evidence.json"), b"{}").unwrap();
        assert!(!load(&dir).unwrap().tail_reference);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
