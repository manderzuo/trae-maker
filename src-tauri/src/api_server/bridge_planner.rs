//! Business input adapter. Clients cannot supply account, hold or price evidence.
use serde_json::{json,Value};
use std::collections::HashSet;
use super::{bridge_runtime::BridgeBudgetRuntime,bridge_prepared::PreparedBudget,pool::PickedAccount};

#[derive(Clone,serde::Deserialize,serde::Serialize,PartialEq)]
#[serde(deny_unknown_fields)]
pub(super) struct PrepareRequest {
    pub wire_version:u8,pub parent_request_id:String,pub request_id:String,pub core_key_id:String,
    pub request_fingerprint:String,pub endpoint:String,pub model:String,pub step_kind:String,pub body:Value,
}
pub(super) trait PreparationSource {
    fn select(&self,video:bool,excluded:&HashSet<String>)->Result<PickedAccount,String>;
    fn capacity(&self,account:&PickedAccount)->Result<Vec<Value>,String>;
    fn estimate(&self,account:&PickedAccount,workload:i64)->Result<i64,String>;
    fn policy(&self,profile:&str,now:i64)->Result<(i64,String,i64),String>;
    fn normalize(&self,account:&PickedAccount,body:&Value,video:bool,key:&str)->Result<Value,String>;
}
pub(super) struct NativePreparationSource<'a>(pub &'a super::ApiSharedState,pub &'a str);
impl PreparationSource for NativePreparationSource<'_> {
    fn select(&self,video:bool,excluded:&HashSet<String>)->Result<PickedAccount,String> {
        self.0.pool.pick_excluding_for(excluded,if video {super::pool::ResourceKind::Work} else {super::pool::ResourceKind::General})
            .ok_or("upstream_account_unavailable".into())
    }
    fn capacity(&self,account:&PickedAccount)->Result<Vec<Value>,String> {
        crate::commands::accounts::query_ent_packs(&account.jwt,&crate::models::DeviceEntry {device_id:account.device_id.clone(),..Default::default()})
            .map_err(|_|"upstream_capacity_unavailable".into())
    }
    fn estimate(&self,account:&PickedAccount,workload:i64)->Result<i64,String> {
        let item=format!("estimate-{:032x}",rand::random::<u128>());
        let url=format!("{}/api/v1/commercial/check_estimated_balance",super::AGENT_HOST);
        let response=ureq::AgentBuilder::new().timeout(std::time::Duration::from_secs(20)).build().post(&url)
            .set("content-type","application/json").set("accept","application/json").set("user-agent","TraeClient/TTNet")
            .set("x-ide-token",account.jwt.strip_prefix("Cloud-IDE-JWT ").unwrap_or(&account.jwt).trim())
            .set("x-app-id",super::APP_ID).set("x-app-version","default").set("x-app-version-code",super::IDE_VERSION_CODE)
            .set("x-ide-version",super::IDE_VERSION).set("x-ide-version-code",super::IDE_VERSION_CODE).set("x-ide-version-type","stable")
            .set("x-device-type","windows").set("x-device-brand","H610E-B").set("x-device-cpu","Intel")
            .set("x-device-id",&account.device_id).set("x-machine-id",&account.machine_id).set("x-os-version","Windows 10 Pro")
            .set("request-traffic-type","prod").set("package-type","stable_cn").set("x-lgw-req-sdk-type","3")
            .set("x-lscbd-aid","787976").set("x-lscbd-platform","windows").set("x-ss-dp","787976").set("app-version",super::IDE_VERSION)
            .set("x-request-id",&item).set("referer",&url)
            .send_json(json!({"access_type":"SoloLite","items":[{"item_id":item,"estimated_token_usage":{"completion_tokens":workload},
                "config_name":"seedance2-fast","function":"video_generation"}]}))
            .map_err(|_|"native_estimate_unavailable")?;
        let value:Value=response.into_json().map_err(|_|"native_estimate_invalid")?;
        parse_native_estimate(&value,&item)
    }
    fn policy(&self,profile:&str,now:i64)->Result<(i64,String,i64),String> {
        // This is administrator-owned local configuration, never a request field.
        // No policy is silently manufactured for an unsupported specification.
        let path=self.0.data_dir.join("bridge-budget-policy.json");
        let metadata=std::fs::metadata(&path).map_err(|_|"budget_policy_unconfigured")?;
        if metadata.len()>128*1024 {return Err("budget_policy_invalid".into());}
        let value:Value=serde_json::from_slice(&std::fs::read(path).map_err(|_|"budget_policy_unavailable")?).map_err(|_|"budget_policy_invalid")?;
        parse_risk_policy(&value,profile,now)
    }
    fn normalize(&self,account:&PickedAccount,body:&Value,video:bool,_key:&str)->Result<Value,String> {
        if video {
            // Core verifies user ownership before upload. The upstream asset
            // copy is owned by the authenticated bridge credential, not by the
            // opaque Core Key mirror used only for execution/billing identity.
            let value=super::video::prepare_native_request(&self.0.data_dir,self.1,body,account).map_err(|_|"reference_preparation_failed")?;
            Ok(super::video::build_request_body(&value))
        } else {
            let sanitized=super::payload::sanitize_scheduler_chat_body(body);
            let bytes=serde_json::to_vec(&sanitized).map_err(|_|"chat_normalization_failed")?;
            let bytes=super::payload::prepare_llm_chat_body_with_conversation(&bytes,&self.0.default_model,&account.uid,&account.device_id,&account.machine_id,None);
            let mut normalized:Value=serde_json::from_slice(&bytes).map_err(|_|"chat_normalization_failed")?;
            // The legacy converter unconditionally writes 4096. Enforce the
            // budgeted helper limit in the final, persisted upstream payload.
            normalized["max_tokens"]=body["max_tokens"].clone();
            normalized["prompt_max_tokens"]=json!(32768);
            Ok(normalized)
        }
    }
}
fn chat_profile(input:&Value,model:&str,kind:&str)->Result<(Value,String),String> {
    let mut body=super::payload::sanitize_scheduler_chat_body(input);
    if !body["messages"].as_array().is_some_and(|m|!m.is_empty()) {return Err("invalid_budget_business_request".into());}
    let mut text_only=body.clone();let mut images=0usize;let mut image_bytes=0usize;
    for message in text_only["messages"].as_array_mut().unwrap() {
        if let Some(parts)=message["content"].as_array_mut() {for part in parts {
            if part["type"]!="image_url" {continue;}
            let url=part.pointer("/image_url/url").and_then(Value::as_str).ok_or("invalid_chat_image")?;
            let valid=if url.starts_with("data:image/") {url.contains(";base64,")} else {
                url.len()<=2048 && url::Url::parse(url).is_ok_and(|u|u.scheme()=="https" && u.host_str().is_some() && u.username().is_empty() && u.password().is_none())
            };
            if !valid {return Err("invalid_chat_image".into());}
            images+=1;image_bytes=image_bytes.checked_add(url.len()).ok_or("budget_chat_input_too_large")?;
            if images>10 || image_bytes>6*1024*1024 {return Err("budget_chat_input_too_large".into());}
            part["image_url"]["url"]=json!("[image]");
        }}
    }
    if kind=="assist" && images!=0 {return Err("invalid_chat_image".into());}
    if serde_json::to_vec(&text_only).map_err(|_|"invalid chat input")?.len()>64*1024 {
        return Err("budget_chat_input_too_large".into());
    }
    body["model"]=json!(model);body["max_tokens"]=json!(if kind=="assist" {1024} else {4096});
    if kind=="assist" {body.as_object_mut().unwrap().remove("tools");body.as_object_mut().unwrap().remove("tool_choice");}
    let profile=if images==0 {format!("{kind}:{model}")} else {format!("{kind}:{model}:images{images}")};
    Ok((body,profile))
}
fn parse_native_estimate(value:&Value,item:&str)->Result<i64,String> {
    if value["code"].as_i64()!=Some(0) {return Err("native_estimate_rejected".into());}
    let rows=value["results"].as_array().ok_or("native_estimate_invalid")?;
    if rows.len()!=1 || rows[0]["item_id"].as_str()!=Some(item) {return Err("native_estimate_binding_mismatch".into());}
    if rows[0]["sufficient"].as_bool()!=Some(true) {return Err("upstream_capacity_insufficient".into());}
    let amount=micro(&rows[0]["estimated_credits"])?;
    if amount<=0 {return Err("native_estimate_invalid".into());} Ok(amount)
}
fn parse_risk_policy(value:&Value,profile:&str,now:i64)->Result<(i64,String,i64),String> {
    if value["version"].as_u64()!=Some(1) {return Err("budget_policy_invalid".into());}
    let entries=value["profiles"].as_array().ok_or("budget_policy_invalid")?;
    let matches:Vec<_>=entries.iter().filter(|p|p["profile"].as_str()==Some(profile)).collect();
    if matches.len()!=1 {return Err("budget_policy_unconfigured".into());}
    let p=matches[0];
    let version=p["policy_version"].as_str().filter(|s|!s.trim().is_empty() && s.len()<=256).ok_or("budget_policy_invalid")?;
    if !p["source"].as_str().is_some_and(|s|!s.trim().is_empty()) {return Err("budget_policy_invalid".into());}
    let expires=p["expires_at_ms"].as_i64().filter(|n|*n>now).ok_or("budget_policy_expired")?;
    let hold=micro(&p["hold_credits"])?;
    if hold<=0 {return Err("budget_policy_invalid".into());}
    // Reference-video price depends on verified reference duration. Until that
    // metadata adapter exists, a count-only profile cannot authorize its spend.
    if profile.contains(":video") && !profile.ends_with(":video0") {return Err("reference_video_budget_metadata_required".into());}
    Ok((hold,version.into(),expires))
}
pub(super) fn prepare(runtime:&BridgeBudgetRuntime,request:&PrepareRequest,source:&dyn PreparationSource,now:i64)->Result<PreparedBudget,String> {
    use super::{bridge_budget::{CapacityEligibility,CapacitySnapshot,CapacityReservation,check_capacity},bridge_prepared::TrustedPreparation};
    use rusqlite::OptionalExtension;
    if request.wire_version!=2 || now<0 || !request.body.is_object() || serde_json::to_vec(&request.body).map_err(|_|"invalid input")?.len()>7*1024*1024
        || [&request.parent_request_id,&request.request_id,&request.core_key_id,&request.request_fingerprint,&request.model].iter()
            .any(|s|s.is_empty() || s.len()>256 || s.trim()!=s.as_str() || s.chars().any(char::is_control)) {
        return Err("invalid_budget_business_request".into());
    }
    let video=request.step_kind=="video";
    if video && (request.endpoint!="videos" || request.model!="seedance" || request.parent_request_id!=request.request_id)
        || !video && (!matches!(request.step_kind.as_str(),"assist"|"chat") || request.endpoint!="chat" || request.model=="seedance") {
        return Err("invalid_budget_business_request".into());
    }
    let _planning=runtime.begin_planning(&request.request_id)?;
    let claim=serde_json::to_value(request).map_err(|_|"invalid input")?;
    if let Some(replay)=runtime.with_store(|store,lease|store.replay_business_preparation(lease,&request.request_id,&claim))? {return Ok(replay);}
    runtime.with_store(|store,lease| {
        if !lease.charge_ready() {return Err("bridge_recovery_required".into());}
        let active:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_api_keys WHERE key_id=?1 AND active=1 AND snapshot_version>0)",[&request.core_key_id],|r|r.get(0)).map_err(|e|e.to_string())?;
        if active {Ok(())} else {Err("core_key_not_active".into())}
    })?;
    let (body,profile,workload)=if video {video_profile(&request.body)?} else {
        let (body,profile)=chat_profile(&request.body,&request.model,&request.step_kind)?;
        (body,profile,None)
    };
    let mut excluded=HashSet::new();
    let mut last_error="upstream_account_unavailable".to_string();
    // Only preparation may try another account. Bound read-side work and never
    // turn a persistence/identity/normalization failure into another attempt.
    for _ in 0..8 {
    let account=match source.select(video,&excluded) {
        Ok(account)=>account,
        Err(error) if error=="upstream_account_unavailable"=>return Err(last_error),
        Err(error)=>return Err(error),
    };
    if !excluded.insert(account.uid.clone()) {return Err("preparation_candidate_repeated".into());}
    let attempt=(|| {
    runtime.with_store(|store,_| {
        let fenced:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_accounts WHERE account_ref=?1 AND rebase_state!='open')",[&account.uid],|r|r.get(0)).map_err(|e|e.to_string())?;
        if fenced {Err("capacity snapshot stale or account fenced".into())} else {Ok(())}
    })?;
    let live=parse_capacity(&source.capacity(&account)?,now)?;
    let (hold,policy,expiry,evidence)=if let Some(workload)=workload {
        let estimate=source.estimate(&account,workload)?;
        let observed=runtime.with_store(|s,_|s.confirmed_profile_high_water(&account.uid,&profile,now.saturating_sub(7*86400*1000)))?;
        (video_hold(estimate,observed)?,"native-estimate-buffer-v1".to_string(),i64::MAX,"native_estimate")
    } else if request.step_kind=="assist" {
        (2_000_000,"bounded-assist-2-credit-risk-v1".into(),i64::MAX,"policy_only")
    } else {
        let (hold,version,expiry)=source.policy(&profile,now)?;
        (hold,version,expiry,"policy_only")
    };
    let expiry=expiry.min(live.valid_until_ms).min(now.checked_add(60_000).ok_or("budget time overflow")?);
    if expiry<=now || hold<=0 {return Err("budget_policy_expired".into());}
    // Reject known insufficient capacity before account-bound asset uploads.
    // Repeat this preflight after uploads; the atomic prepare CAS remains the
    // authority when another request competes between either check and commit.
    let preflight=|store:&mut super::bridge_billing::BridgeBillingStore,lease:&super::bridge_budget_lease::BridgeBudgetLease| {
        let epoch:Option<i64>=store.connection.query_row("SELECT snapshot_epoch FROM bridge_capacity_accounts WHERE account_ref=?1",[&account.uid],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
        let totals=store.capacity_totals(&account.uid)?;
        let promised=totals.pending.checked_add(totals.awaiting_receipt).and_then(|v|v.checked_add(hold)).ok_or("capacity overflow")?;
        let available=if video {live.general.checked_add(live.work).ok_or("capacity overflow")?} else {live.general};
        if promised>available {return Err("upstream_capacity_insufficient".into());}
        let general_promised:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_slots WHERE account_ref=?1 AND stage IN ('P','R') AND eligibility='general')",[&account.uid],|r|r.get(0)).map_err(|e|e.to_string())?;
        if video && general_promised && promised>live.general {return Err("upstream_capacity_insufficient".into());}
        if epoch.is_none() {store.initialize_capacity(lease,&CapacitySnapshot {account_ref:account.uid.clone(),snapshot_ref:format!("native-entitlements:{now}"),epoch:1,
            general:live.general,work:live.work,observed_at_ms:now})?;}
        check_capacity(&store.connection,&CapacityReservation {budget_id:request.request_id.clone(),core_key_id:request.core_key_id.clone(),
            account_ref:account.uid.clone(),snapshot_epoch:epoch.unwrap_or(1),eligibility:if video {CapacityEligibility::GeneralOrWork} else {CapacityEligibility::GeneralOnly},hold})?;
        Ok(epoch.unwrap_or(1))
    };
    runtime.with_store(|store,lease|preflight(store,lease))?;
    let normalized=source.normalize(&account,&body,video,&request.core_key_id)?;
    runtime.with_store(|store,lease| {
        let epoch=preflight(store,lease)?;
        store.prepare_budget(lease,&TrustedPreparation {parent_request_id:request.parent_request_id.clone(),request_id:request.request_id.clone(),core_key_id:request.core_key_id.clone(),
            request_fingerprint:request.request_fingerprint.clone(),endpoint:request.endpoint.clone(),model:request.model.clone(),step_kind:request.step_kind.clone(),
            account_ref:account.uid.clone(),snapshot_epoch:epoch,eligibility:if video {CapacityEligibility::GeneralOrWork} else {CapacityEligibility::GeneralOnly},
            policy_version:policy,pricing_profile_key:profile.clone(),evidence_level:evidence.into(),hold_microcredits:hold,expires_at_ms:expiry,
            body:json!({"business_claim":claim,"upstream":normalized})},None,now)
    })
    })();
    match attempt {
        Ok(prepared)=>return Ok(prepared),
        Err(error) if matches!(error.as_str(),"upstream_capacity_insufficient"|"upstream_capacity_unavailable"|"capacity snapshot stale or account fenced")=>last_error=error,
        Err(error)=>return Err(error),
    }
    }
    Err(last_error)
}

#[derive(Debug,PartialEq)]
pub(super) struct LiveCapacity { pub general:i64,pub work:i64,pub valid_until_ms:i64 }
fn micro(value:&Value)->Result<i64,String> {
    let text=match value {Value::String(s)=>s.clone(),Value::Number(n)=>n.to_string(),_=>return Err("invalid credit amount".into())};
    aiwork_core::CreditAmount::parse(&text,"credits").map(|v|v.as_microcredits())
}
pub(super) fn parse_capacity(packs:&[Value],now:i64)->Result<LiveCapacity,String> {
    if now<0 {return Err("invalid capacity observation time".into());}
    let mut live=LiveCapacity {general:0,work:0,valid_until_ms:now.checked_add(60_000).ok_or("capacity time overflow")?};
    // The 60s age bound applies independently at preparation. valid_until tracks
    // entitlement expiry, not freshness, so the caller can bind both separately.
    live.valid_until_ms=i64::MAX;
    for pack in packs {
        let base=&pack["entitlement_base_info"];
        let product=base["product_id"].as_i64();
        if !matches!(product,Some(208|209)) || base["quota"]["credits_limit"].is_null() {continue;}
        let time=|v:&Value|->Result<Option<i64>,String> {
            if v.is_null() {Ok(None)} else {v.as_i64().filter(|n|*n>=0).map(Some).ok_or("invalid entitlement time".into())}
        };
        let start=time(&base["start_time"])?;
        let end=[time(&base["end_time"])?,time(&pack["expire_time"])?].into_iter().flatten().filter(|v|*v>0).min();
        if start.is_some_and(|v|v>now/1000) || end.is_some_and(|v|v<=now/1000) {continue;}
        let remaining=micro(&base["quota"]["credits_limit"])? .saturating_sub(micro(&pack["usage"]["credits_amount"])?).max(0);
        let slot=if product==Some(209) {&mut live.work} else {&mut live.general};
        *slot=slot.checked_add(remaining).ok_or("capacity overflow")?;
        if remaining>0 {if let Some(end)=end {live.valid_until_ms=live.valid_until_ms.min(end.checked_mul(1000).ok_or("expiry overflow")?);}}
    }
    live.general.checked_add(live.work).ok_or("capacity overflow")?;
    Ok(live)
}
pub(super) fn video_profile(body:&Value)->Result<(Value,String,Option<i64>),String> {
    let mut body=body.clone();
    let obj=body.as_object_mut().ok_or("invalid video input")?;
    // Only supported native business fields cross this boundary. In particular
    // mode/session/account/price selectors can never override the server plan.
    for key in obj.keys() {
        if !matches!(key.as_str(),"model"|"prompt"|"duration"|"resolution"|"ratio"|"image_urls"|"video_urls"|"image_asset_ids"|"video_asset_ids"|"watermark") {
            return Err("unsupported video field".into());
        }
    }
    if !obj.get("prompt").and_then(Value::as_str).is_some_and(|s|!s.trim().is_empty() && s.len()<=32_000) {return Err("invalid video prompt".into());}
    if obj.get("model").is_some_and(|v|!matches!(v.as_str(),Some("seedance"|"seedance2-fast"))) {return Err("unsupported video model".into());}
    obj.insert("model".into(),json!("seedance"));
    obj.entry("duration").or_insert(json!(4));obj.entry("resolution").or_insert(json!("480p"));obj.entry("ratio").or_insert(json!("16:9"));
    let duration=body["duration"].as_i64().filter(|n|(4..=15).contains(n)).ok_or("unsupported video duration")?;
    let resolution=body["resolution"].as_str().filter(|s|matches!(*s,"480p"|"720p")).ok_or("unsupported video resolution")?;
    let ratio=body["ratio"].as_str().filter(|s|matches!(*s,"16:9"|"9:16"|"1:1"|"4:3"|"3:4"|"21:9")).ok_or("unsupported video ratio")?;
    let count=|key:&str|->Result<usize,String> {
        if body[key].is_null() {return Ok(0);}
        let list=body[key].as_array().ok_or("invalid reference list")?;
        if list.len()>10 || list.iter().any(|v|!v.as_str().is_some_and(|s|!s.is_empty() && s.len()<=2048)) {return Err("invalid reference list".into());} Ok(list.len())
    };
    let images=count("image_urls")?+count("image_asset_ids")?;
    let videos=count("video_urls")?+count("video_asset_ids")?;
    if images>10 || videos>10 {return Err("too many references".into());}
    // Native workload has only been captured for these exact unreferenced
    // profiles. Other specifications require explicit local risk policy.
    let workload=if resolution=="720p" && ratio=="16:9" && images==0 && videos==0 {
        match duration {10=>Some(225000),15=>Some(337500),_=>None}
    } else {None};
    let profile=format!("seedance2-fast:{resolution}:{ratio}:{duration}s:images{images}:video{videos}");
    Ok((body,profile,workload))
}
pub(super) fn video_hold(estimate:i64,observed:i64)->Result<i64,String> {
    if estimate<0 || observed<0 || estimate.max(observed)==0 {return Err("invalid video estimate".into());}
    let value=i128::from(estimate.max(observed))*11;
    // 10% risk buffer, rounded upward to one credit. Not an upstream guarantee.
    let credits=(value+9_999_999)/10_000_000;
    i64::try_from(credits*1_000_000).map_err(|_|"video budget overflow".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pack(product:i64,limit:&str,used:&str,start:i64,end:i64)->Value {
        json!({"entitlement_base_info":{"product_id":product,"quota":{"credits_limit":limit},"start_time":start},
            "usage":{"credits_amount":used},"expire_time":end})
    }
    #[test]
    fn chat_vision_budget_counts_images_without_treating_base64_as_text() {
        let image=format!("data:image/png;base64,{}","A".repeat(100_000));
        let body=json!({"messages":[{"role":"user","content":[{"type":"text","text":"describe"},{"type":"image_url","image_url":{"url":image}}]}],"tools":[{"type":"function","function":{"name":"download"}}]});
        let (normalized,profile)=chat_profile(&body,"vision-model","chat").unwrap();
        assert_eq!(profile,"chat:vision-model:images1");assert_eq!(normalized["messages"],body["messages"]);assert_eq!(normalized["tools"],body["tools"]);
        assert_eq!(chat_profile(&json!({"messages":[{"role":"user","content":"hello"}]}),"text-model","chat").unwrap().1,"chat:text-model");
        assert!(chat_profile(&body,"vision-model","assist").is_err(),"bounded text assist budget must not authorize images");
        let huge=json!({"messages":[{"role":"user","content":"x".repeat(65_536)}]});assert!(chat_profile(&huge,"model","chat").is_err());
        let invalid=json!({"messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"file:///private.png"}}]}]});assert!(chat_profile(&invalid,"model","chat").is_err());
    }
    #[test]
    fn capacity_excludes_expired_future_unknown_and_preserves_microcredits() {
        let packs=vec![pack(208,"10.123456","0.000001",0,200),pack(209,"20.000001","0.5",0,300),
            pack(208,"999","0",0,99),pack(208,"999","0",150,300),pack(999,"999","0",0,300)];
        assert_eq!(parse_capacity(&packs,100_000).unwrap(),LiveCapacity {general:10_123_455,work:19_500_001,valid_until_ms:200_000});
        let mut bad=pack(208,"10","0",0,200);bad["usage"]=json!({});
        assert!(parse_capacity(&[bad],100_000).is_err(),"missing use cannot imply zero spent");
    }
    #[test]
    fn profile_normalizes_defaults_but_never_promotes_inferred_workload() {
        let (body,profile,work)=video_profile(&json!({"model":"seedance","prompt":"cat"})).unwrap();
        assert_eq!(body["duration"],4);assert_eq!(body["resolution"],"480p");
        assert_eq!(profile,"seedance2-fast:480p:16:9:4s:images0:video0");assert_eq!(work,None);
        for (duration,expected) in [(5,None),(10,Some(225000)),(15,Some(337500))] {
            assert_eq!(video_profile(&json!({"model":"seedance","prompt":"cat","duration":duration,"resolution":"720p"})).unwrap().2,expected);
        }
        assert_eq!(video_profile(&json!({"model":"seedance","prompt":"cat","duration":10,"resolution":"720p","image_asset_ids":["asset-1"]})).unwrap().2,None);
        for body in [json!({"prompt":"cat","duration":3}),json!({"prompt":"cat","resolution":"4k"}),
            json!({"prompt":"cat","duration":5.5}),json!({"prompt":"cat","session_id":"override"}),json!({"prompt":"cat","account_ref":"user-chosen"})] {
            assert!(video_profile(&body).is_err());
        }
    }
    #[test]
    fn video_budget_rounds_up_buffered_max_not_key_balance() {
        assert_eq!(video_hold(249_750_000,240_759_200).unwrap(),275_000_000);
        assert_eq!(video_hold(374_625_200,0).unwrap(),413_000_000);
        assert_eq!(video_hold(40_000_000,56_208_000).unwrap(),62_000_000);
        assert!(video_hold(0,0).is_err());assert!(video_hold(i64::MAX,0).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn prepare_skips_unusable_accounts_before_creating_a_budget() {
        use std::sync::atomic::{AtomicUsize,Ordering};
        struct Source { calls:AtomicUsize, normalized:AtomicUsize, mode:u8 }
        impl PreparationSource for Source {
            fn select(&self,_:bool,excluded:&HashSet<String>)->Result<PickedAccount,String> {
                let index=self.calls.fetch_add(1,Ordering::SeqCst);
                if index>1 {return Err("upstream_account_unavailable".into());}
                assert_eq!(excluded.contains("unusable"),index>0);
                Ok(PickedAccount {uid:if index==0 {"unusable"} else {"ready"}.into(),jwt:"fixture".into(),
                    device_id:"device".into(),machine_id:"device".into(),domain:String::new(),enterprise_id:String::new(),global_region:false})
            }
            fn capacity(&self,a:&PickedAccount)->Result<Vec<Value>,String> {
                Ok(vec![pack(208,if a.uid=="unusable" && self.mode==0 {"1"} else {"100"},"0",0,300)])
            }
            fn estimate(&self,a:&PickedAccount,_:i64)->Result<i64,String> {
                if a.uid=="unusable" && self.mode==3 {Err("upstream_capacity_insufficient".into())} else {Ok(40_000_000)}
            }
            fn policy(&self,_:&str,_:i64)->Result<(i64,String,i64),String> {Err("budget_policy_unconfigured".into())}
            fn normalize(&self,a:&PickedAccount,b:&Value,_:bool,_:&str)->Result<Value,String> {
                assert_eq!(a.uid,"ready","preflight must precede account-bound uploads");
                self.normalized.fetch_add(1,Ordering::SeqCst);Ok(b.clone())
            }
        }
        for mode in 0..4 {
            let dir=std::env::temp_dir().join(format!("aiwork-fallback-{:032x}",rand::random::<u128>()));
            let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
            runtime.with_store(|s,l| {
                s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").map_err(|e|e.to_string())?;
                if mode==1 || mode==2 {
                    s.initialize_capacity(l,&super::super::bridge_budget::CapacitySnapshot {
                        account_ref:"unusable".into(),snapshot_ref:"fixture".into(),epoch:1,
                        general:if mode==2 {1} else {100_000_000},work:0,observed_at_ms:1})?;
                    if mode==1 {s.connection.execute_batch("UPDATE bridge_capacity_accounts SET rebase_state='fenced',owner_nonce='fixture'").map_err(|e|e.to_string())?;}
                }
                Ok(())
            }).unwrap();
            let source=Source {calls:AtomicUsize::new(0),normalized:AtomicUsize::new(0),mode};
            let req=PrepareRequest {wire_version:2,parent_request_id:"request-a".into(),request_id:"request-a".into(),core_key_id:"key-a".into(),
                request_fingerprint:"fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),step_kind:"video".into(),
                body:json!({"prompt":"cat","duration":10,"resolution":"720p"})};
            let budget=prepare(&runtime,&req,&source,100_000).expect("another eligible account must be tried");
            assert_eq!(source.calls.load(Ordering::SeqCst),2);
            assert_eq!(source.normalized.load(Ordering::SeqCst),1);
            assert_eq!(prepare(&runtime,&req,&source,100_001).unwrap().dispatch_token,budget.dispatch_token);
            assert_eq!(source.calls.load(Ordering::SeqCst),2,"replay never selects another account");
            runtime.with_store(|s,_| {
                assert_eq!(s.capacity_totals("unusable")?.pending,0);
                assert_eq!(s.capacity_totals("ready")?.pending,44_000_000);Ok(())
            }).unwrap();
            drop(runtime);std::fs::remove_dir_all(dir).unwrap();
        }
    }
    #[cfg(windows)]
    #[test]
    fn prepare_replay_reuses_account_body_token_and_only_reserves_small_hold() {
        use std::sync::atomic::{AtomicUsize,Ordering};
        struct Source {calls:AtomicUsize,low_general:std::sync::atomic::AtomicBool}
        impl PreparationSource for Source {
            fn select(&self,_:bool,excluded:&HashSet<String>)->Result<PickedAccount,String> {if excluded.contains("account") {return Err("upstream_account_unavailable".into());} self.calls.fetch_add(1,Ordering::SeqCst);Ok(PickedAccount {
                uid:"account".into(),jwt:"fixture".into(),device_id:"device".into(),machine_id:"device".into(),
                domain:String::new(),enterprise_id:String::new(),global_region:false})}
            fn capacity(&self,_:&PickedAccount)->Result<Vec<Value>,String> {Ok(vec![pack(208,if self.low_general.load(Ordering::SeqCst) {"1"} else {"100"},"0",0,300),pack(209,"100","0",0,300)])}
            fn estimate(&self,_:&PickedAccount,_:i64)->Result<i64,String> {Ok(40_000_000)}
            fn policy(&self,_:&str,_:i64)->Result<(i64,String,i64),String> {Ok((62_000_000,"fixture-policy".into(),300_000))}
            fn normalize(&self,_:&PickedAccount,body:&Value,_:bool,_:&str)->Result<Value,String> {Ok(body.clone())}
        }
        let dir=std::env::temp_dir().join(format!("aiwork-planner-{:032x}",rand::random::<u128>()));
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        runtime.with_store(|s,_| {s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").map_err(|e|e.to_string())}).unwrap();
        let source=Source {calls:AtomicUsize::new(0),low_general:std::sync::atomic::AtomicBool::new(false)};
        let mut req=PrepareRequest {wire_version:2,parent_request_id:"request-a".into(),request_id:"request-a".into(),core_key_id:"key-a".into(),
            request_fingerprint:"fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),step_kind:"video".into(),
            body:json!({"model":"seedance","prompt":"cat","duration":5})};
        let first=prepare(&runtime,&req,&source,100_000).unwrap();
        assert_eq!(first.authorization.hold_credits.as_microcredits(),62_000_000);
        assert_eq!(first.evidence_level,"policy_only");
        assert_eq!(prepare(&runtime,&req,&source,101_000).unwrap().dispatch_token,first.dispatch_token);
        assert_eq!(source.calls.load(Ordering::SeqCst),1,"replay must precede network and uploads");
        req.body["duration"]=json!(10);assert!(prepare(&runtime,&req,&source,102_000).is_err());
        assert_eq!(source.calls.load(Ordering::SeqCst),1,"identity conflict must not contact upstream");
        req.request_id="request-b".into();req.parent_request_id="request-b".into();req.body["resolution"]=json!("720p");
        let second=prepare(&runtime,&req,&source,103_000).unwrap();
        assert_eq!(second.authorization.hold_credits.as_microcredits(),44_000_000);
        assert_eq!(second.evidence_level,"native_estimate");
        runtime.with_store(|s,_| {assert_eq!(s.capacity_totals("account")?.pending,106_000_000);Ok(())}).unwrap();
        let mut forgery=serde_json::to_value(&req).unwrap();forgery["hold_microcredits"]=json!(1);
        assert!(serde_json::from_value::<PrepareRequest>(forgery).is_err());
        runtime.with_store(|s,l| {s.cancel_budget(l,&first,104_000)?;s.cancel_budget(l,&second,104_000)?;Ok(())}).unwrap();
        let mut chat=req.clone();chat.request_id="request-assist".into();chat.parent_request_id="request-parent".into();
        chat.endpoint="chat".into();chat.model="deepseek-v4-flash".into();chat.step_kind="assist".into();chat.body=json!({"messages":[{"role":"user","content":"hi"}]});
        prepare(&runtime,&chat,&source,105_000).unwrap();
        source.low_general.store(true,Ordering::SeqCst);
        req.request_id="request-c".into();req.parent_request_id="request-c".into();
        assert!(prepare(&runtime,&req,&source,106_000).is_err(),"expired general capacity cannot be backed by video-only credits when chat is still promised");
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
}
