//! V2 locally-confirmed receipt facts and an atomic, replayable event outbox.
//! A post-terminal session observation is an explicit local confirmation policy,
//! not a promise that the upstream publishes an immutable Final flag.
use super::{bridge_billing::BridgeBillingStore,bridge_budget::{self,CapacityTransition},bridge_budget_lease::BridgeBudgetLease,bridge_prepared::{self,PreparedAuthorization}};
use aiwork_core::{BillingReceipt,BillingReceiptStatus,CreditAmount};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD,Engine};
use rusqlite::{params,OptionalExtension,Transaction,TransactionBehavior};
use crate::commands::usage_history::BudgetUsageSourceConflict;
use serde::{Deserialize,Serialize};

#[derive(Debug,Clone,Copy,PartialEq,Eq)]
pub(super) enum ReceiptChange {Pending,Created,Duplicate,Conflict}
#[derive(Debug,Clone,Serialize,Deserialize)]
pub(super) struct BudgetReceiptEvent {
    pub wire_version:u8,pub generation:String,pub sequence:i64,pub event_id:String,
    pub request_id:String,pub core_key_id:String,pub budget_id:String,pub account_ref:String,
    pub bridge_instance_id:String,pub kind:String,pub confirmation_policy:String,
    pub receipt:Option<BillingReceipt>,pub conflict:Option<BudgetUsageSourceConflict>,pub evidence_hash:String,
}
const SESSION_POLICY:&str="post-terminal-session-observation-v1";
const NO_SEND_POLICY:&str="durable-local-no-send-v1";
pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[
    ("table","bridge_budget_receipts","CREATE TABLE bridge_budget_receipts (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_prepared_budgets(budget_id),
        receipt_state TEXT NOT NULL CHECK(receipt_state IN ('final','failed_no_charge','conflict')),
        actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR (typeof(actual_microcredits)='integer' AND actual_microcredits>=0)),
        receipt_json TEXT,
        semantic_hash BLOB NOT NULL CHECK(typeof(semantic_hash)='blob' AND length(semantic_hash)=32),
        confirmation_policy TEXT NOT NULL,
        source_read_started_at_ms INTEGER,
        source_query_end_ms INTEGER,
        CHECK(receipt_state='conflict' OR (actual_microcredits IS NOT NULL AND receipt_json IS NOT NULL)),
        CHECK(receipt_state!='failed_no_charge' OR (actual_microcredits IS NOT NULL AND actual_microcredits=0))
    )"),
    ("table","bridge_budget_receipt_events","CREATE TABLE bridge_budget_receipt_events (
        sequence INTEGER PRIMARY KEY AUTOINCREMENT,
        generation TEXT NOT NULL,
        event_id TEXT UNIQUE NOT NULL,
        budget_id TEXT NOT NULL REFERENCES bridge_budget_receipts(budget_id),
        event_kind TEXT NOT NULL CHECK(event_kind IN ('final','failed_no_charge','conflict')),
        evidence_hash BLOB NOT NULL CHECK(typeof(evidence_hash)='blob' AND length(evidence_hash)=32),
        payload_json TEXT NOT NULL,
        UNIQUE(budget_id,event_kind,evidence_hash)
    )"),
    ("index","bridge_budget_receipt_events_generation","CREATE INDEX bridge_budget_receipt_events_generation ON bridge_budget_receipt_events(generation,sequence)"),
];
fn db_error(error:rusqlite::Error)->String {format!("budget receipt database error: {error}")}
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(db_error)?;} Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let actual:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((found,sql))=actual else {return Err(format!("missing budget receipt object: {name}"))};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?!=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {return Err(format!("invalid budget receipt schema: {name}"));}
    } Ok(())
}
fn receipt_hash(receipt:&BillingReceipt)->[u8;32] {
    aiwork_core::canonical_json_hash(&serde_json::json!({"request_id":receipt.request_id,"status":receipt.status,
        "actual_microcredits":receipt.actual_credits.map(|amount|amount.as_microcredits()),"unit":receipt.unit,"source_ref":receipt.source_ref,"task_ref":receipt.task_ref}))
}
fn persist_fact(tx:&Transaction<'_>,lease:&BridgeBudgetLease,auth:&PreparedAuthorization,receipt:&BillingReceipt,policy:&str,source_times:Option<(i64,i64)>)->Result<ReceiptChange,String> {
    bridge_budget::require_fact_lease(tx,lease)?;
    if auth.bridge_instance_id!=lease.instance_id() || auth.request_id!=receipt.request_id || receipt.unit!="credits" || receipt.observed_at_ms<0 || receipt.source_ref.is_empty() {return Err("receipt identity or evidence mismatch".into());}
    let amount=receipt.actual_credits.ok_or("confirmed receipt has no exact amount")?;
    let kind=match receipt.status {BillingReceiptStatus::Final=>"final",BillingReceiptStatus::FailedNoCharge if amount.as_microcredits()==0=>"failed_no_charge",_=>return Err("unsupported confirmed receipt status".into())};
    let semantic=receipt_hash(receipt);
    let stored:Option<(String,Vec<u8>)>=tx.query_row("SELECT receipt_state,semantic_hash FROM bridge_budget_receipts WHERE budget_id=?1",[&auth.budget_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
    let (change,event_kind)=match stored {
        None=>{
            if kind=="final" {bridge_budget::transition_in_transaction(tx,lease,&auth.budget_id,CapacityTransition::Actual(amount.as_microcredits()))?;}
            let receipt_json=serde_json::to_string(receipt).map_err(|_|"receipt encoding failed")?;
            tx.execute("INSERT INTO bridge_budget_receipts(budget_id,receipt_state,actual_microcredits,receipt_json,semantic_hash,confirmation_policy,source_read_started_at_ms,source_query_end_ms)
                VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",params![auth.budget_id,kind,amount.as_microcredits(),receipt_json,semantic.as_slice(),policy,source_times.map(|t|t.0),source_times.map(|t|t.1)]).map_err(db_error)?;
            (ReceiptChange::Created,kind)
        },
        Some((state,hash)) if hash==semantic=>return Ok(if state=="conflict" {ReceiptChange::Conflict}else{ReceiptChange::Duplicate}),
        Some((_state,_hash))=>{
            // Preserve the first money fact. An incompatible later source is
            // evidence for reconciliation, never a second automatic charge.
            tx.execute("UPDATE bridge_budget_receipts SET receipt_state='conflict' WHERE budget_id=?1",[&auth.budget_id]).map_err(db_error)?;
            // Unknown account exposure cannot be silently cleared by rebase.
            // Other accounts and Keys remain independent; result reads continue.
            tx.execute("UPDATE bridge_capacity_accounts SET rebase_state='fenced',owner_nonce=?1,fence_epoch=fence_epoch+1 WHERE account_ref=?2 AND rebase_state='open'",
                params![format!("receipt-conflict:{}",auth.budget_id),auth.account_ref]).map_err(db_error)?;
            (ReceiptChange::Conflict,"conflict")
        },
    };
    let hash_text=URL_SAFE_NO_PAD.encode(semantic);
    let event_id=format!("budget-event-{}",URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&serde_json::json!({"budget":auth.budget_id,"kind":event_kind,"evidence":hash_text}))));
    let event=BudgetReceiptEvent {wire_version:2,generation:lease.generation().into(),sequence:0,event_id:event_id.clone(),request_id:auth.request_id.clone(),
        core_key_id:auth.core_key_id.clone(),budget_id:auth.budget_id.clone(),account_ref:auth.account_ref.clone(),bridge_instance_id:auth.bridge_instance_id.clone(),
        kind:event_kind.into(),confirmation_policy:policy.into(),receipt:Some(receipt.clone()),conflict:None,evidence_hash:hash_text};
    let payload=serde_json::to_string(&event).map_err(|_|"receipt event encoding failed")?;
    tx.execute("INSERT INTO bridge_budget_receipt_events(generation,event_id,budget_id,event_kind,evidence_hash,payload_json) VALUES (?1,?2,?3,?4,?5,?6)
        ON CONFLICT(budget_id,event_kind,evidence_hash) DO NOTHING",params![lease.generation(),event_id,auth.budget_id,event_kind,semantic.as_slice(),payload]).map_err(db_error)?;
    Ok(change)
}
fn persist_source_conflict(tx:&Transaction<'_>,lease:&BridgeBudgetLease,auth:&PreparedAuthorization,conflict:BudgetUsageSourceConflict)->Result<ReceiptChange,String> {
    bridge_budget::require_fact_lease(tx,lease)?;
    if auth.bridge_instance_id!=lease.instance_id() {return Err("conflict belongs to another bridge instance".into());}
    let hash=URL_SAFE_NO_PAD.decode(&conflict.evidence_hash).map_err(|_|"invalid source conflict hash")?;
    if hash.len()!=32 {return Err("invalid source conflict hash size".into());}
    tx.execute("INSERT INTO bridge_budget_receipts(budget_id,receipt_state,semantic_hash,confirmation_policy) VALUES (?1,'conflict',?2,?3)
        ON CONFLICT(budget_id) DO UPDATE SET receipt_state='conflict'",params![auth.budget_id,&hash,SESSION_POLICY]).map_err(db_error)?;
    tx.execute("UPDATE bridge_capacity_accounts SET rebase_state='fenced',owner_nonce=?1,fence_epoch=fence_epoch+1 WHERE account_ref=?2 AND rebase_state='open'",
        params![format!("receipt-conflict:{}",auth.budget_id),auth.account_ref]).map_err(db_error)?;
    let event_id=format!("budget-event-{}",URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&serde_json::json!({"budget":auth.budget_id,"kind":"conflict","evidence":conflict.evidence_hash}))));
    let event=BudgetReceiptEvent {wire_version:2,generation:lease.generation().into(),sequence:0,event_id:event_id.clone(),request_id:auth.request_id.clone(),core_key_id:auth.core_key_id.clone(),
        budget_id:auth.budget_id.clone(),account_ref:auth.account_ref.clone(),bridge_instance_id:auth.bridge_instance_id.clone(),kind:"conflict".into(),confirmation_policy:SESSION_POLICY.into(),
        receipt:None,evidence_hash:conflict.evidence_hash.clone(),conflict:Some(conflict)};
    let payload=serde_json::to_string(&event).map_err(|_|"conflict event encoding failed")?;
    tx.execute("INSERT INTO bridge_budget_receipt_events(generation,event_id,budget_id,event_kind,evidence_hash,payload_json) VALUES (?1,?2,?3,'conflict',?4,?5)
        ON CONFLICT(budget_id,event_kind,evidence_hash) DO NOTHING",params![lease.generation(),event_id,auth.budget_id,hash,payload]).map_err(db_error)?;
    Ok(ReceiptChange::Conflict)
}
fn reject_legacy_collision(tx:&Transaction<'_>,request:&str,account:&str,session:&str)->Result<(),String> {
    let collision:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_requests WHERE request_id=?1) OR EXISTS(SELECT 1 FROM bridge_core_upstream_sessions WHERE account_ref=?2 AND session_id=?3)",params![request,account,session],|r|r.get(0)).map_err(db_error)?;
    if collision {return Err("receipt session acquired a conflicting legacy attribution".into());} Ok(())
}
impl BridgeBillingStore {
    pub(super) fn confirm_budget_usage(&mut self,lease:&BridgeBudgetLease,budget_id:&str)->Result<ReceiptChange,String> {
        let execution=self.budget_execution(budget_id)?.ok_or("budget has not been sent")?;
        let Some(finished)=execution.finished_at_ms else {return Ok(ReceiptChange::Pending)};
        use super::bridge_billing::CoreBillingSessionLookup;
        if !matches!(self.usage_session_for_request(&execution.request_id)?,CoreBillingSessionLookup::Unique {core_key_id,account_ref,session_id,..}
            if core_key_id==execution.core_key_id && account_ref==execution.account_ref && session_id==execution.session_ref) {return Err("budget session attribution ambiguous".into());}
        let identity=bridge_prepared::sent_execution_identity(&self.connection,budget_id)?;
        if let Some(conflict)=crate::commands::usage_history::budget_usage_source_conflict(&self.data_dir,&execution.account_ref,&execution.session_ref,finished)? {
            let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
            if identity!=bridge_prepared::sent_execution_identity(&tx,budget_id)? {return Err("conflict preparation binding changed".into());}
            reject_legacy_collision(&tx,&execution.request_id,&execution.account_ref,&execution.session_ref)?;
            let result=persist_source_conflict(&tx,lease,&identity.authorization,conflict)?;tx.commit().map_err(db_error)?;return Ok(result);
        }
        let Some(source)=crate::commands::usage_history::budget_usage_receipt_evidence(&self.data_dir,&execution.account_ref,&execution.session_ref,&execution.request_id,&execution.core_key_id,finished)? else {return Ok(ReceiptChange::Pending)};
        let receipt=BillingReceipt {request_id:execution.request_id.clone(),status:BillingReceiptStatus::Final,actual_credits:Some(source.credits),unit:"credits".into(),
            source_ref:format!("trae-usage-session:{}",source.session_id),task_ref:execution.task_ref.clone(),observed_at_ms:source.observed_at_ms};
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let current=bridge_prepared::sent_execution_identity(&tx,budget_id)?;
        if identity!=current {return Err("receipt preparation binding changed".into());}
        reject_legacy_collision(&tx,&execution.request_id,&execution.account_ref,&execution.session_ref)?;
        let result=persist_fact(&tx,lease,&identity.authorization,&receipt,SESSION_POLICY,Some((source.read_started_at_ms,source.query_end_ms)))?;
        tx.commit().map_err(db_error)?;Ok(result)
    }
    pub(super) fn confirm_budget_no_send(&mut self,lease:&BridgeBudgetLease,budget_id:&str)->Result<ReceiptChange,String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let (auth,proof)=bridge_prepared::budget_no_send_disposition(&tx,budget_id)?;
        let receipt=BillingReceipt {request_id:auth.request_id.clone(),status:BillingReceiptStatus::FailedNoCharge,actual_credits:Some(CreditAmount::default()),unit:"credits".into(),
            source_ref:format!("aiwork-v2-local-no-send:{}",proof.cancel_ref),task_ref:None,observed_at_ms:proof.canceled_at_ms};
        let result=persist_fact(&tx,lease,&auth,&receipt,NO_SEND_POLICY,None)?;
        tx.commit().map_err(db_error)?;Ok(result)
    }
    pub(super) fn budget_receipt_events(&self,generation:&str,after:i64,limit:usize)->Result<Vec<BudgetReceiptEvent>,String> {
        if generation!=self.bridge_identity()?.1 {return Err("receipt event generation changed; recover pending requests before resetting cursor".into());}
        if after<0 || !(1..=256).contains(&limit) {return Err("invalid receipt event page".into());}
        let mut stmt=self.connection.prepare("SELECT sequence,generation,event_id,budget_id,event_kind,evidence_hash,payload_json FROM bridge_budget_receipt_events WHERE generation=?1 AND sequence>?2 ORDER BY sequence LIMIT ?3").map_err(db_error)?;
        let rows=stmt.query_map(params![generation,after,limit as i64],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,String>(4)?,r.get::<_,Vec<u8>>(5)?,r.get::<_,String>(6)?))).map_err(db_error)?;
        let mut events=Vec::new();
        for row in rows {
            let (sequence,stored_generation,event_id,budget_id,kind,hash,json)=row.map_err(db_error)?;
            let mut event:BudgetReceiptEvent=serde_json::from_str(&json).map_err(|_|"invalid stored receipt event")?;
            let evidence_valid=match (&event.receipt,&event.conflict) {
                (Some(receipt),None)=>receipt_hash(receipt).as_slice()==hash,
                (None,Some(conflict))=>kind=="conflict" && conflict.evidence_hash==URL_SAFE_NO_PAD.encode(&hash),
                _=>false,
            };
            let auth=bridge_prepared::stored_budget_authorization(&self.connection,&budget_id)?;
            if event.wire_version!=2 || event.generation!=stored_generation || event.event_id!=event_id || event.budget_id!=budget_id || event.kind!=kind
                || !evidence_valid || event.evidence_hash!=URL_SAFE_NO_PAD.encode(&hash) || event.request_id!=auth.request_id || event.core_key_id!=auth.core_key_id
                || event.account_ref!=auth.account_ref || event.bridge_instance_id!=auth.bridge_instance_id {return Err("stored receipt event binding mismatch".into());}
            event.sequence=sequence;events.push(event);
        } Ok(events)
    }
}
#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome},bridge_execution::ExecutionState};
    use serde_json::json;
    fn send(store:&mut BridgeBillingStore,lease:&BridgeBudgetLease,request:&str)->String {
        let mut value=input();value.request_id=request.into();
        let prepared=store.prepare_budget(lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(lease,&prepared,20).unwrap() else {panic!("consume")};
        assert!(store.mark_budget_send_intent(lease,&prepared.authorization.budget_id,&ctx.consume_epoch).unwrap());
        store.bind_budget_task(lease,&prepared.authorization.budget_id,&format!("task-{request}")).unwrap();
        store.finish_budget_result(lease,&prepared.authorization.budget_id,ExecutionState::Succeeded,&json!({"ok":true}),chrono::Utc::now().timestamp_millis()+1000).unwrap();
        prepared.authorization.budget_id
    }
    fn cache(dir:&std::path::Path,store:&BridgeBillingStore,ids:&[(&str,&str)],time_offset:i64) {
        let mut rows=serde_json::Map::new();let mut observations=serde_json::Map::new();
        for (id,amount) in ids {
            let e=store.budget_execution(id).unwrap().unwrap();let observed=e.finished_at_ms.unwrap()+time_offset;
            rows.insert(e.session_ref.clone(),json!({"session_id":e.session_ref,"usage_time":observed/1000,"date":"2026-09-27","model_name":"seedance","credits_float":amount,
                "core_request_id":e.request_id,"core_key_id":e.core_key_id,"ambiguous":false,"core_attribution_ambiguous":false}));
            let source_row=rows.get(&e.session_ref).unwrap().clone();
            observations.insert(e.session_ref,json!({"read_started_at_ms":observed,"completed_at_ms":observed+100,"query_end_ms":observed+300000,"complete":true,"row":source_row}));
        }
        let file=dir.join("data").join("usage_history.json");std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        crate::fs_utils::write_json(&file,&json!({"accounts":{"account":{"name":"A","daily":{},"session_usage":rows,"session_observations":observations}}})).unwrap();
    }
    #[test]
    fn exact_receipt_and_outbox_commit_once_even_when_actual_exceeds_hold() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");cache(&dir,&store,&[(&id,"50.123456")],2000);
        store.connection.execute("UPDATE bridge_core_api_keys SET active=0",[]).unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Created);
        let events=store.budget_receipt_events(lease.generation(),0,10).unwrap();assert_eq!(events.len(),1);
        assert_eq!(events[0].receipt.as_ref().unwrap().actual_credits.unwrap().to_string(),"50.123456");assert_eq!(events[0].core_key_id,"key-a");
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_123_456);
        cache(&dir,&store,&[(&id,"50.123456")],3000);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Duplicate);
        assert_eq!(store.budget_receipt_events(lease.generation(),0,10).unwrap().len(),1);
        drop(store);let store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.budget_receipt_events(lease.generation(),0,10).unwrap()[0].event_id,events[0].event_id);
        cleanup(dir,store,lease);
    }
    #[test]
    fn stale_source_stays_pending_and_changed_actual_emits_conflict_without_double_charge() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");cache(&dir,&store,&[(&id,"12")],-2000);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Pending);
        cache(&dir,&store,&[(&id,"12")],2000);assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Created);
        cache(&dir,&store,&[(&id,"13")],3000);assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Conflict);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Conflict);
        let events=store.budget_receipt_events(lease.generation(),0,10).unwrap();assert_eq!(events.len(),2);assert_eq!(events[1].kind,"conflict");
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,12_000_000);
        let mut next=input();next.request_id="request-new".into();assert!(store.prepare_budget(&lease,&next,None,30).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn no_send_receipt_comes_only_from_durable_disposition() {
        let (dir,mut store,lease)=fixture();let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        assert!(store.confirm_budget_no_send(&lease,&prepared.authorization.budget_id).is_err());
        assert!(matches!(store.consume_budget(&lease,&prepared,1000).unwrap(),ConsumeOutcome::Rejected(_)));
        assert_eq!(store.confirm_budget_no_send(&lease,&prepared.authorization.budget_id).unwrap(),ReceiptChange::Created);
        let events=store.budget_receipt_events(lease.generation(),0,10).unwrap();assert_eq!(events.len(),1);
        assert_eq!(events[0].receipt.as_ref().unwrap().status,aiwork_core::BillingReceiptStatus::FailedNoCharge);assert_eq!(events[0].receipt.as_ref().unwrap().actual_credits.unwrap().as_microcredits(),0);
        let sent=send(&mut store,&lease,"request-sent");assert!(store.confirm_budget_no_send(&lease,&sent).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn outbox_failure_rolls_back_receipt_and_capacity_and_can_retry() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");cache(&dir,&store,&[(&id,"15")],2000);
        store.connection.execute_batch("CREATE TRIGGER inject_outbox_failure BEFORE INSERT ON bridge_budget_receipt_events BEGIN SELECT RAISE(ABORT,'injected-outbox-error'); END").unwrap();
        assert!(store.confirm_budget_usage(&lease,&id).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000);
        let count:i64=store.connection.query_row("SELECT COUNT(*) FROM bridge_budget_receipts",[],|r|r.get(0)).unwrap();assert_eq!(count,0);
        store.connection.execute_batch("DROP TRIGGER inject_outbox_failure").unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Created);
        cleanup(dir,store,lease);
    }
    #[test]
    fn events_are_bounded_and_wrong_generation_cannot_skip_recovery() {
        let (dir,mut store,lease)=fixture();let a=send(&mut store,&lease,"request-a");let b=send(&mut store,&lease,"request-b");cache(&dir,&store,&[(&a,"10"),(&b,"20")],2000);
        store.confirm_budget_usage(&lease,&b).unwrap();store.confirm_budget_usage(&lease,&a).unwrap();
        let first=store.budget_receipt_events(lease.generation(),0,1).unwrap();assert_eq!(first.len(),1);assert_eq!(first[0].budget_id,b);
        let second=store.budget_receipt_events(lease.generation(),first[0].sequence,1).unwrap();assert_eq!(second.len(),1);assert_eq!(second[0].budget_id,a);
        assert!(store.budget_receipt_events("other-generation",0,10).is_err());
        assert!(store.budget_receipt_events(lease.generation(),0,0).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn ambiguous_cache_source_emits_conflict_instead_of_silently_staying_pending() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");cache(&dir,&store,&[(&id,"12")],2000);
        let e=store.budget_execution(&id).unwrap().unwrap();
        let path=dir.join("data").join("usage_history.json");
        let mut value:serde_json::Value=crate::fs_utils::read_json(&path);
        value["accounts"]["account"]["session_usage"][&e.session_ref]["ambiguous"]=json!(true);
        value["accounts"]["account"]["session_observations"][&e.session_ref]["row"]["ambiguous"]=json!(true);
        crate::fs_utils::write_json(&path,&value).unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Conflict);
        assert_eq!(store.confirm_budget_usage(&lease,&id).unwrap(),ReceiptChange::Conflict);
        let events=store.budget_receipt_events(lease.generation(),0,10).unwrap();assert_eq!(events.len(),1);assert_eq!(events[0].kind,"conflict");
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000,"unknown actual must not be invented or released");
        assert!(store.pending_core_session_accounts().unwrap().is_empty(),"conflict is quarantined, not retried as ordinary pending");
        cleanup(dir,store,lease);
    }
    #[test]
    fn ten_keys_settle_concurrently_without_cross_attribution_or_duplicate_events() {
        let (dir,mut store,lease)=fixture();let mut ids=Vec::new();
        for index in 0..10 {
            let key=format!("key-{index}");let request=format!("request-{index}");
            store.connection.execute("INSERT INTO bridge_core_api_keys VALUES (?1,?1,1,1)",[&key]).unwrap();
            let mut value=input();value.request_id=request.clone();value.core_key_id=key.clone();value.hold_microcredits=1_000_000;
            let prepared=store.prepare_budget(&lease,&value,None,10).unwrap();
            let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("consume")};
            store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&ctx.consume_epoch).unwrap();
            store.bind_budget_task(&lease,&prepared.authorization.budget_id,&format!("task-{index}")).unwrap();
            store.finish_budget_result(&lease,&prepared.authorization.budget_id,ExecutionState::Succeeded,&json!({"ok":true}),chrono::Utc::now().timestamp_millis()+1000).unwrap();
            ids.push((prepared.authorization.budget_id,format!("{}.123456",index+1)));
        }
        cache(&dir,&store,&ids.iter().map(|(id,amount)|(id.as_str(),amount.as_str())).collect::<Vec<_>>(),2000);
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(10));let start=std::time::Instant::now();
        std::thread::scope(|scope| {
            let mut handles=Vec::new();
            for (id,_) in &ids {let b=barrier.clone();let d=&dir;let l=&lease;
                handles.push(scope.spawn(move || {let mut s=BridgeBillingStore::open(d).unwrap();b.wait();
                    assert_eq!(s.confirm_budget_usage(l,id).unwrap(),ReceiptChange::Created);
                    assert_eq!(s.confirm_budget_usage(l,id).unwrap(),ReceiptChange::Duplicate);
                }));}
            for handle in handles {handle.join().unwrap();}
        });
        let elapsed=start.elapsed();let events=store.budget_receipt_events(lease.generation(),0,256).unwrap();assert_eq!(events.len(),10);
        for index in 0..10 {
            let event=events.iter().find(|event|event.request_id==format!("request-{index}")).unwrap();
            assert_eq!(event.core_key_id,format!("key-{index}"));assert_eq!(event.budget_id,ids[index].0);
            assert_eq!(event.receipt.as_ref().unwrap().actual_credits.unwrap().to_string(),ids[index].1);
        }
        let totals=store.capacity_totals("account").unwrap();assert_eq!((totals.pending,totals.awaiting_receipt,totals.confirmed),(0,0,56_234_560));
        println!("isolated 10-key source-cache to bridge receipt/outbox: {} ms; not a public Core latency test",elapsed.as_millis());
        assert!(elapsed<std::time::Duration::from_secs(5));cleanup(dir,store,lease);
    }
}
