//! Entitlement-use evidence for dedicated-account quiescent rebasing.
use serde::{Deserialize,Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use rusqlite::{params,Connection,OptionalExtension,Transaction};
#[cfg(test)]
use rusqlite::TransactionBehavior;
use super::{bridge_billing::BridgeBillingStore,bridge_budget,bridge_budget_lease::BridgeBudgetLease,bridge_rebase::{self,RebaseFence,CoveredCapacity}};

pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[("table","bridge_capacity_anchors","CREATE TABLE bridge_capacity_anchors (
    account_ref TEXT PRIMARY KEY NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
    snapshot_epoch INTEGER NOT NULL CHECK(typeof(snapshot_epoch)='integer' AND snapshot_epoch>=1),
    event_sequence INTEGER NOT NULL CHECK(typeof(event_sequence)='integer' AND event_sequence>=0),
    legacy_sessions INTEGER NOT NULL CHECK(typeof(legacy_sessions)='integer' AND legacy_sessions>=0),
    observation_json TEXT NOT NULL CHECK(length(observation_json) BETWEEN 1 AND 131072)
)")];
fn db_error(e:rusqlite::Error)->String {format!("capacity source database error: {e}")}
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(db_error)?;}Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let actual:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((found,sql))=actual else {return Err(format!("missing capacity source object: {name}"))};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?!=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {return Err(format!("invalid capacity source schema: {name}"));}
    }Ok(())
}

#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PackUse {product:i64,limit:i64,used:i64,start_ms:Option<i64>,end_ms:Option<i64>}
#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PackObservation {pub observed_at_ms:i64,packs:BTreeMap<String,PackUse>}

fn credits(value:Option<&Value>)->Result<i64,String> {
    let text=match value {Some(Value::String(s))=>s.clone(),Some(Value::Number(n))=>n.to_string(),_=>return Err("capacity_source_invalid_credits".into())};
    let amount=aiwork_core::CreditAmount::parse(&text,"credits")?.as_microcredits();
    if amount<0 {return Err("capacity_source_invalid_credits".into());}Ok(amount)
}
fn time_ms(value:Option<&Value>)->Result<Option<i64>,String> {
    match value {
        None|Some(Value::Null)=>Ok(None),
        Some(v)=>{let seconds=v.as_i64().or_else(||v.as_str().and_then(|s|s.parse().ok())).ok_or("capacity_source_invalid_time")?;
            if seconds<0 {return Err("capacity_source_invalid_time".into());}
            Ok(Some(seconds.checked_mul(1000).ok_or("capacity_source_time_overflow")?))}
    }
}
// Canonicalize recursively: JSON object insertion order must not change identity.
fn canonical(value:&Value)->Value {
    match value {
        Value::Object(m)=>Value::Object(m.iter().map(|(k,v)|(k.clone(),canonical(v))).collect::<BTreeMap<_,_>>().into_iter().collect()),
        Value::Array(a)=>Value::Array(a.iter().map(canonical).collect()),_=>value.clone(),
    }
}
fn digest(value:&Value)->Result<String,String> {
    use sha2::{Digest,Sha256};
    let bytes=serde_json::to_vec(&canonical(value)).map_err(|_|"capacity_source_encoding_failed")?;
    Ok(format!("{:x}",Sha256::digest(bytes)))
}

impl PackObservation {
    pub(super) fn parse(packs:&[Value],now:i64)->Result<Self,String> {
        if now<0 || packs.len()>256 || serde_json::to_vec(packs).map_err(|_|"capacity_source_encoding_failed")?.len()>128*1024 {
            return Err("capacity_source_bounds".into());
        }
        let mut observed=BTreeMap::new();
        for raw in packs {
            let base=raw.get("entitlement_base_info").ok_or("capacity_source_missing_base")?;
            let limit=base.get("quota").and_then(|q|q.get("credits_limit"));
            let used=raw.get("usage").and_then(|u|u.get("credits_amount"));
            if limit.is_none() && used.is_none() {continue;}
            let product=base.get("product_id").and_then(Value::as_i64).ok_or("capacity_source_invalid_product")?;
            if !matches!(product,208|209|221) {return Err("capacity_source_unknown_product".into());}
            // Native read-only responses omit usage for sparse daily rewards.
            // Do not invent zero use or authorize their full quota. Once exact
            // usage appears, count the WHOLE observed use as a new-pack debit.
            if used.is_none() && raw.get("usage").and_then(Value::as_object).is_some_and(|m|m.is_empty()) {continue;}
            let start_ms=time_ms(base.get("start_time"))?;
            let end_ms=match (time_ms(base.get("end_time"))?.filter(|v|*v>0),time_ms(raw.get("expire_time"))?.filter(|v|*v>0)) {
                (Some(a),Some(b))=>Some(a.min(b)),(a,b)=>a.or(b),
            };
            let item=PackUse {product,limit:credits(limit)?,used:credits(used)?,start_ms,end_ms};
            let mut identity=raw.as_object().ok_or("capacity_source_invalid_pack")?.clone();
            identity.remove("usage");
            // An explicit pack ID cannot be counted twice merely because quota
            // metadata differs. Where no ID is present, require the complete
            // non-usage projection to remain stable (changes fail closed).
            let key=match base.get("entitlement_id").filter(|v|!v.is_null()).or_else(||raw.get("id")) {
                Some(Value::String(s)) if !s.is_empty() && s.len()<=256=>digest(&serde_json::json!({"id":s}))?,
                Some(Value::Number(n))=>digest(&serde_json::json!({"id":n}))?,
                Some(Value::Null)|None=>digest(&Value::Object(identity))?,
                _=>return Err("capacity_source_invalid_pack_id".into()),
            };
            if observed.insert(key,item).is_some() {return Err("capacity_source_duplicate_pack".into());}
        }
        let observation=Self {observed_at_ms:now,packs:observed};observation.validate()?;Ok(observation)
    }
    fn validate(&self)->Result<(),String> {
        if self.observed_at_ms<0 || self.packs.len()>256 {return Err("capacity_source_bounds".into());}
        for (key,p) in &self.packs {
            if key.len()!=64 || !key.bytes().all(|b|b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                || !matches!(p.product,208|209|221) || p.limit<0 || p.used<0 || p.used>p.limit
                || p.start_ms.is_some_and(|v|v<0) || p.end_ms.is_some_and(|v|v<0)
                || matches!((p.start_ms,p.end_ms),(Some(a),Some(b)) if a>b) {
                return Err("capacity_source_invalid_observation".into());
            }
        }Ok(())
    }
    pub(super) fn proves(&self,previous:&Self,actual:i64)->Result<(),String> {
        self.validate()?;previous.validate()?;
        if self.observed_at_ms<=previous.observed_at_ms || actual<0 {return Err("capacity_source_unproven".into());}
        if previous.packs.keys().any(|key|!self.packs.contains_key(key)) {return Err("capacity_source_missing_pack".into());}
        let mut delta=0i64;
        for (key,next) in &self.packs {
            let prior=if let Some(old)=previous.packs.get(key) {
                if old.product!=next.product || old.limit!=next.limit || old.start_ms!=next.start_ms || old.end_ms!=next.end_ms || next.used<old.used {
                    return Err("capacity_source_pack_changed".into());
                }old.used
            } else {0};
            delta=delta.checked_add(next.used-prior).ok_or("capacity_source_overflow")?;
        }
        if delta!=actual {return Err("capacity_source_debit_mismatch".into());}Ok(())
    }
    pub(super) fn balances(&self)->Result<(i64,i64),String> {
        self.validate()?;let (mut general,mut work)=(0i64,0i64);
        for p in self.packs.values() {
            if p.start_ms.is_some_and(|v|v>self.observed_at_ms) || p.end_ms.is_some_and(|v|v<=self.observed_at_ms) {continue;}
            let target=if p.product==209 {&mut work} else {&mut general};
            *target=target.checked_add(p.limit-p.used).ok_or("capacity_source_overflow")?;
        }
        general.checked_add(work).ok_or("capacity_source_overflow")?;Ok((general,work))
    }
}

type Anchor=(i64,i64,i64,String);
fn anchor(conn:&Connection,account:&str)->Result<Option<Anchor>,String> {
    conn.query_row("SELECT snapshot_epoch,event_sequence,legacy_sessions,observation_json FROM bridge_capacity_anchors WHERE account_ref=?1",[account],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(db_error)
}
pub(super) fn verify_anchor(tx:&Transaction<'_>,fence:&RebaseFence,observation:&PackObservation)->Result<(),String> {
    let (epoch,event,legacy,json)=anchor(tx,&fence.account_ref)?.ok_or("capacity_anchor_missing")?;
    if epoch!=fence.snapshot_epoch || event>fence.event_sequence || legacy!=fence.legacy_sessions {return Err("capacity_anchor_watermark_changed".into());}
    let previous:PackObservation=serde_json::from_str(&json).map_err(|_|"capacity_anchor_invalid")?;
    let snapshot=bridge_rebase::snapshot(tx,&fence.account_ref)?;
    if snapshot.observed_at_ms!=previous.observed_at_ms || previous.balances()? != (snapshot.general,snapshot.work) {return Err("capacity_anchor_snapshot_changed".into());}
    let actual:i64=tx.query_row("SELECT COALESCE(SUM(s.actual_microcredits),0) FROM bridge_capacity_slots s WHERE s.account_ref=?1 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id)",[&fence.account_ref],|r|r.get(0)).map_err(db_error)?;
    observation.proves(&previous,actual)
}
pub(super) fn advance_anchor(tx:&Transaction<'_>,fence:&RebaseFence,observation:&PackObservation)->Result<(),String> {
    let changed=tx.execute("UPDATE bridge_capacity_anchors SET snapshot_epoch=?1,event_sequence=?2,observation_json=?3 WHERE account_ref=?4 AND snapshot_epoch=?5 AND legacy_sessions=?6",
        params![fence.snapshot_epoch.checked_add(1).ok_or("snapshot epoch overflow")?,fence.event_sequence,serde_json::to_string(observation).map_err(|_|"capacity_source_encoding_failed")?,fence.account_ref,fence.snapshot_epoch,fence.legacy_sessions]).map_err(db_error)?;
    if changed!=1 {return Err("capacity_anchor_changed".into());}Ok(())
}
pub(super) fn initialize_anchor_in_tx(tx:&Transaction<'_>,account:&str,observation:&PackObservation)->Result<(),String> {
        let (general,work)=observation.balances()?;
        let encoded=serde_json::to_string(observation).map_err(|_|"capacity_source_encoding_failed")?;
        let snapshot=bridge_rebase::snapshot(&tx,account)?;
        let (event,legacy,latest)=bridge_rebase::quiet_watermark(&tx,account)?;
        let unsafe_state:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_accounts WHERE account_ref=?1 AND rebase_state!='open') OR EXISTS(SELECT 1 FROM bridge_capacity_slots s WHERE s.account_ref=?1 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id))",[account],|r|r.get(0)).map_err(db_error)?;
        if unsafe_state || observation.observed_at_ms<latest || snapshot.observed_at_ms!=observation.observed_at_ms || (general,work)!=(snapshot.general,snapshot.work) {
            return Err("capacity_anchor_initial_snapshot_unproven".into());
        }
        // Legacy receipt finality is not covered by the v2 session policy.
        if legacy!=0 {return Err("capacity_anchor_legacy_activity_unproven".into());}
        if let Some(existing)=anchor(&tx,account)? {
            return if existing==(snapshot.epoch,event,legacy,encoded) {Ok(())} else {Err("capacity_anchor_already_exists".into())};
        }
        tx.execute("INSERT INTO bridge_capacity_anchors(account_ref,snapshot_epoch,event_sequence,legacy_sessions,observation_json) VALUES (?1,?2,?3,?4,?5)",params![account,snapshot.epoch,event,legacy,encoded]).map_err(db_error)?;
        Ok(())
}
impl BridgeBillingStore {
    #[cfg(test)]
    pub(super) fn initialize_capacity_anchor(&mut self,lease:&BridgeBudgetLease,account:&str,observation:&PackObservation)->Result<(),String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        initialize_anchor_in_tx(&tx,account,observation)?;tx.commit().map_err(db_error)
    }
    pub(super) fn commit_dedicated_capacity(&mut self,lease:&BridgeBudgetLease,fence:&RebaseFence,observation:&PackObservation)->Result<(),String> {
        let (general,work)=observation.balances()?;
        let hash=digest(&serde_json::to_value(observation).map_err(|_|"capacity_source_encoding_failed")?)?;
        let coverage=CoveredCapacity {snapshot:bridge_budget::CapacitySnapshot {account_ref:fence.account_ref.clone(),snapshot_ref:format!("dedicated-packs:{hash}"),
            epoch:fence.snapshot_epoch.checked_add(1).ok_or("snapshot epoch overflow")?,general,work,observed_at_ms:observation.observed_at_ms},covered_event_sequence:fence.event_sequence,
            coverage_ref:format!("dedicated-pack-debit-v1:{hash}"),quiescent_since_ms:fence.started_at_ms,external_activity_excluded:true};
        self.commit_capacity_rebase_source(lease,fence,&coverage,Some(observation))
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DedicatedConfig {version:u8,accounts:Vec<String>}
fn dedicated_accounts(data_dir:&std::path::Path)->Result<Vec<String>,String> {
    use std::io::Read;
    let file=match std::fs::File::open(data_dir.join("bridge-dedicated-accounts.json")) {
        Ok(file)=>file,Err(e) if e.kind()==std::io::ErrorKind::NotFound=>return Ok(Vec::new()),Err(_)=>return Err("dedicated_config_unavailable".into()),
    };
    let mut bytes=Vec::new();file.take(32769).read_to_end(&mut bytes).map_err(|_|"dedicated_config_unavailable")?;
    if bytes.len()>32768 {return Err("dedicated_config_invalid".into());}
    let config:DedicatedConfig=serde_json::from_slice(&bytes).map_err(|_|"dedicated_config_invalid")?;
    let mut seen=std::collections::HashSet::new();
    if config.version!=1 || config.accounts.len()>256 || config.accounts.iter().any(|s|s.is_empty() || s.len()>128 || !s.bytes().all(|b|b.is_ascii_alphanumeric() || matches!(b,b'-'|b'_'|b'.')) || !seen.insert(s)) {
        return Err("dedicated_config_invalid".into());
    }Ok(config.accounts)
}
pub(super) fn dedicated_enabled(data_dir:&std::path::Path,account:&str)->Result<bool,String> {Ok(dedicated_accounts(data_dir)?.iter().any(|v|v==account))}
pub(super) fn pending_accounts(data_dir:&std::path::Path)->Result<Vec<(String,i64)>,String> {
    let accounts=dedicated_accounts(data_dir)?;if accounts.is_empty() {return Ok(Vec::new());}
    let store=BridgeBillingStore::open(data_dir)?;
    let mut stmt=store.connection.prepare("SELECT s.account_ref,MIN(a.observed_at_ms) FROM bridge_capacity_slots s JOIN bridge_capacity_accounts a ON a.account_ref=s.account_ref JOIN bridge_capacity_anchors b ON b.account_ref=s.account_ref
        WHERE s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id) GROUP BY s.account_ref ORDER BY s.account_ref").map_err(db_error)?;
    let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).map_err(db_error)?;
    let mut pending=Vec::new();for row in rows {let row=row.map_err(db_error)?;if accounts.contains(&row.0) {pending.push(row);}}
    Ok(pending)
}
struct FenceGuard<'a> {runtime:&'a super::bridge_runtime::BridgeBudgetRuntime,fence:RebaseFence,committed:bool}
impl Drop for FenceGuard<'_> {
    fn drop(&mut self) {if !self.committed {let _=self.runtime.with_store(|s,l|s.abort_capacity_rebase(l,&self.fence));}}
}
pub(super) fn reconcile_with(runtime:&super::bridge_runtime::BridgeBudgetRuntime,account:&str,fetch:impl FnOnce()->Result<Vec<Value>,String>)->Result<bool,String> {
    reconcile_with_mode(runtime,account,false,fetch)
}
pub(super) fn reconcile_increased_capacity(runtime:&super::bridge_runtime::BridgeBudgetRuntime,account:&str,general:i64,work:i64,fetch:impl FnOnce()->Result<Vec<Value>,String>)->Result<bool,String> {
    let needed=runtime.with_store(|s,_| {
        let current:Option<(i64,i64)>=s.connection.query_row("SELECT a.general_microcredits,a.work_microcredits FROM bridge_capacity_accounts a JOIN bridge_capacity_anchors b ON b.account_ref=a.account_ref WHERE a.account_ref=?1",[account],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        Ok(current.is_some_and(|(g,w)|general>g || work>w))
    })?;
    if !needed {return Ok(false);}
    reconcile_with_mode(runtime,account,true,fetch)
}
fn reconcile_with_mode(runtime:&super::bridge_runtime::BridgeBudgetRuntime,account:&str,allow_zero:bool,fetch:impl FnOnce()->Result<Vec<Value>,String>)->Result<bool,String> {
    if !dedicated_enabled(runtime.data_dir(),account)? {return Ok(false);}
    let fence=runtime.with_store(|s,l| {
        let pending:bool=s.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_slots s WHERE s.account_ref=?1 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id))",[account],|r|r.get(0)).map_err(db_error)?;
        if !pending && !allow_zero {return Ok(None);}
        if anchor(&s.connection,account)?.is_none() {return Err("capacity_anchor_missing".into());}
        s.begin_capacity_rebase(l,account,chrono::Utc::now().timestamp_millis()).map(Some)
    })?;
    let Some(fence)=fence else {return Ok(false)};
    let mut guard=FenceGuard {runtime,fence,committed:false};
    let packs=fetch()?; // No DB transaction/runtime mutex across upstream I/O.
    let observed=PackObservation::parse(&packs,chrono::Utc::now().timestamp_millis())?;
    if !dedicated_enabled(runtime.data_dir(),account)? {return Err("dedicated_account_disabled".into());}
    runtime.with_store(|s,l|s.commit_dedicated_capacity(l,&guard.fence,&observed))?;
    guard.committed=true;Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn pack(id:&str,product:i64,limit:&str,used:&str)->Value {
        json!({"id":id,"entitlement_base_info":{"product_id":product,"quota":{"credits_limit":limit},"start_time":1},"usage":{"credits_amount":used},"expire_time":1000})
    }
    #[cfg(windows)]
    #[test]
    #[ignore = "explicit read-only live entitlement probe; never runs with isolated regression"]
    fn live_readonly_entitlement_shape() {
        assert_eq!(std::env::var("BRIDGE_CAPACITY_READONLY_ACK").as_deref(),Ok("1"));
        let root=std::path::PathBuf::from(std::env::var("BRIDGE_CAPACITY_READONLY_DATA").expect("explicit existing data directory required"));
        assert!(root.is_absolute() && root.is_dir());
        let paths=["conf/vault_key.bin","conf/vault.stronghold","data/checkin_accounts.json","data/device_map.json"].map(|p|root.join(p));
        let before=paths.iter().map(|p|std::fs::read(p).expect("all existing source files must be present")).collect::<Vec<_>>();
        let result=(||->Result<(),String> {
            let state=crate::state::AppState {data_dir:root,python_dir:Default::default(),python_exe:String::new(),jwt_refresh_lock:Default::default()};
            let accounts=crate::vault::load_accounts(&state);
            let devices:crate::models::DeviceMap=serde_json::from_slice(&before[3]).map_err(|_|"device map invalid")?;
            let (account,device)=accounts.accounts.iter().filter(|a|!a.jwt.trim().is_empty()).find_map(|a|a.user_id.as_deref().and_then(|id|devices.get(id)).filter(|d|!d.device_id.is_empty()).map(|d|(a,d))).ok_or("no existing usable credential")?;
            let fetch=||crate::commands::accounts::query_ent_packs_for_bridge(&account.jwt,device).map_err(|e| {
                if e.contains("status code 401") {"live entitlement authentication rejected (401)".to_string()} else {"live entitlement read failed (raw response withheld)".to_string()}
            });
            let first=fetch()?;
            let shape=first.iter().map(|p|serde_json::json!({
                "product":p.pointer("/entitlement_base_info/product_id"),"kind":p.pointer("/entitlement_base_info/product_type"),
                "quota":p.pointer("/entitlement_base_info/quota"),"used":p.pointer("/usage/credits_amount"),
                "group_name":p.get("group_name"),"display_desc":p.get("display_desc"),"status":p.get("status"),"ent_status":p.pointer("/entitlement_base_info/ent_status"),
                "start":p.pointer("/entitlement_base_info/start_time"),"end":p.pointer("/entitlement_base_info/end_time"),"expire":p.get("expire_time"),
                "entitlement_id_type":p.pointer("/entitlement_base_info/entitlement_id").map(|v|if v.is_string() {"string"} else {"other"}),
                "available_endpoint":p.pointer("/entitlement_base_info/available_endpoint")})).collect::<Vec<_>>();
            println!("read-only entitlement schema (no credentials or identifiers): {}",serde_json::to_string(&shape).unwrap());
            let before=PackObservation::parse(&first,chrono::Utc::now().timestamp_millis())?;
            println!("source parser balances in microcredits: {:?}",before.balances()?);
            let admission=super::super::bridge_planner::parse_capacity(&first,before.observed_at_ms)?;
            if before.balances()? != (admission.general,admission.work) {return Err("admission and reconciliation capacity differ".into());}
            std::thread::sleep(std::time::Duration::from_millis(1000));
            let second=fetch()?;
            let after=PackObservation::parse(&second,chrono::Utc::now().timestamp_millis())?;
            after.proves(&before,0)?;
            println!("two read-only observations matched at zero delta; not a paid settlement proof");Ok(())
        })();
        for (path,bytes) in paths.iter().zip(before) {assert_eq!(std::fs::read(path).unwrap(),bytes,"source configuration must remain unchanged");}
        result.unwrap();
    }
    #[test]
    fn exact_pack_debit_can_restore_general_capacity_without_inventing_a_receipt() {
        let before=PackObservation::parse(&[pack("g",208,"100","0"),pack("w",209,"100","0")],10_000).unwrap();
        let after=PackObservation::parse(&[pack("w",209,"100","50"),pack("g",208,"100","0")],20_000).unwrap();
        after.proves(&before,50_000_000).unwrap();
        assert_eq!(after.balances().unwrap(),(100_000_000,50_000_000));
        assert!(before.proves(&before,50_000_000).is_err(),"same balance is not debit coverage");
        assert!(after.proves(&before,49_999_999).is_err());
        assert!(after.proves(&before,50_000_001).is_err());
    }
    #[test]
    fn changed_or_missing_packs_refunds_and_unclassified_resources_cannot_prove_coverage() {
        let before=PackObservation::parse(&[pack("g",208,"100","10")],10_000).unwrap();
        for next in [vec![],vec![pack("other",208,"100","20")],vec![pack("g",208,"100","9")],vec![pack("g",208,"101","20")]] {
            let after=PackObservation::parse(&next,20_000).unwrap();assert!(after.proves(&before,10_000_000).is_err());
        }
        assert!(PackObservation::parse(&[pack("same",208,"100","0"),pack("same",208,"100","0")],20_000).is_err());
        assert!(PackObservation::parse(&[pack("unknown",999,"100","0")],20_000).is_err());
        assert!(PackObservation::parse(&[pack("over",208,"100","101")],20_000).is_err());
        let after=PackObservation::parse(&[pack("g",208,"100","15"),pack("new",209,"10","5")],20_000).unwrap();
        after.proves(&before,10_000_000).unwrap();assert_eq!(after.balances().unwrap(),(85_000_000,5_000_000));
    }
    #[test]
    fn zero_expiry_matches_native_capacity_and_duplicate_ids_cannot_inflate_balance() {
        let mut unexpired=pack("g",208,"10.000001","0.000001");
        unexpired["expire_time"]=json!(0);unexpired["entitlement_base_info"]["end_time"]=json!(0);
        let observed=PackObservation::parse(&[unexpired.clone()],10_000).unwrap();
        assert_eq!(observed.balances().unwrap(),(10_000_000,0));
        let mut duplicate=unexpired.clone();duplicate["entitlement_base_info"]["quota"]["credits_limit"]=json!("20");
        assert!(PackObservation::parse(&[unexpired,duplicate],10_000).is_err());
    }
    #[test]
    fn native_monthly_credits_and_sparse_usage_do_not_break_known_capacity() {
        let mut monthly=pack("monthly",221,"500","500");monthly["entitlement_base_info"]["entitlement_id"]=json!("monthly-native");
        monthly.as_object_mut().unwrap().remove("id");
        let mut sparse=pack("daily",208,"150","0");sparse["usage"]=json!({});
        let before=PackObservation::parse(&[monthly.clone(),sparse.clone(),pack("g",208,"100","10")],10_000).unwrap();
        assert_eq!(before.balances().unwrap(),(90_000_000,0),"unreported usage cannot authorize an assumed unused 150 credits");
        sparse["usage"]=json!({"credits_amount":"5"});
        monthly["display_desc"]=json!("translated label changed");
        let after=PackObservation::parse(&[monthly,sparse,pack("g",208,"100","10")],20_000).unwrap();
        after.proves(&before,5_000_000).unwrap();
        assert_eq!(after.balances().unwrap(),(235_000_000,0));
        let mut malformed=pack("bad",208,"100","0");malformed["usage"]=Value::Null;
        assert!(PackObservation::parse(&[malformed],20_000).is_err());
    }
    #[cfg(windows)]
    #[test]
    fn initial_snapshot_and_anchor_rollback_as_one_transaction() {
        use super::super::{bridge_prepared::tests::{fixture,cleanup},bridge_budget::CapacitySnapshot};
        let (dir,mut store,lease)=fixture();
        let observed=PackObservation::parse(&[pack("new",208,"100","0")],10_000).unwrap();
        let snapshot=CapacitySnapshot {account_ref:"new-account".into(),snapshot_ref:"fixture".into(),epoch:1,general:100_000_000,work:0,observed_at_ms:10_000};
        store.connection.execute_batch("CREATE TRIGGER fail_initial_anchor BEFORE INSERT ON bridge_capacity_anchors BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(store.initialize_capacity_source(&lease,&snapshot,Some(&observed)).is_err());
        let exists:bool=store.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_accounts WHERE account_ref='new-account')",[],|r|r.get(0)).unwrap();assert!(!exists);
        store.connection.execute_batch("DROP TRIGGER fail_initial_anchor").unwrap();
        store.initialize_capacity_source(&lease,&snapshot,Some(&observed)).unwrap();
        cleanup(dir,store,lease);
    }
    #[cfg(windows)]
    #[test]
    fn anchor_and_coverage_commit_together_and_survive_reopen() {
        use super::super::{bridge_billing::BridgeBillingStore,bridge_prepared::tests::{fixture,cleanup},bridge_receipts::tests::{send,cache}};
        let (dir,mut store,lease)=fixture();
        let mut g=pack("g",208,"100","0");g["entitlement_base_info"]["start_time"]=json!(0);g["expire_time"]=Value::Null;
        let mut w=pack("w",209,"100","0");w["entitlement_base_info"]["start_time"]=json!(0);w["expire_time"]=Value::Null;
        let before=PackObservation::parse(&[g.clone(),w.clone()],1).unwrap();
        store.initialize_capacity_anchor(&lease,"account",&before).unwrap();
        let id=send(&mut store,&lease,"request-source");cache(&dir,&store,&[(&id,"50")],2000);store.confirm_budget_usage(&lease,&id).unwrap();
        let fence=store.begin_capacity_rebase(&lease,"account",chrono::Utc::now().timestamp_millis()+10_000).unwrap();
        assert!(store.commit_dedicated_capacity(&lease,&fence,&PackObservation::parse(&[g.clone(),w.clone()],fence.started_at_ms+1).unwrap()).is_err());
        w["usage"]["credits_amount"]=json!("50");
        let after=PackObservation::parse(&[g,w],fence.started_at_ms+1).unwrap();
        store.connection.execute_batch("CREATE TRIGGER fail_anchor BEFORE UPDATE ON bridge_capacity_anchors BEGIN SELECT RAISE(ABORT,'anchor-write-failure'); END").unwrap();
        assert!(store.commit_dedicated_capacity(&lease,&fence,&after).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        let epoch:i64=store.connection.query_row("SELECT snapshot_epoch FROM bridge_capacity_accounts",[],|r|r.get(0)).unwrap();assert_eq!(epoch,1);
        store.connection.execute_batch("DROP TRIGGER fail_anchor").unwrap();
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        store.commit_dedicated_capacity(&lease,&fence,&after).unwrap();
        store.commit_dedicated_capacity(&lease,&fence,&after).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,0);
        let balances:(i64,i64)=store.connection.query_row("SELECT general_microcredits,work_microcredits FROM bridge_capacity_accounts",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        assert_eq!(balances,(100_000_000,50_000_000));
        assert!(store.initialize_capacity_anchor(&lease,"account",&before).is_err(),"old anchor cannot overwrite committed coverage");
        let debit:i64=store.connection.query_row("SELECT actual_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[id],|r|r.get(0)).unwrap();assert_eq!(debit,50_000_000);
        cleanup(dir,store,lease);
    }
    #[cfg(windows)]
    #[test]
    fn initial_anchor_cannot_absorb_unknown_or_already_settled_debits() {
        use super::super::{bridge_prepared::tests::{fixture,cleanup},bridge_receipts::tests::{send,cache}};
        let (dir,mut store,lease)=fixture();
        let before=PackObservation::parse(&[pack("g",208,"100","0"),pack("w",209,"100","0")],10_000).unwrap();
        assert!(store.initialize_capacity_anchor(&lease,"account",&before).is_err(),"timestamp must match initial snapshot");
        let id=send(&mut store,&lease,"request-no-anchor");
        assert!(store.initialize_capacity_anchor(&lease,"account",&before).is_err());
        cache(&dir,&store,&[(&id,"50")],2000);store.confirm_budget_usage(&lease,&id).unwrap();
        assert!(store.initialize_capacity_anchor(&lease,"account",&before).is_err());
        let fence=store.begin_capacity_rebase(&lease,"account",chrono::Utc::now().timestamp_millis()+10_000).unwrap();
        assert!(store.commit_dedicated_capacity(&lease,&fence,&before).is_err());
        store.abort_capacity_rebase(&lease,&fence).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        cleanup(dir,store,lease);
    }
    #[cfg(windows)]
    #[test]
    fn dedicated_driver_is_opt_in_and_never_holds_database_lock_during_fetch() {
        use super::super::{bridge_runtime::BridgeBudgetRuntime,bridge_prepared::tests::fixture,bridge_receipts::tests::{send,cache}};
        let (dir,mut store,mut lease)=fixture();
        let packs=vec![json!({"id":"g","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"0"}}),
            json!({"id":"w","entitlement_base_info":{"product_id":209,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"0"}})];
        store.initialize_capacity_anchor(&lease,"account",&PackObservation::parse(&packs,1).unwrap()).unwrap();
        let id=send(&mut store,&lease,"request-driver");
        store.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1-1000,finished_at_ms=?1 WHERE budget_id=?2",params![chrono::Utc::now().timestamp_millis()-10_000,id]).unwrap();
        cache(&dir,&store,&[(&id,"50")],2000);store.confirm_budget_usage(&lease,&id).unwrap();
        store.finish_activity_lease_cleanly(&mut lease,||Ok(())).unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        assert!(!reconcile_with(&runtime,"account",||panic!("missing config must not fetch")).unwrap());
        std::fs::write(dir.join("bridge-dedicated-accounts.json"),serde_json::to_vec(&json!({"version":1,"accounts":["account"]})).unwrap()).unwrap();
        assert!(dedicated_enabled(&dir,"account").unwrap());assert!(!dedicated_enabled(&dir,"other").unwrap());
        assert_eq!(super::super::usage_refresh::pending_refresh_accounts(&dir).unwrap().len(),1,"settled fees awaiting capacity coverage must remain discoverable after restart");
        assert!(reconcile_with(&runtime,"account",||Err("fixture outage".into())).is_err());
        runtime.with_store(|s,_| {let state:String=s.connection.query_row("SELECT rebase_state FROM bridge_capacity_accounts",[],|r|r.get(0)).unwrap();assert_eq!(state,"open");Ok(())}).unwrap();
        let mut after=packs;after[1]["usage"]["credits_amount"]=json!("50");
        assert!(reconcile_with(&runtime,"account",|| {
            // Separate thread acquires runtime + DB lock while simulated network
            // is pending: a lock accidentally held across I/O would time out.
            let (tx,rx)=std::sync::mpsc::channel();let worker=runtime.clone();
            let join=std::thread::spawn(move ||{tx.send(worker.with_store(|s,_|s.capacity_totals("account").map(|v|v.confirmed))).unwrap();});
            assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap().unwrap(),50_000_000);join.join().unwrap();std::thread::sleep(std::time::Duration::from_millis(2));Ok(after)
        }).unwrap());
        assert!(!reconcile_with(&runtime,"account",||panic!("no uncovered debit must not poll")).unwrap());
        let mut top_up=vec![json!({"id":"g","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"0"}}),
            json!({"id":"w","entitlement_base_info":{"product_id":209,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"50"}})];
        top_up.push(json!({"id":"bonus","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"50"}},"usage":{"credits_amount":"0"}}));
        assert!(reconcile_increased_capacity(&runtime,"account",150_000_000,50_000_000,||{std::thread::sleep(std::time::Duration::from_millis(2));Ok(top_up)}).unwrap());
        runtime.with_store(|s,_| {let g:i64=s.connection.query_row("SELECT general_microcredits FROM bridge_capacity_accounts",[],|r|r.get(0)).unwrap();assert_eq!(g,150_000_000);Ok(())}).unwrap();
        runtime.with_store(|s,_| {assert_eq!(s.capacity_totals("account")?.confirmed,0);Ok(())}).unwrap();
        assert!(super::super::usage_refresh::pending_refresh_accounts(&dir).unwrap().is_empty());
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
}
