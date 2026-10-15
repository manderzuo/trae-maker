//! Audited release of local capacity; never an invented zero-fee receipt.
use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine};
use rusqlite::{params,Connection,OptionalExtension,Transaction,TransactionBehavior};
use serde::{Deserialize,Serialize};
use super::{bridge_billing::BridgeBillingStore,bridge_budget,bridge_budget_lease::BridgeBudgetLease,bridge_execution::ExecutionState,bridge_prepared};

// Wire shape mirrored by Core's BudgetHoldRelease; keep identity and evidence
// distinct from BillingReceipt so absence can never masquerade as actual zero.
#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub(super) struct BudgetHoldRelease {
    pub wire_version:u8,pub request_id:String,pub budget_id:String,pub core_key_id:String,
    pub account_ref:String,pub bridge_instance_id:String,pub session_ref:String,
    pub hold_microcredits:i64,pub policy:String,pub review_ref:Option<String>,
    pub started_at_ms:i64,pub finished_at_ms:i64,pub query_start_ms:i64,pub query_end_ms:i64,
    pub first_read_at_ms:i64,pub last_read_at_ms:i64,pub observed_at_ms:i64,pub source_hash:String,
}
fn hash(value:&impl Serialize)->Result<String,String> {
    Ok(URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&serde_json::to_value(value).map_err(|_|"hold evidence encoding failed")?)))
}
pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[
    ("table","bridge_budget_hold_releases","CREATE TABLE bridge_budget_hold_releases (
      budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_budget_executions(budget_id),
      proof_json TEXT NOT NULL,
      evidence_hash TEXT NOT NULL,
      released_at_ms INTEGER NOT NULL CHECK(released_at_ms>0)
    )"),
];
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(|e|e.to_string())?;}Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,sql) in SCHEMA_OBJECTS {
        let actual:(String,String)=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|"hold release schema missing")?;
        if actual.0!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&actual.1)?!=super::bridge_billing::tokenize_sqlite_schema_sql(sql)? {return Err("hold release schema invalid".into());}
    }Ok(())
}
pub(super) fn verified_release(connection:&Connection,id:&str)->Result<Option<BudgetHoldRelease>,String> {
        let row:Option<(String,String)>=connection.query_row("SELECT proof_json,evidence_hash FROM bridge_budget_hold_releases WHERE budget_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
        row.map(|(json,digest)| {
            let proof:BudgetHoldRelease=serde_json::from_str(&json).map_err(|_|"invalid hold proof")?;
            let auth=bridge_prepared::stored_budget_authorization(connection,id)?;
            if hash(&proof)?!=digest || proof.budget_id!=id || proof.request_id!=auth.request_id || proof.core_key_id!=auth.core_key_id
                || proof.account_ref!=auth.account_ref || proof.bridge_instance_id!=auth.bridge_instance_id || proof.session_ref!=format!("core-v2-{id}")
                || proof.hold_microcredits!=auth.hold_credits.as_microcredits() {return Err("hold proof binding mismatch".into());}
            Ok(proof)
        }).transpose()
}
impl BridgeBillingStore {
    pub(super) fn budget_hold_release(&self,id:&str)->Result<Option<BudgetHoldRelease>,String> {
        verified_release(&self.connection,id)
    }
    pub(super) fn release_absent_budget_hold(&mut self,lease:&BridgeBudgetLease,id:&str,review:Option<&str>)->Result<bool,String> {
        if self.budget_hold_release(id)?.is_some() {return Ok(true);}
        let execution=self.budget_execution(id)?.ok_or("execution missing")?;
        if execution.state!=ExecutionState::Failed {return Ok(false);}
        let Some(finished)=execution.finished_at_ms else{return Ok(false)};
        let result=self.load_budget_result(id)?.ok_or("terminal result missing")?;
        // A local recovery failure is still an unknown upstream outcome.
        let explicit_failure=result["status"]=="failed" && result["upstream_error"].is_object();
        if review.is_none() && !explicit_failure {return Ok(false);}
        if review.is_some_and(|s|s.trim().is_empty() || s.len()>256) {return Err("invalid review reference".into());}
        use super::bridge_billing::CoreBillingSessionLookup;
        if !matches!(self.usage_session_for_request(&execution.request_id)?,CoreBillingSessionLookup::Unique {core_key_id,account_ref,session_id,..}
            if core_key_id==execution.core_key_id && account_ref==execution.account_ref && session_id==execution.session_ref) {return Err("hold session attribution ambiguous".into());}
        let Some(source)=crate::commands::usage_history::budget_absent_usage(&self.data_dir,&execution.account_ref,&execution.session_ref,execution.started_at_ms,finished)? else{return Ok(false)};
        let identity=bridge_prepared::sent_execution_identity(&self.connection,id)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
        bridge_budget::require_fact_lease(&tx,lease)?;
        if identity!=bridge_prepared::sent_execution_identity(&tx,id)? || identity.authorization.bridge_instance_id!=lease.instance_id() {return Err("hold identity changed".into());}
        let (stage,hold):(String,i64)=tx.query_row("SELECT stage,hold_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|e|e.to_string())?;
        let conflicting:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_budget_receipts WHERE budget_id=?1) OR EXISTS(SELECT 1 FROM bridge_core_requests WHERE request_id=?2) OR EXISTS(SELECT 1 FROM bridge_core_upstream_sessions WHERE account_ref=?3 AND session_id=?4)",params![id,execution.request_id,execution.account_ref,execution.session_ref],|r|r.get(0)).map_err(|e|e.to_string())?;
        if conflicting || stage!="R" {return Ok(false);}
        let proof=BudgetHoldRelease {wire_version:2,request_id:execution.request_id,budget_id:id.into(),core_key_id:execution.core_key_id,
            account_ref:execution.account_ref,bridge_instance_id:execution.bridge_instance_id,session_ref:execution.session_ref,hold_microcredits:hold,
            policy:if review.is_some(){"reviewed-absence-reservation-release-v1"}else{"failed-absence-reservation-release-v1"}.into(),review_ref:review.map(str::to_owned),
            started_at_ms:source.started_at_ms,finished_at_ms:source.finished_at_ms,query_start_ms:source.query_start_ms,query_end_ms:source.query_end_ms,
            first_read_at_ms:source.first_read_at_ms,last_read_at_ms:source.last_read_at_ms,observed_at_ms:source.observed_at_ms,source_hash:hash(&source)?};
        tx.execute("INSERT INTO bridge_budget_hold_releases(budget_id,proof_json,evidence_hash,released_at_ms) VALUES (?1,?2,?3,?4)",params![id,serde_json::to_string(&proof).map_err(|_|"hold proof encoding failed")?,hash(&proof)?,chrono::Utc::now().timestamp_millis()]).map_err(|e|e.to_string())?;
        tx.execute("UPDATE bridge_capacity_slots SET stage='released',amount_microcredits=0 WHERE budget_id=?1 AND stage='R'",[id]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e|e.to_string())?;Ok(true)
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome},bridge_receipts::{ReceiptChange,tests::cache}};
    fn failed(store:&mut BridgeBillingStore,lease:&BridgeBudgetLease,id:&str,explicit:bool)->String {
        let mut value=input();value.request_id=id.into();let prepared=store.prepare_budget(lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(lease,&prepared,20).unwrap() else {panic!("consume")};
        let id=prepared.authorization.budget_id;store.mark_budget_send_intent(lease,&id,&ctx.consume_epoch).unwrap();
        store.bind_budget_task(lease,&id,&format!("task-{id}")).unwrap();
        let result=if explicit {serde_json::json!({"status":"failed","upstream_error":{"code":"3003","message":"provider rejected input"}})}else{serde_json::json!({"status":"failed","error":{"code":"bridge_execution_interrupted"}})};
        let now=chrono::Utc::now().timestamp_millis();
        store.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",params![now-2_000_000,id]).unwrap();
        store.finish_budget_result(lease,&id,ExecutionState::Failed,&result,now-1_800_000).unwrap();id
    }
    fn absence(dir:&std::path::Path,store:&BridgeBillingStore,id:&str,span:i64) {
        let execution=store.budget_execution(id).unwrap().unwrap();let now=chrono::Utc::now().timestamp_millis();
        let file=dir.join("data/usage_history.json");std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        crate::fs_utils::write_json(&file,&serde_json::json!({"accounts":{"account":{"name":"fixture","daily":{},"absent_sessions":{
            execution.session_ref:{"started_at_ms":execution.started_at_ms,"finished_at_ms":execution.finished_at_ms,"query_start_ms":1,"query_end_ms":now+300000,
            "first_read_at_ms":now-span,"last_read_at_ms":now,"observed_at_ms":now}}}}})).unwrap();
    }
    #[test]
    fn hold_release_requires_complete_evidence_and_late_receipt_settles_once_after_restart() {
        let (dir,mut store,lease)=fixture();let id=failed(&mut store,&lease,"failed-explicit",true);
        assert!(!store.release_absent_budget_hold(&lease,&id,None).unwrap());
        absence(&dir,&store,&id,1000);assert!(!store.release_absent_budget_hold(&lease,&id,None).unwrap());
        absence(&dir,&store,&id,61000);assert!(store.release_absent_budget_hold(&lease,&id,None).unwrap());
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,0);
        assert!(store.latest_budget_receipt_event(&id).unwrap().is_none(),"no invented zero receipt");
        assert_eq!(store.pending_core_session_accounts().unwrap().len(),1,"released reservation still tracks late bills");
        let proof=store.budget_hold_release(&id).unwrap().unwrap();drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.budget_hold_release(&id).unwrap().unwrap(),proof);
        cache(&dir,&store,&[(&id,"12.3456")],1_801_000);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Created);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Duplicate);
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,12_345_600);
        assert_eq!(store.budget_receipt_events(lease.generation(),0,10).unwrap().len(),1);
        cleanup(dir,store,lease);
    }
    #[test]
    fn interrupted_failure_needs_review_and_real_charge_never_releases_as_absence() {
        let (dir,mut store,lease)=fixture();let id=failed(&mut store,&lease,"interrupted",false);
        absence(&dir,&store,&id,61000);assert!(!store.release_absent_budget_hold(&lease,&id,None).unwrap());
        store.review_budget_billing(&lease,&id,"operator-reviewed-fixture",None).unwrap();
        let paid=failed(&mut store,&lease,"paid",true);cache(&dir,&store,&[(&paid,"3.1")],1_800_001);
        assert!(!store.release_absent_budget_hold(&lease,&paid,Some("review")).unwrap());
        assert!(store.review_budget_billing(&lease,&paid,"review",Some(3_200_000)).is_err());
        store.review_budget_billing(&lease,&paid,"review",Some(3_100_000)).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,3_100_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn reviewed_old_unknown_accepts_only_the_exact_fresh_charge_without_changing_execution() {
        let (dir,mut store,lease)=fixture();let id=failed(&mut store,&lease,"old-unknown",false);
        cache(&dir,&store,&[(&id,"167.6964")],1_800_010);
        let now=chrono::Utc::now().timestamp_millis();
        store.connection.execute("UPDATE bridge_budget_executions SET execution_state='unknown',started_at_ms=?1,finished_at_ms=NULL,result_ciphertext=NULL,result_hash=NULL WHERE budget_id=?2",params![now-172_800_000,id]).unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Pending);
        assert!(store.review_budget_billing(&lease,&id,"operator-audit",Some(185_000_000)).is_err());
        store.review_budget_billing(&lease,&id,"operator-audit",Some(167_696_400)).unwrap();
        store.review_budget_billing(&lease,&id,"operator-audit",Some(167_696_400)).unwrap();
        assert_eq!(store.budget_execution(&id).unwrap().unwrap().state,ExecutionState::Unknown);
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,167_696_400);
        assert_eq!(store.budget_receipt_events(lease.generation(),0,10).unwrap().len(),1);
        cleanup(dir,store,lease);
    }
    #[test]
    fn release_transaction_failure_preserves_capacity_and_no_fake_bill() {
        let (dir,mut store,lease)=fixture();let id=failed(&mut store,&lease,"rollback",true);absence(&dir,&store,&id,61000);
        store.connection.execute_batch("CREATE TRIGGER fail_release BEFORE UPDATE OF stage ON bridge_capacity_slots WHEN NEW.stage='released' BEGIN SELECT RAISE(ABORT,'fixture rollback'); END").unwrap();
        assert!(store.release_absent_budget_hold(&lease,&id,None).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000);
        assert!(store.budget_hold_release(&id).unwrap().is_none());assert!(store.latest_budget_receipt_event(&id).unwrap().is_none());
        store.connection.execute_batch("DROP TRIGGER fail_release").unwrap();
        assert!(store.release_absent_budget_hold(&lease,&id,None).unwrap());cleanup(dir,store,lease);
    }
}
