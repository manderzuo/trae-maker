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
        || config.plugin_version!="1.0.1" || !matches!(config.contract_version.as_str(),"tail-reference-v1"|"video-reference-continuation-v1") {
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
    let native_proof=&proof["native_video_extend_evidence"];
    let digest=|field:&str|native_proof[field].as_str().is_some_and(|s|s.len()==64&&s.bytes().all(|b|b.is_ascii_hexdigit()));
    let native=date_valid && config.contract_version=="video-reference-continuation-v1"
        && proof["output_semantics"]=="new_segment"
        && proof["parameter_mapping"]["native_video_extend"]=="video_asset_ids"
        && proof["verified_modes"].as_array().is_some_and(|a|a.iter().any(|v|v=="native_video_extend"))
        && native_proof["financial_state"]=="settled"
        && ["source_request_id","continuation_request_id"].iter().all(|k|native_proof[*k].as_str().is_some_and(super::bridge_billing::valid_request_id))
        && native_proof["source_request_id"]!=native_proof["continuation_request_id"]
        && digest("source_video_sha256")&&digest("continuation_video_sha256")
        && native_proof["actual_video_credits"].as_str().and_then(|v|aiwork_core::CreditAmount::parse(v,"credits").ok()).is_some_and(|v|v.as_microcredits()>0);
    if !tail && !native {return Ok(unverified());}
    // Video-reference continuation produces a NEW segment. This contract does
    // not claim byte-preserving prefix extension or strict first-frame locking.
    Ok(VideoCapabilities {tail_reference:tail,native_first_frame:false,native_video_extend:native,
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
    fn full_video_contract_requires_mapping_settled_proof_and_never_unlocks_first_frame() {
        let dir=fixture();
        let mut proof=serde_json::json!({"verified_modes":["tail_reference","native_video_extend"],"verified_at":"2026-09-29","parameter_mapping":{"tail_reference":"image_asset_ids","native_video_extend":"video_asset_ids"},"output_semantics":"new_segment","native_video_extend_evidence":{"source_request_id":"request-source","continuation_request_id":"request-child","source_video_sha256":"ab".repeat(32),"continuation_video_sha256":"cd".repeat(32),"financial_state":"settled","actual_video_credits":"112.9696"}});
        let save=|value:&serde_json::Value| {
            let bytes=serde_json::to_vec(value).unwrap();
            std::fs::write(dir.join("video-continuation-evidence.json"),&bytes).unwrap();
            let config=serde_json::json!({"provider":"trae_native","client_version":super::super::IDE_VERSION,"plugin_version":"1.0.1","contract_version":"video-reference-continuation-v1","evidence_digest":format!("{:x}",Sha256::digest(&bytes))});
            std::fs::write(dir.join("video-capabilities.json"),serde_json::to_vec(&config).unwrap()).unwrap();
        };
        save(&proof);let result=load(&dir).unwrap();
        assert!(result.native_video_extend && result.tail_reference);assert!(!result.native_first_frame);
        proof["native_video_extend_evidence"]["financial_state"]=serde_json::json!("pending");save(&proof);assert!(!load(&dir).unwrap().native_video_extend);
        proof["native_video_extend_evidence"]["financial_state"]=serde_json::json!("settled");proof["parameter_mapping"]["native_video_extend"]=serde_json::json!("first_frame");save(&proof);assert!(!load(&dir).unwrap().native_video_extend);
        std::fs::remove_dir_all(dir).unwrap();
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
