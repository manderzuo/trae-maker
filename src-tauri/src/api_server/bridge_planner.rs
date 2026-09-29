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
    fn reference_seconds(&self,body:&Value)->Result<Option<u64>,String> {
        if ["video_urls","video_asset_ids"].iter().any(|k|body[k].as_array().is_some_and(|v|!v.is_empty())) {
            Err("reference_video_budget_metadata_required".into())
        } else {Ok(None)}
    }
}
pub(super) struct NativePreparationSource<'a>(pub &'a super::ApiSharedState,pub &'a str);
impl PreparationSource for NativePreparationSource<'_> {
    fn reference_seconds(&self,body:&Value)->Result<Option<u64>,String> {
        super::bridge_reference::owned_reference_seconds(&self.0.data_dir,self.1,body)
    }
    fn select(&self,video:bool,excluded:&HashSet<String>)->Result<PickedAccount,String> {
        self.0.pool.pick_excluding_for(excluded,if video {super::pool::ResourceKind::Work} else {super::pool::ResourceKind::General})
            .ok_or("upstream_account_unavailable".into())
    }
    fn capacity(&self,account:&PickedAccount)->Result<Vec<Value>,String> {
        crate::commands::accounts::query_ent_packs_for_bridge(&account.jwt,&crate::models::DeviceEntry {device_id:account.device_id.clone(),..Default::default()})
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
        let metadata=match std::fs::metadata(&path) {
            Ok(value)=>value,
            Err(error) if error.kind()==std::io::ErrorKind::NotFound=>return default_chat_policy(profile),
            Err(_)=>return Err("budget_policy_unavailable".into()),
        };
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
            if !body["model"].as_str().is_some_and(|m|super::payload::model_config(m).is_some()) {
                return Err("invalid_budget_business_request".into());
            }
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
/// Release-owned per-model risk allowances, not prices or maximum upstream bills.
/// chat_profile bounds text to 64 KiB, output to 4096 tokens and references to
/// ten images / 6 MiB. Only named, supported Trae adapters are eligible. Actual
/// receipts still settle each request; unknowns retain their existing hold.
fn default_chat_policy(profile:&str)->Result<(i64,String,i64),String> {
    let model=profile.strip_prefix("chat:").ok_or("budget_policy_unconfigured")?;
    let model=if let Some((model,count))=model.rsplit_once(":images") {
        if !count.parse::<usize>().is_ok_and(|n|(1..=10).contains(&n)) {return Err("budget_policy_unconfigured".into());}
        model
    } else {model};
    let (canonical,_)=super::payload::model_config(model).ok_or("budget_policy_unconfigured")?;
    if !super::models_sync::default_models().iter().any(|m|m.id.eq_ignore_ascii_case(canonical)) {
        return Err("budget_policy_unconfigured".into());
    }
    Ok((10_000_000,format!("bounded-chat-10-credit-v1:{}",canonical.to_ascii_lowercase()),i64::MAX))
}

fn parse_risk_policy(value:&Value,profile:&str,now:i64)->Result<(i64,String,i64),String> {
    if value["version"].as_u64()!=Some(1) {return Err("budget_policy_invalid".into());}
    let entries=value["profiles"].as_array().ok_or("budget_policy_invalid")?;
    // Catalog/dispatch model IDs are ASCII-case insensitive. Apply the same
    // contract to policy lookup, retaining the exact-one-match ambiguity guard.
    let matches:Vec<_>=entries.iter().filter(|p|p["profile"].as_str().is_some_and(|p|p.eq_ignore_ascii_case(profile))).collect();
    if matches.is_empty() {
        if video_spec(profile).is_some() {return neighboring_policy(value,profile,now).or_else(|error|missing_video_fallback(value,profile,now,error));}
        return default_chat_policy(profile);
    }
    if matches.len()!=1 {return Err("budget_policy_unconfigured".into());}
    let p=matches[0];
    let version=p["policy_version"].as_str().filter(|s|!s.trim().is_empty() && s.len()<=256).ok_or("budget_policy_invalid")?;
    if !p["source"].as_str().is_some_and(|s|!s.trim().is_empty()) {return Err("budget_policy_invalid".into());}
    let expires=p["expires_at_ms"].as_i64().ok_or("budget_policy_invalid")?;
    if expires<=now {
        return if video_spec(profile).is_some() {neighboring_policy(value,profile,now).or_else(|e|missing_video_fallback(value,profile,now,if e=="budget_policy_unconfigured" {"budget_policy_expired".into()} else {e}))} else {Err("budget_policy_expired".into())};
    }
    let hold=micro(&p["hold_credits"])?;
    if hold<=0 {return Err("budget_policy_invalid".into());}
    // This profile suffix is produced by the server's immutable local media
    // inspection, never accepted from client JSON or the language model.
    if profile.contains(":video") && !profile.ends_with(":video0")
        && !profile.rsplit_once(":ref_seconds").is_some_and(|(_,s)|s.parse::<u64>().is_ok_and(|v|(1..=60).contains(&v))) {
        return Err("reference_video_budget_metadata_required".into());
    }
    Ok((hold,version.into(),expires))
}

/// Administrator-owned temporary risk allowance, never an upstream price ceiling.
/// Keep the request's actual profile; the marker identifies only how its hold was chosen.
fn missing_video_fallback(value:&Value,profile:&str,now:i64,error:String)->Result<(i64,String,i64),String> {
    if !matches!(error.as_str(),"budget_policy_unconfigured"|"budget_policy_expired") {return Err(error);}
    if video_spec(profile).is_none() {return Err(error);}
    let p=&value["video_fallback"];
    if p.is_null() || p["enabled"]==false {return Err(error);}
    if p["enabled"]!=true || p["model_family"]!="seedance2-fast" || p["baseline_resolution"]!="720p" || p["baseline_duration_seconds"]!=15 {
        return Err("budget_policy_invalid".into());
    }
    let hold=p["hold_microcredits"].as_i64().filter(|n|*n>0).ok_or("budget_policy_invalid")?;
    let version=p["policy_version"].as_str().filter(|s|s.trim()==*s && !s.is_empty() && s.len()<=96 && !s.chars().any(char::is_control)).ok_or("budget_policy_invalid")?;
    if !p["source"].as_str().is_some_and(|s|!s.trim().is_empty() && s.len()<=512 && !s.chars().any(char::is_control)) {return Err("budget_policy_invalid".into());}
    let expiry=p["expires_at_ms"].as_i64().ok_or("budget_policy_invalid")?;
    if expiry<=now {return Err("budget_policy_expired".into());}
    Ok((hold,format!("fallback-risk-720p15s-v1:{version}"),expiry))
}

fn video_spec(profile:&str)->Option<(String,String,i64)> {
    let fields:Vec<_>=profile.splitn(6,':').collect();
    if fields.len()!=6 || fields[0]!="seedance2-fast" || !matches!(fields[1],"480p"|"720p") || fields[2]!="16" || fields[3]!="9" {return None;}
    let duration=fields[4].strip_suffix('s')?.parse::<i64>().ok().filter(|d|(4..=15).contains(d))?;
    let reference:Vec<_>=fields[5].split(':').collect();
    if !(2..=3).contains(&reference.len()) {return None;}
    let images=reference[0].strip_prefix("images")?.parse::<u32>().ok()?;
    let videos=reference[1].strip_prefix("video")?.parse::<u32>().ok()?;
    if images>10 || videos>10 || (videos==0 && reference.len()!=2) {return None;}
    if videos>0 && (reference.len()!=3 || !reference[2].strip_prefix("ref_seconds").and_then(|s|s.parse::<u32>().ok()).is_some_and(|s|(1..=60).contains(&s))) {return None;}
    Some((fields[..4].join(":"),fields[5].into(),duration))
}
fn reference_cover(target:&str,candidate:&str)->Option<i64> {
    let split=|s:&str|s.rsplit_once(":ref_seconds").and_then(|(class,seconds)|seconds.parse::<i64>().ok().map(|n|(class.to_owned(),n)));
    if target==candidate {return Some(0);}
    let (class,seconds)=split(target)?;let (other,total)=split(candidate)?;
    (class==other && total>=seconds).then_some(total-seconds)
}
fn scale_hold(amount:i64,source:i64,target:i64)->Result<i64,String> {
    if amount<=0 || source<=0 || target<=0 {return Err("invalid video estimate".into());}
    if target<=source {return Ok(amount);}
    let value=(i128::from(amount)*i128::from(target)+i128::from(source)-1)/i128::from(source);
    i64::try_from(((value+999_999)/1_000_000)*1_000_000).map_err(|_|"video budget overflow".into())
}
fn neighboring_policy(value:&Value,profile:&str,now:i64)->Result<(i64,String,i64),String> {
    let (family,reference,target)=video_spec(profile).ok_or("budget_policy_unconfigured")?;
    let entries=value["profiles"].as_array().ok_or("budget_policy_invalid")?;
    let candidates:Vec<_>=entries.iter().filter_map(|p| {
        let name=p["profile"].as_str()?;let (f,r,d)=video_spec(name)?;
        let gap=reference_cover(&reference,&r)?;
        (name!=profile && f==family && d>=target && p["expires_at_ms"].as_i64().is_some_and(|t|t>now)).then_some((d,(gap,name)))
    }).collect();
    let (duration,(_,name))=candidates.iter().filter(|(d,_)|*d>=target).min_by_key(|(d,(gap,name))|(*d,*gap,*name))
        .ok_or("budget_policy_unconfigured")?;
    if entries.iter().filter(|p|p["profile"].as_str().is_some_and(|p|p.eq_ignore_ascii_case(name))).count()!=1 {
        return Err("budget_policy_invalid".into());
    }
    let (amount,version,expiry)=parse_risk_policy(value,name,now)?;
    let version:String=version.chars().take(96).collect();
    Ok((scale_hold(amount,*duration,target)?,format!("neighbor-risk-v1:{duration}s:{version}"),expiry))
}
fn observed_video_budget(prices:&[(String,i64)],profile:&str)->Result<Option<(i64,String,&'static str)>,String> {
    if let Some((_,amount))=prices.iter().find(|(p,_)|p==profile) {
        return Ok(Some((video_hold(*amount,0)?,"observed-price-buffer-v1".into(),"observed_actual")));
    }
    let Some((family,reference,target))=video_spec(profile) else {return Ok(None)};
    let candidates:Vec<_>=prices.iter().filter_map(|(p,amount)| {
        let (f,r,d)=video_spec(p)?;let gap=reference_cover(&reference,&r)?;
        (f==family && d>=target).then_some((d,(gap,*amount)))
    }).collect();
    let Some((duration,(_,amount)))=candidates.iter().filter(|(d,_)|*d>=target).min_by_key(|(d,(gap,amount))|(*d,*gap,-*amount))
        else {return Ok(None)};
    Ok(Some((video_hold(scale_hold(*amount,*duration,target)?,0)?,format!("observed-neighbor-risk-v1:{duration}s"),"policy_only")))
}
fn fallback_video_budget(source:&dyn PreparationSource,profile:&str,now:i64,observed:Option<(i64,String,&'static str)>)->Result<(i64,String,i64,&'static str),String> {
    match source.policy(profile,now) {
        Ok((_,version,_)) if version.starts_with("fallback-risk-720p15s-v1:") && observed.is_some()=> {
            let (hold,version,evidence)=observed.unwrap();Ok((hold,version,i64::MAX,evidence))
        },
        Ok((hold,version,expiry))=>Ok((hold,version,expiry,"policy_only")),
        Err(error) if matches!(error.as_str(),"budget_policy_unconfigured"|"budget_policy_expired")=> {
            observed.map(|(hold,version,evidence)|(hold,version,i64::MAX,evidence)).ok_or(error)
        },
        Err(error)=>Err(error),
    }
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
    let (body,mut profile,workload)=if video {video_profile(&request.body)?} else {
        let (body,profile)=chat_profile(&request.body,&request.model,&request.step_kind)?;
        (body,profile,None)
    };
    if video {if let Some(seconds)=source.reference_seconds(&body)? {profile.push_str(&format!(":ref_seconds{seconds}"));}}
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
    let packs=source.capacity(&account)?;
    let live=parse_capacity(&packs,now)?;
    let observation=if super::bridge_capacity_source::dedicated_enabled(runtime.data_dir(),&account.uid)? {
        Some(super::bridge_capacity_source::PackObservation::parse(&packs,now)?)
    } else {None};
    if observation.is_some() {
        // A top-up with no outstanding debit has no receipt-triggered worker.
        // Re-read under a quiescent fence before accepting the larger capacity.
        // Failure preserves the conservative snapshot; regular preflight below
        // can still admit a request within its already verified capacity.
        let _=super::bridge_capacity_source::reconcile_increased_capacity(runtime,&account.uid,live.general,live.work,||source.capacity(&account));
    }
    let (hold,policy,expiry,evidence)=if video {
        let prices=runtime.with_store(|s,_|s.confirmed_video_prices(&account.uid,now.saturating_sub(7*86400*1000),now))?;
        let observed=observed_video_budget(&prices,&profile)?;
        if let Some((hold,version,"observed_actual"))=observed.as_ref() {
            (*hold,version.clone(),i64::MAX,"observed_actual")
        } else if let Some(workload)=workload {
            match source.estimate(&account,workload) {
                Ok(estimate)=>(video_hold(estimate,0)?,"native-estimate-buffer-v1".to_string(),i64::MAX,"native_estimate"),
                Err(error) if error=="native_estimate_unavailable"=>fallback_video_budget(source,&profile,now,observed)?,
                Err(error)=>return Err(error),
            }
        } else {fallback_video_budget(source,&profile,now,observed)?}
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
        if epoch.is_none() {store.initialize_capacity_source(lease,&CapacitySnapshot {account_ref:account.uid.clone(),snapshot_ref:format!("native-entitlements:{now}"),epoch:1,
            general:live.general,work:live.work,observed_at_ms:now},observation.as_ref())?;}
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
        if !matches!(product,Some(208|209|221)) || base["quota"]["credits_limit"].is_null() {continue;}
        let time=|v:&Value|->Result<Option<i64>,String> {
            if v.is_null() {Ok(None)} else {v.as_i64().filter(|n|*n>=0).map(Some).ok_or("invalid entitlement time".into())}
        };
        let start=time(&base["start_time"])?;
        let end=[time(&base["end_time"])?,time(&pack["expire_time"])?].into_iter().flatten().filter(|v|*v>0).min();
        if start.is_some_and(|v|v>now/1000) || end.is_some_and(|v|v<=now/1000) {continue;}
        if pack["usage"].as_object().is_some_and(|m|m.is_empty()) {continue;}
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
    let workload=if resolution=="720p" && images==0 && videos==0 {
        match duration {10=>Some(225000),15=>Some(337500),_=>None}
    } else {None};
    // Aspect ratio is an execution parameter, not a pricing dimension for
    // Seedance Fast 480p/720p. Keep the legacy 16:9 policy namespace so existing
    // calibrated duration/resolution budgets and receipts remain usable. The
    // actual body retains the requested ratio. Reference classes stay separate.
    let profile=format!("seedance2-fast:{resolution}:16:9:{duration}s:images{images}:video{videos}");
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
    fn fallback_policy() -> Value {
        json!({"version":1,"profiles":[],"video_fallback":{"enabled":true,"model_family":"seedance2-fast","baseline_resolution":"720p","baseline_duration_seconds":15,"hold_microcredits":413_000_000,"policy_version":"fixture","source":"test risk allowance","expires_at_ms":200}})
    }
    #[test]
    fn fallback_covers_missing_specs_without_changing_generation_parameters() {
        let policy=fallback_policy();
        for resolution in ["480p","720p"] {for duration in 4..=15 {for ratio in ["16:9","9:16","1:1"] {
            let (body,profile,_)=video_profile(&json!({"prompt":"cat","duration":duration,"resolution":resolution,"ratio":ratio,"video_asset_ids":["owned-video"]})).unwrap();
            let quote=parse_risk_policy(&policy,&format!("{profile}:ref_seconds11"),100).unwrap();
            assert_eq!(quote.0,413_000_000);assert!(quote.1.starts_with("fallback-risk-720p15s-v1:"));
            assert_eq!(body["duration"],duration);assert_eq!(body["resolution"],resolution);assert_eq!(body["ratio"],ratio);
        }}}
        for profile in ["other:720p:16:9:10s:images0:video0","seedance2-fast:1080p:16:9:10s:images0:video0","seedance2-fast:720p:16:9:16s:images0:video0","seedance2-fast:720p:16:9:10s:images0:video1"] {
            assert!(parse_risk_policy(&policy,profile,100).is_err());
        }
    }
    #[test]
    fn fallback_never_bypasses_disabled_expired_invalid_or_ambiguous_policy() {
        let profile="seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds11";
        for (field,bad) in [("enabled",json!(false)),("expires_at_ms",json!(99)),("hold_microcredits",json!(0)),("baseline_duration_seconds",json!(10)),("source",json!(""))] {
            let mut p=fallback_policy();p["video_fallback"][field]=bad;
            assert!(parse_risk_policy(&p,profile,100).is_err(),"invalid {field}");
        }
        let mut p=fallback_policy();let entry=json!({"profile":profile,"hold_credits":"10","policy_version":"exact","source":"fixture","expires_at_ms":200});
        p["profiles"]=json!([entry.clone(),entry]);assert!(parse_risk_policy(&p,profile,100).is_err());
        let mut neighbor=p["profiles"][0].clone();neighbor["profile"]=json!("seedance2-fast:720p:16:9:15s:images0:video1:ref_seconds15");
        p["profiles"]=json!([neighbor.clone(),neighbor]);assert!(parse_risk_policy(&p,profile,100).is_err(),"ambiguous covering neighbors must not silently fall back");
    }
    #[test]
    fn longer_reference_neighbor_covers_but_shorter_reference_does_not() {
        let mut p=fallback_policy();p["profiles"]=json!([
            {"profile":"seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds6","hold_credits":"100","policy_version":"short","source":"fixture","expires_at_ms":200},
            {"profile":"seedance2-fast:720p:16:9:15s:images0:video1:ref_seconds15","hold_credits":"300","policy_version":"long","source":"fixture","expires_at_ms":200}]);
        let q=parse_risk_policy(&p,"seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds11",100).unwrap();assert_eq!(q.0,300_000_000);
        p["profiles"].as_array_mut().unwrap().pop();
        assert_eq!(parse_risk_policy(&p,"seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds11",100).unwrap().0,413_000_000);
        p["profiles"]=json!([{"profile":"seedance2-fast:720p:16:9:5s:images0:video1:ref_seconds15","hold_credits":"100","policy_version":"short-output","source":"fixture","expires_at_ms":200}]);
        assert_eq!(parse_risk_policy(&p,"seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds11",100).unwrap().0,413_000_000);
    }
    #[test]
    fn shorter_output_price_is_not_a_covering_quote() {
        let mut p=fallback_policy();p["profiles"]=json!([{"profile":"seedance2-fast:720p:16:9:5s:images0:video0","hold_credits":"100","policy_version":"short","source":"fixture","expires_at_ms":200}]);
        assert_eq!(parse_risk_policy(&p,"seedance2-fast:720p:16:9:10s:images0:video0",100).unwrap().0,413_000_000);
        assert!(observed_video_budget(&[("seedance2-fast:720p:16:9:5s:images0:video0".into(),100_000_000)],"seedance2-fast:720p:16:9:10s:images0:video0").unwrap().is_none());
    }
    #[test]
    fn missing_video_spec_uses_longer_neighbor_without_crossing_reference_class() {
        let policies=json!({"version":1,"profiles":[
            {"profile":"seedance2-fast:720p:16:9:5s:images0:video0","hold_credits":"133","policy_version":"five","source":"calibrated","expires_at_ms":200},
            {"profile":"seedance2-fast:720p:16:9:10s:images0:video0","hold_credits":"280","policy_version":"ten","source":"calibrated","expires_at_ms":200}
        ]});
        let quote=parse_risk_policy(&policies,"seedance2-fast:720p:16:9:7s:images0:video0",100).unwrap();
        assert_eq!(quote.0,280_000_000);
        assert!(quote.1.contains("neighbor"));
        assert!(parse_risk_policy(&policies,"seedance2-fast:720p:16:9:12s:images0:video0",100).is_err());
        for other in ["seedance2-fast:480p:16:9:7s:images0:video0","seedance2-fast:720p:16:9:7s:images1:video0",
            "seedance2-fast:720p:16:9:7s:images0:video1:ref_seconds6"] {
            assert!(parse_risk_policy(&policies,other,100).is_err());
        }
        assert!(parse_risk_policy(&policies,"seedance2-fast:720p:16:9:7s:images0:video0",201).is_err());
    }
    #[test]
    fn observed_neighbor_prices_preserve_reference_class_and_buffer_only_the_hold() {
        let prices=vec![("seedance2-fast:720p:16:9:5s:images1:video0".into(),120_000_000),
            ("seedance2-fast:720p:16:9:10s:images1:video0".into(),250_000_000)];
        let exact=observed_video_budget(&prices,"seedance2-fast:720p:16:9:5s:images1:video0").unwrap().unwrap();
        assert_eq!((exact.0,exact.2),(132_000_000,"observed_actual"));
        let neighbor=observed_video_budget(&prices,"seedance2-fast:720p:16:9:7s:images1:video0").unwrap().unwrap();
        assert_eq!((neighbor.0,neighbor.2),(275_000_000,"policy_only"));
        assert!(observed_video_budget(&prices,"seedance2-fast:720p:16:9:12s:images1:video0").unwrap().is_none());
        for other in ["seedance2-fast:480p:16:9:7s:images1:video0","seedance2-fast:720p:16:9:7s:images0:video0",
            "seedance2-fast:720p:16:9:7s:images1:video1:ref_seconds6"] {
            assert!(observed_video_budget(&prices,other).unwrap().is_none());
        }
    }
    #[cfg(windows)]
    #[test]
    fn finalized_video_price_survives_restart_and_prepares_next_task_without_exact_policy() {
        use std::sync::atomic::{AtomicBool,Ordering};
        struct Source {known:AtomicBool}
        impl PreparationSource for Source {
            fn select(&self,_:bool,_:&HashSet<String>)->Result<PickedAccount,String> {Ok(PickedAccount {uid:"account".into(),jwt:"fixture".into(),device_id:"d".into(),machine_id:"d".into(),domain:String::new(),enterprise_id:String::new(),global_region:false})}
            fn capacity(&self,_:&PickedAccount)->Result<Vec<Value>,String> {Ok(vec![pack(208,"1000","0",0,4102444800)])}
            fn estimate(&self,_:&PickedAccount,_:i64)->Result<i64,String> {Err("native_estimate_unavailable".into())}
            fn policy(&self,p:&str,now:i64)->Result<(i64,String,i64),String> {if self.known.load(Ordering::SeqCst) {let mut policy=fallback_policy();policy["video_fallback"]["expires_at_ms"]=json!(i64::MAX);parse_risk_policy(&policy,p,now)} else {Err("budget_policy_unconfigured".into())}}
            fn normalize(&self,_:&PickedAccount,b:&Value,_:bool,_:&str)->Result<Value,String> {Ok(b.clone())}
            fn reference_seconds(&self,_:&Value)->Result<Option<u64>,String> {Ok(Some(11))}
        }
        let dir=std::env::temp_dir().join(format!("aiwork-calibrated-price-{:032x}",rand::random::<u128>()));
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        runtime.with_store(|s,_| {s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").map_err(|e|e.to_string())}).unwrap();
        let now=chrono::Utc::now().timestamp_millis();
        let mut req=PrepareRequest {wire_version:2,parent_request_id:"first".into(),request_id:"first".into(),core_key_id:"key-a".into(),request_fingerprint:"first-fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),step_kind:"video".into(),body:json!({"prompt":"cat","duration":10,"resolution":"720p","ratio":"9:16","video_asset_ids":["owned-video"]})};
        let source=Source {known:AtomicBool::new(true)};
        let first=prepare(&runtime,&req,&source,now-7000).unwrap();
        assert_eq!(first.authorization.hold_credits.as_microcredits(),413_000_000);
        assert_eq!(first.evidence_level,"policy_only");
        runtime.with_store(|s,l| {
            use super::super::{bridge_prepared::ConsumeOutcome,bridge_execution::ExecutionState};
            let ConsumeOutcome::Granted(ctx)=s.consume_budget(l,&first,now-6000)? else {return Err("consume".into())};
            assert_eq!(ctx.body["upstream"],video_profile(&req.body)?.0);
            s.mark_budget_send_intent(l,&first.authorization.budget_id,&ctx.consume_epoch)?;
            s.bind_budget_task(l,&first.authorization.budget_id,"video-fixture")?;
            s.finish_budget_result(l,&first.authorization.budget_id,ExecutionState::Succeeded,&json!({"status":"completed"}),now+1000)?;
            super::super::bridge_receipts::tests::cache(&dir,s,&[(&first.authorization.budget_id,"456")],2000);
            s.confirm_budget_usage(l,&first.authorization.budget_id)?;Ok(())
        }).unwrap();
        runtime.begin_close();drop(runtime);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();source.known.store(false,Ordering::SeqCst);
        req.request_id="next".into();req.parent_request_id="next".into();req.request_fingerprint="next-fingerprint".into();req.body["ratio"]=json!("1:1");
        let next=prepare(&runtime,&req,&source,now+4000).expect("a verified final sample must replace missing precise policy after restart");
        assert_eq!(next.authorization.hold_credits.as_microcredits(),502_000_000);
        assert_eq!(next.evidence_level,"observed_actual");
        runtime.with_store(|s,_| {
            assert_eq!(s.confirmed_video_prices("account",now-10000,now+4000)?,vec![("seedance2-fast:720p:16:9:10s:images0:video1:ref_seconds11".into(),456_000_000)]);
            assert!(s.confirmed_video_prices("different-account",now-10000,now+4000)?.is_empty());
            s.connection.execute("UPDATE bridge_budget_receipts SET receipt_state='conflict' WHERE budget_id=?1",[&first.authorization.budget_id]).map_err(|e|e.to_string())?;
            assert!(s.confirmed_video_prices("account",now-10000,now+4000)?.is_empty(),"conflicted receipts must immediately stop calibrating future holds");
            Ok(())
        }).unwrap();
        runtime.begin_close();drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn advertised_chat_models_have_bounded_model_specific_budget_without_calibration_entries() {
        let empty=json!({"version":1,"profiles":[]});
        for model in super::super::models_sync::default_models() {
            let profile=format!("chat:{}",model.id);
            let (hold,version,expiry)=parse_risk_policy(&empty,&profile,100).unwrap();
            assert_eq!(hold,10_000_000);
            assert!(version.contains(&model.id.to_ascii_lowercase()));
            assert!(expiry>100);
        }
        let image=parse_risk_policy(&empty,"chat:Doubao-Seed-2.1-Pro:images2",100).unwrap();
        assert_eq!(image.0,10_000_000);
        for unknown in ["chat:unknown-model","chat:DeepSeek-V4-Flash:images0","chat:DeepSeek-V4-Flash:images11",
            "seedance2-fast:720p:16:9:6s:images0:video0"] {
            assert!(parse_risk_policy(&empty,unknown,100).is_err());
        }
        let override_policy=json!({"version":1,"profiles":[{"profile":"chat:GLM-5.3-Flash","hold_credits":"3","policy_version":"admin-glm","source":"admin risk cap","expires_at_ms":200}]});
        assert_eq!(parse_risk_policy(&override_policy,"chat:glm-5.3-flash",100).unwrap().0,3_000_000);
        assert_eq!(parse_risk_policy(&override_policy,"chat:deepseek-v4-flash",100).unwrap().0,10_000_000);
        assert!(parse_risk_policy(&override_policy,"chat:glm-5.3-flash",201).is_err(),"expired explicit policy must not silently widen to defaults");
    }
    #[test]
    fn risk_policy_matches_catalog_model_case_but_rejects_ambiguous_duplicates() {
        let mut policies=json!({"version":1,"profiles":[{"profile":"chat:deepseek-v4-flash","hold_credits":"10","policy_version":"fixture","source":"bounded test","expires_at_ms":100}]});
        assert_eq!(parse_risk_policy(&policies,"chat:DeepSeek-V4-Flash",1).unwrap().0,10_000_000);
        assert!(parse_risk_policy(&policies,"chat:unknown-model",1).is_err());
        let mut duplicate=policies["profiles"][0].clone();duplicate["profile"]=json!("chat:DeepSeek-V4-Flash");
        policies["profiles"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_risk_policy(&policies,"chat:deepseek-v4-flash",1).is_err(),"case aliases must never select an arbitrary budget");
    }
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
        let mut sparse=pack(208,"150","0",0,200);sparse["usage"]=json!({});
        assert_eq!(parse_capacity(&[pack(221,"500","100",0,200),sparse],100_000).unwrap().general,400_000_000,"monthly credits count but sparse usage must not imply unspent credits");
        let mut bad=pack(208,"10","0",0,200);bad["usage"]=Value::Null;
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
    fn aspect_ratio_preserves_execution_but_shares_calibrated_price() {
        for resolution in ["480p","720p"] {
            for duration in 4..=15 {
                for ratio in ["16:9","9:16","1:1","4:3","3:4","21:9"] {
                    let (body,profile,_)=video_profile(&json!({"prompt":"cat","duration":duration,"resolution":resolution,"ratio":ratio})).unwrap();
                    assert_eq!(body["ratio"],ratio);
                    assert_eq!(profile,format!("seedance2-fast:{resolution}:16:9:{duration}s:images0:video0"));
                }
            }
        }
        let (_,profile,_)=video_profile(&json!({"prompt":"cat","duration":5,"resolution":"720p","ratio":"9:16","image_asset_ids":["asset-1"]})).unwrap();
        assert_eq!(profile,"seedance2-fast:720p:16:9:5s:images1:video0");
    }
    #[test]
    fn video_budget_rounds_up_buffered_max_not_key_balance() {
        assert_eq!(video_hold(249_750_000,240_759_200).unwrap(),275_000_000);
        assert_eq!(video_hold(374_625_200,0).unwrap(),413_000_000);
        assert_eq!(video_hold(40_000_000,56_208_000).unwrap(),62_000_000);
        assert!(video_hold(0,0).is_err());assert!(video_hold(i64::MAX,0).is_err());
    }
    #[test]
    fn reference_policy_requires_server_measured_duration_and_never_uses_count_only_price() {
        let profile="seedance2-fast:480p:16:9:5s:images0:video1:ref_seconds6";
        let policy=json!({"version":1,"profiles":[{"profile":profile,"policy_version":"reference-risk-v1","source":"bounded reference calibration","expires_at_ms":300000,"hold_credits":"160"}]});
        assert_eq!(parse_risk_policy(&policy,profile,100000).unwrap().0,160_000_000);
        for bad in ["seedance2-fast:480p:16:9:5s:images0:video1","seedance2-fast:480p:16:9:5s:images0:video1:ref_seconds0","seedance2-fast:480p:16:9:5s:images0:video1:ref_seconds61"] {
            let mut p=policy.clone();p["profiles"][0]["profile"]=json!(bad);assert!(parse_risk_policy(&p,bad,100000).is_err());
        }
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
        struct Source {calls:AtomicUsize,low_general:std::sync::atomic::AtomicBool,bonus:std::sync::atomic::AtomicBool}
        impl PreparationSource for Source {
            fn select(&self,_:bool,excluded:&HashSet<String>)->Result<PickedAccount,String> {if excluded.contains("account") {return Err("upstream_account_unavailable".into());} self.calls.fetch_add(1,Ordering::SeqCst);Ok(PickedAccount {
                uid:"account".into(),jwt:"fixture".into(),device_id:"device".into(),machine_id:"device".into(),
                domain:String::new(),enterprise_id:String::new(),global_region:false})}
            fn capacity(&self,_:&PickedAccount)->Result<Vec<Value>,String> {
                let mut packs=vec![pack(208,if self.low_general.load(Ordering::SeqCst) {"1"} else {"100"},"0",0,4102444800),pack(209,"100","0",0,4102444800)];
                if self.bonus.load(Ordering::SeqCst) {let mut extra=pack(208,"50","0",0,4102444800);extra["id"]=json!("bonus");packs.push(extra);}
                std::thread::sleep(std::time::Duration::from_millis(2));Ok(packs)
            }
            fn estimate(&self,_:&PickedAccount,_:i64)->Result<i64,String> {Ok(40_000_000)}
            fn policy(&self,_:&str,_:i64)->Result<(i64,String,i64),String> {Ok((62_000_000,"fixture-policy".into(),300_000))}
            fn normalize(&self,_:&PickedAccount,body:&Value,_:bool,_:&str)->Result<Value,String> {Ok(body.clone())}
        }
        let dir=std::env::temp_dir().join(format!("aiwork-planner-{:032x}",rand::random::<u128>()));
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        std::fs::write(dir.join("bridge-dedicated-accounts.json"),serde_json::to_vec(&json!({"version":1,"accounts":["account"]})).unwrap()).unwrap();
        runtime.with_store(|s,_| {s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").map_err(|e|e.to_string())}).unwrap();
        let source=Source {calls:AtomicUsize::new(0),low_general:std::sync::atomic::AtomicBool::new(false),bonus:std::sync::atomic::AtomicBool::new(false)};
        let mut req=PrepareRequest {wire_version:2,parent_request_id:"request-a".into(),request_id:"request-a".into(),core_key_id:"key-a".into(),
            request_fingerprint:"fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),step_kind:"video".into(),
            body:json!({"model":"seedance","prompt":"cat","duration":5})};
        let first=prepare(&runtime,&req,&source,100_000).unwrap();
        runtime.with_store(|s,_| {let n:i64=s.connection.query_row("SELECT COUNT(*) FROM bridge_capacity_anchors WHERE account_ref='account'",[],|r|r.get(0)).unwrap();assert_eq!(n,1,"anchor must precede first paid budget");Ok(())}).unwrap();
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
        source.bonus.store(true,Ordering::SeqCst);
        prepare(&runtime,&chat,&source,105_000).unwrap();
        runtime.with_store(|s,_| {let general:i64=s.connection.query_row("SELECT general_microcredits FROM bridge_capacity_accounts WHERE account_ref='account'",[],|r|r.get(0)).unwrap();assert_eq!(general,150_000_000,"new packs must become usable without an unrelated future debit");Ok(())}).unwrap();
        source.low_general.store(true,Ordering::SeqCst);
        source.bonus.store(false,Ordering::SeqCst);
        req.request_id="request-c".into();req.parent_request_id="request-c".into();
        assert!(prepare(&runtime,&req,&source,106_000).is_err(),"expired general capacity cannot be backed by video-only credits when chat is still promised");
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
}
