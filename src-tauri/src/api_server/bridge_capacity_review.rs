//! Operator reviewed source statements. No public HTTP admission or billing API.
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease,bridge_capacity_source::PackObservation,bridge_rebase::RebaseFence};
use serde_json::Value;
use rusqlite::{params,OptionalExtension,TransactionBehavior};
use sha2::{Digest,Sha256};
fn db_error(e:rusqlite::Error)->String{format!("capacity review database error: {e}")}
fn field(v:&Value,key:&str)->Result<i64,String>{v[key].as_i64().ok_or_else(||"capacity_review_invalid".into())}
fn credits(v:&Value)->Result<i64,String>{let text=v.as_str().ok_or("capacity_review_source_invalid")?;let c=aiwork_core::CreditAmount::parse(text,"credits")?.as_microcredits();if c<0{return Err("capacity_review_source_invalid".into());}Ok(c)}
impl BridgeBillingStore {
    /// Explicit operator repair only. Complete authenticated source rows are
    /// required for every final bill. Unexplained package drift stays as a net
    /// capacity deduction, with no new charge, refund or receipt event.
    pub(super) fn commit_reviewed_capacity(&mut self,lease:&BridgeBudgetLease,fence:&RebaseFence,source:&Value,observation:&PackObservation)->Result<(),String>{
        use super::{bridge_rebase,bridge_capacity_source};
        observation.validate()?;
        let encoded_source=serde_json::to_vec(&serde_json::json!({"statement":source,"observation":observation})).map_err(|_|"capacity_review_invalid")?;
        if encoded_source.len()>8*1024*1024{return Err("capacity_review_bounds".into());}
        let hash=format!("{:x}",Sha256::digest(&encoded_source));
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let(state,stored)=bridge_rebase::check_fence(&tx,lease,fence)?;
        if state=="committed"{let stored:Value=serde_json::from_str(stored.as_deref().ok_or("capacity_review_conflict")?).map_err(|_|"capacity_review_conflict")?;return if stored.pointer("/reviewed_source_adjustment/source_hash").and_then(Value::as_str)==Some(hash.as_str()){Ok(())}else{Err("capacity_review_conflict".into())};}
        if state!="pending"{return Err("capacity_rebase_not_pending".into());}bridge_rebase::require_pending_owner(&tx,fence)?;
        let(event,legacy,latest)=bridge_rebase::quiet_watermark(&tx,&fence.account_ref)?;
        if event!=fence.event_sequence||legacy!=fence.legacy_sessions||latest>fence.started_at_ms{return Err("capacity_rebase_watermark_changed".into());}
        let snapshot=bridge_rebase::snapshot(&tx,&fence.account_ref)?;let start=field(source,"query_start_ms")?;let end=field(source,"query_end_ms")?;let read=field(source,"read_started_at_ms")?;let completed=field(source,"completed_at_ms")?;
        let now=chrono::Utc::now().timestamp_millis();
        if source["account_ref"].as_str()!=Some(fence.account_ref.as_str())||source["complete"]!=true||start<0||start>snapshot.observed_at_ms||end<read.saturating_sub(2000)||end>read.saturating_add(1000)||read<fence.started_at_ms||completed<read||observation.observed_at_ms<completed||observation.observed_at_ms<=fence.started_at_ms||now.saturating_sub(read)>120000||read.saturating_sub(now)>60000{return Err("capacity_review_source_unproven".into());}
        let rows=source["sessions"].as_object().ok_or("capacity_review_source_invalid")?;if rows.len()>4096{return Err("capacity_review_bounds".into());}
        let mut sums=std::collections::BTreeMap::new();let mut all_cost=0i64;
        for(sid,row)in rows{
            let time=field(row,"usage_time")?.checked_mul(1000).ok_or("capacity_review_overflow")?;
            if sid.is_empty()||sid.len()>256||row["session_id"].as_str()!=Some(sid.as_str())||row["ambiguous"]==true||time<start.saturating_sub(1000)||time>end{return Err("capacity_review_source_invalid".into());}
            let cost=credits(&row["credits_float"])?;all_cost=all_cost.checked_add(cost).ok_or("capacity_review_overflow")?;sums.insert(sid.clone(),(cost,time));
        }
        let pending={let mut stmt=tx.prepare("SELECT e.session_ref,e.started_at_ms,e.finished_at_ms,s.actual_microcredits FROM bridge_capacity_slots s JOIN bridge_budget_executions e USING(budget_id) WHERE s.account_ref=?1 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id)").map_err(db_error)?;let values=stmt.query_map([&fence.account_ref],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,Option<i64>>(2)?,r.get::<_,i64>(3)?))).map_err(db_error)?.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?;values};
        if pending.is_empty(){return Err("capacity_review_no_receipts".into());}
        let mut known=std::collections::BTreeSet::new();let mut actual=0i64;
        for(sid,started,finished,cost)in &pending{
            if !known.insert(sid.clone())||start>*started||finished.is_some_and(|v|end<v)||sums.get(sid).map(|v|v.0)!=Some(*cost){return Err("capacity_review_receipt_mismatch".into());}
            let unique:i64=tx.query_row("SELECT count(*)FROM bridge_budget_executions WHERE account_ref=?1 AND session_ref=?2",params![fence.account_ref,sid],|r|r.get(0)).map_err(db_error)?;if unique!=1{return Err("capacity_review_identity_ambiguous".into());}
            actual=actual.checked_add(*cost).ok_or("capacity_review_overflow")?;
        }
        let mut foreign=0i64;
        for(sid,(cost,time))in &sums{
            if known.contains(sid)||time.saturating_add(1000)<=snapshot.observed_at_ms{continue;}
            let covered:Option<i64>=tx.query_row("SELECT s.actual_microcredits FROM bridge_budget_executions e JOIN bridge_capacity_slots s USING(budget_id) JOIN bridge_capacity_covered c USING(budget_id)WHERE e.account_ref=?1 AND e.session_ref=?2 AND e.started_at_ms<=?3",params![fence.account_ref,sid,snapshot.observed_at_ms],|r|r.get(0)).optional().map_err(db_error)?;
            let legacy_cost:Option<i64>=tx.query_row("SELECT r.actual_microcredits FROM bridge_core_upstream_sessions e JOIN bridge_billing_receipts r USING(request_id) WHERE e.account_ref=?1 AND e.session_id=?2 AND r.status='final' AND e.associated_at_ms<=?3",params![fence.account_ref,sid,snapshot.observed_at_ms],|r|r.get(0)).optional().map_err(db_error)?;
            if covered.or(legacy_cost)==Some(*cost){continue;}
            foreign=foreign.checked_add(*cost).ok_or("capacity_review_overflow")?;
        }
        let prior:Option<String>=tx.query_row("SELECT observation_json FROM bridge_capacity_anchors WHERE account_ref=?1",[&fence.account_ref],|r|r.get(0)).optional().map_err(db_error)?;
        let old_adjustment=bridge_capacity_source::reviewed_adjustment(&tx,&fence.account_ref,fence.snapshot_epoch)?;
        let old_buffer=old_adjustment.as_ref().map(bridge_capacity_source::adjustment_buffer).transpose()?.unwrap_or(0);
        let(mut buffer,mut allowance)=(0i64,0i64);let mut retired=std::collections::BTreeSet::<String>::new();
        if let Some(prior)=prior{
            let previous:PackObservation=serde_json::from_str(&prior).map_err(|_|"capacity_anchor_invalid")?;previous.validate()?;
            if previous.observed_at_ms!=snapshot.observed_at_ms||bridge_capacity_source::net_balances(&previous,old_buffer)?!=(snapshot.general,snapshot.work){return Err("capacity_anchor_snapshot_changed".into());}
            let meta:Option<String>=tx.query_row("SELECT coverage_json FROM bridge_capacity_rebases WHERE account_ref=?1 AND state='committed' ORDER BY json_extract(coverage_json,'$.snapshot.epoch') DESC LIMIT 1",[&fence.account_ref],|r|r.get(0)).optional().map_err(db_error)?;
            if let Some(meta)=meta{let meta:Value=serde_json::from_str(&meta).map_err(|_|"capacity_review_invalid")?;allowance=meta.get("expired_allowance_microcredits").map(|v|v.as_i64().filter(|v|*v>=0).ok_or("capacity_review_invalid")).transpose()?.unwrap_or(0);retired=meta.get("expired_pack_ids").map(|v|serde_json::from_value(v.clone()).map_err(|_|"capacity_review_invalid")).transpose()?.unwrap_or_default();}
            if observation.packs.keys().any(|id|retired.contains(id)){return Err("capacity_source_pack_changed".into());}
            let mut delta=0i64;
            for(id,p)in &observation.packs{let used=if let Some(old)=previous.packs.get(id){if old.product!=p.product||old.limit!=p.limit||old.start_ms!=p.start_ms||old.end_ms!=p.end_ms||p.used<old.used{return Err("capacity_source_pack_changed".into());}old.used}else{0};delta=delta.checked_add(p.used-used).ok_or("capacity_review_overflow")?;}
            for(id,p)in &previous.packs{if !observation.packs.contains_key(id){if p.used!=p.limit&&!p.end_ms.is_some_and(|end|end<=observation.observed_at_ms){return Err("capacity_source_missing_pack".into());}allowance=allowance.checked_add(p.limit-p.used).ok_or("capacity_review_overflow")?;retired.insert(id.clone());}}
            let expected=actual.checked_add(foreign).and_then(|v|v.checked_add(old_buffer)).ok_or("capacity_review_overflow")?;
            if delta>expected{return Err("capacity_review_unexplained_source_debit".into());}
            let unexplained=expected-delta;let expired_consumed=unexplained.min(allowance);allowance-=expired_consumed;buffer=unexplained-expired_consumed;
        }else{
            // A missing legacy anchor is repaired only when the complete source
            // lifetime statement proves every currently reported consumed unit.
            if observation.packs.values().any(|p|p.start_ms.is_none()){return Err("capacity_review_lifetime_unproven".into());}
            let earliest=observation.packs.values().filter_map(|p|p.start_ms).min().ok_or("capacity_review_lifetime_unproven")?;
            let used=observation.packs.values().try_fold(0i64,|sum,p|sum.checked_add(p.used)).ok_or("capacity_review_overflow")?;
            if start>earliest||all_cost!=used||all_cost!=actual||foreign!=0{return Err("capacity_review_lifetime_unproven".into());}
        }
        if retired.len()>4096{return Err("capacity_review_bounds".into());}
        let(g,w)=bridge_capacity_source::net_balances(observation,buffer)?;let epoch=fence.snapshot_epoch.checked_add(1).ok_or("capacity_review_overflow")?;
        let reference=format!("reviewed-session-net-v1:{hash}");let snapshot_ref=format!("dedicated-packs:{hash}");
        let coverage=serde_json::json!({"snapshot":{"account_ref":fence.account_ref,"snapshot_ref":snapshot_ref,"epoch":epoch,"general":g,"work":w,"observed_at_ms":observation.observed_at_ms},"coverage_ref":reference,"event_sequence":event,"quiescent_since_ms":fence.started_at_ms,"external_activity_excluded":true,"expired_allowance_microcredits":allowance,"expired_pack_ids":retired,"reviewed_source_adjustment":{"policy":"full-session-statement-net-v1","source_hash":hash,"buffer_microcredits":buffer,"actual_covered_microcredits":actual,"foreign_statement_microcredits":foreign,"query_start_ms":start,"query_end_ms":end,"read_started_at_ms":read,"completed_at_ms":completed}});
        tx.execute("INSERT INTO bridge_capacity_covered(budget_id,owner_nonce,actual_microcredits,receipt_hash) SELECT s.budget_id,?1,s.actual_microcredits,r.semantic_hash FROM bridge_capacity_slots s JOIN bridge_budget_receipts r USING(budget_id) WHERE s.account_ref=?2 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id)",params![fence.owner_nonce,fence.account_ref]).map_err(db_error)?;
        tx.execute("UPDATE bridge_capacity_accounts SET snapshot_ref=?1,snapshot_epoch=?2,general_microcredits=?3,work_microcredits=?4,observed_at_ms=?5,rebase_state='open',owner_nonce=NULL WHERE account_ref=?6",params![snapshot_ref,epoch,g,w,observation.observed_at_ms,fence.account_ref]).map_err(db_error)?;
        tx.execute("INSERT INTO bridge_capacity_anchors(account_ref,snapshot_epoch,event_sequence,legacy_sessions,observation_json) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(account_ref)DO UPDATE SET snapshot_epoch=excluded.snapshot_epoch,event_sequence=excluded.event_sequence,legacy_sessions=excluded.legacy_sessions,observation_json=excluded.observation_json",params![fence.account_ref,epoch,event,legacy,serde_json::to_string(observation).map_err(|_|"capacity_review_invalid")?]).map_err(db_error)?;
        tx.execute("UPDATE bridge_capacity_rebases SET state='committed',coverage_json=?1 WHERE owner_nonce=?2",params![coverage.to_string(),fence.owner_nonce]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
}

#[cfg(all(test,windows))]
mod tests{
 use super::*;use serde_json::json;
 use crate::api_server::{bridge_prepared::tests::{fixture,cleanup},bridge_receipts::tests::{send,cache}};
 fn packs(used:&str,now:i64)->PackObservation{PackObservation::parse(&[208,209].map(|p|json!({"id":format!("p-{p}"),"entitlement_base_info":{"product_id":p,"quota":{"credits_limit":"100"},"start_time":0,"end_time":null},"usage":{"credits_amount":if p==208{used}else{"0"}}})),now).unwrap()}
 fn setup(anchor:bool)->(std::path::PathBuf,BridgeBillingStore,BridgeBudgetLease,String,RebaseFence,Value){
  let(dir,mut s,lease)=fixture();if anchor{s.initialize_capacity_anchor(&lease,"account",&packs("0",1)).unwrap();}
  let id=send(&mut s,&lease,"request-reviewed-capacity");let now=chrono::Utc::now().timestamp_millis();s.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1,finished_at_ms=?2 WHERE budget_id=?3",rusqlite::params![now-7200000,now-3600000,&id]).unwrap();cache(&dir,&s,&[(&id,"50")],2000);s.confirm_budget_usage(&lease,&id).unwrap();
  let e=s.budget_execution(&id).unwrap().unwrap();let f=s.begin_capacity_rebase(&lease,"account",now).unwrap();let source=json!({"account_ref":"account","complete":true,"query_start_ms":0,"query_end_ms":now,"read_started_at_ms":now+1,"completed_at_ms":now+2,"sessions":{e.session_ref.clone():{"session_id":e.session_ref,"usage_time":(now-1800000)/1000,"credits_float":"50","ambiguous":false}}});
  (dir,s,lease,id,f,source)
 }
 #[test]fn reviewed_capacity_retains_unexplained_debit_as_a_nonfinancial_safety_buffer(){
  let(dir,mut s,lease,id,f,source)=setup(true);let observation=packs("45",f.started_at_ms+3);
  s.commit_reviewed_capacity(&lease,&f,&source,&observation).unwrap();
  s.commit_reviewed_capacity(&lease,&f,&source,&observation).unwrap();
  assert_eq!(s.capacity_totals("account").unwrap().confirmed,0);let net:(i64,i64)=s.connection.query_row("SELECT general_microcredits,work_microcredits FROM bridge_capacity_accounts",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();assert_eq!(net,(50_000_000,100_000_000));
  let amount:i64=s.connection.query_row("SELECT actual_microcredits FROM bridge_budget_receipts WHERE budget_id=?1",[&id],|r|r.get(0)).unwrap();assert_eq!(amount,50_000_000);
  drop(s);let mut s=BridgeBillingStore::open(&dir).unwrap();
  let next=s.begin_capacity_rebase(&lease,"account",f.started_at_ms+10).unwrap();s.commit_dedicated_capacity(&lease,&next,&packs("45",f.started_at_ms+11)).unwrap();s.commit_dedicated_capacity(&lease,&next,&packs("45",f.started_at_ms+11)).unwrap();let net:i64=s.connection.query_row("SELECT general_microcredits FROM bridge_capacity_accounts",[],|r|r.get(0)).unwrap();assert_eq!(net,50_000_000,"ordinary refresh must preserve the reviewed safety deduction");
  cleanup(dir,s,lease);
 }
 #[test]fn reviewed_capacity_rolls_back_all_bookkeeping_if_anchor_write_fails(){
  let(dir,mut s,lease,_id,f,source)=setup(true);s.connection.execute_batch("CREATE TRIGGER deny_review BEFORE UPDATE ON bridge_capacity_anchors BEGIN SELECT RAISE(ABORT,'fixture atomicity');END").unwrap();
  assert!(s.commit_reviewed_capacity(&lease,&f,&source,&packs("45",f.started_at_ms+3)).is_err());assert_eq!(s.capacity_totals("account").unwrap().confirmed,50_000_000);assert_eq!(s.connection.query_row("SELECT snapshot_epoch FROM bridge_capacity_accounts",[],|r|r.get::<_,i64>(0)).unwrap(),1);s.connection.execute_batch("DROP TRIGGER deny_review").unwrap();s.abort_capacity_rebase(&lease,&f).unwrap();cleanup(dir,s,lease);
 }
 #[test]fn reviewed_capacity_accounts_for_foreign_usage_without_billing_the_key(){
  let(dir,mut s,lease,_id,f,mut source)=setup(true);source["sessions"]["foreign"]=json!({"session_id":"foreign","usage_time":(f.started_at_ms-1800000)/1000,"credits_float":"2","ambiguous":false});
  s.commit_reviewed_capacity(&lease,&f,&source,&packs("52",f.started_at_ms+3)).unwrap();assert_eq!(s.capacity_totals("account").unwrap().confirmed,0);let count:i64=s.connection.query_row("SELECT count(*)FROM bridge_budget_receipt_events",[],|r|r.get(0)).unwrap();assert_eq!(count,1);cleanup(dir,s,lease);
 }
 #[test]fn reviewed_capacity_bootstraps_only_with_complete_lifetime_debit_coverage(){
  let(dir,mut s,lease,_id,f,source)=setup(false);s.commit_reviewed_capacity(&lease,&f,&source,&packs("50",f.started_at_ms+3)).unwrap();assert_eq!(s.capacity_totals("account").unwrap().confirmed,0);assert_eq!(s.connection.query_row("SELECT count(*)FROM bridge_capacity_anchors",[],|r|r.get::<_,i64>(0)).unwrap(),1);cleanup(dir,s,lease);
 }
 #[test]fn reviewed_capacity_rejects_missing_mismatched_ambiguous_or_incomplete_source_rows(){
  for mode in 0..6{let(dir,mut s,lease,id,f,mut source)=setup(true);let sid=s.budget_execution(&id).unwrap().unwrap().session_ref;
   match mode{0=>source["complete"]=json!(false),1=>{source["sessions"].as_object_mut().unwrap().remove(&sid);},2=>source["sessions"][&sid]["credits_float"]=json!("49"),3=>source["sessions"][&sid]["ambiguous"]=json!(true),4=>source["account_ref"]=json!("foreign"),_=>source["read_started_at_ms"]=json!(f.started_at_ms-1)}
   assert!(s.commit_reviewed_capacity(&lease,&f,&source,&packs("45",f.started_at_ms+3)).is_err());assert_eq!(s.capacity_totals("account").unwrap().confirmed,50_000_000);s.abort_capacity_rebase(&lease,&f).unwrap();cleanup(dir,s,lease);
  }
 }
}
