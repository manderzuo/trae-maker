//! Execution/result facts stay independent from receipt arrival and Key activity.
use super::{bridge_billing::BridgeBillingStore,bridge_budget::{self,CapacityTransition},bridge_budget_lease::BridgeBudgetLease,bridge_prepared::{self,PreparedAuthorization}};
use rusqlite::{params,Connection,OptionalExtension,Transaction,TransactionBehavior};
use serde::{Deserialize,Serialize};
use serde_json::Value;

#[derive(Debug,Clone,Copy,PartialEq,Eq,Serialize,Deserialize)]
#[serde(rename_all="snake_case")]
pub(super) enum ExecutionState { Running,Unknown,Succeeded,Failed }

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub(super) struct BudgetExecution {
    pub budget_id:String,pub request_id:String,pub core_key_id:String,pub account_ref:String,
    pub bridge_instance_id:String,pub session_ref:String,pub step_kind:String,
    pub state:ExecutionState,pub task_ref:Option<String>,pub started_at_ms:i64,
    pub finished_at_ms:Option<i64>,pub result_available:bool,
}

#[derive(Clone,PartialEq,Eq,Serialize,Deserialize)]
pub(super) struct ExecutionIdentity {
    pub authorization:PreparedAuthorization,pub session_ref:String,pub step_kind:String,
}

pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[
    ("table","bridge_budget_executions","CREATE TABLE bridge_budget_executions (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_prepared_budgets(budget_id),
        request_id TEXT UNIQUE NOT NULL,
        core_key_id TEXT NOT NULL REFERENCES bridge_core_api_keys(key_id),
        account_ref TEXT NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
        bridge_instance_id TEXT NOT NULL,
        session_ref TEXT NOT NULL CHECK(length(session_ref) BETWEEN 1 AND 256),
        step_kind TEXT NOT NULL CHECK(step_kind IN ('assist','chat','video')),
        execution_state TEXT NOT NULL CHECK(execution_state IN ('running','unknown','succeeded','failed')),
        task_ref TEXT CHECK(task_ref IS NULL OR length(task_ref) BETWEEN 1 AND 256),
        started_at_ms INTEGER NOT NULL CHECK(typeof(started_at_ms)='integer' AND started_at_ms>=0),
        finished_at_ms INTEGER CHECK(finished_at_ms IS NULL OR (typeof(finished_at_ms)='integer' AND finished_at_ms>=started_at_ms)),
        result_ciphertext BLOB,
        result_hash BLOB,
        UNIQUE(account_ref,session_ref),
        CHECK((execution_state IN ('running','unknown') AND finished_at_ms IS NULL AND result_ciphertext IS NULL AND result_hash IS NULL) OR
              (execution_state IN ('succeeded','failed') AND finished_at_ms IS NOT NULL AND result_ciphertext IS NOT NULL AND typeof(result_ciphertext)='blob' AND length(result_ciphertext)>0 AND result_hash IS NOT NULL AND typeof(result_hash)='blob' AND length(result_hash)=32)),
        CHECK(execution_state!='succeeded' OR step_kind!='video' OR task_ref IS NOT NULL)
    )"),
    ("index","bridge_budget_executions_pending","CREATE INDEX bridge_budget_executions_pending ON bridge_budget_executions(account_ref,started_at_ms,budget_id)"),
];
fn db_error(error:rusqlite::Error)->String {format!("budget execution database error: {error}")}
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(db_error)?;}
    // A v3 send intent cannot be replayed as new work. Recover its identity as
    // unknown, without inventing completion or financial evidence.
    let mut after=String::new();
    loop {
        let ids={let mut stmt=tx.prepare("SELECT budget_id FROM bridge_prepared_budgets WHERE state='send_intent' AND budget_id>?1 ORDER BY budget_id LIMIT 128").map_err(db_error)?;
            let rows=stmt.query_map([&after],|r|r.get::<_,String>(0)).map_err(db_error)?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?};
        if ids.is_empty() {break;}
        for id in &ids {
            let identity=bridge_prepared::sent_execution_identity(tx,id)?;
            let prepared:i64=tx.query_row("SELECT created_at_ms FROM bridge_prepared_budgets WHERE budget_id=?1",[id],|r|r.get(0)).map_err(db_error)?;
            register_send(tx,&identity,prepared,ExecutionState::Unknown)?;
        }
        after=ids.last().unwrap().clone();
    } Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let actual:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((found,sql))=actual else {return Err(format!("missing budget execution object: {name}"))};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?!=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {return Err(format!("invalid budget execution schema: {name}"));}
    } Ok(())
}
impl ExecutionState {
    fn as_str(self)->&'static str {match self {Self::Running=>"running",Self::Unknown=>"unknown",Self::Succeeded=>"succeeded",Self::Failed=>"failed"}}
    fn parse(value:&str)->Result<Self,String> {match value {"running"=>Ok(Self::Running),"unknown"=>Ok(Self::Unknown),"succeeded"=>Ok(Self::Succeeded),"failed"=>Ok(Self::Failed),_=>Err("invalid execution state".into())}}
    fn terminal(self)->bool {matches!(self,Self::Succeeded|Self::Failed)}
}

pub(super) fn register_send(tx:&Transaction<'_>,identity:&ExecutionIdentity,started:i64,state:ExecutionState)->Result<(),String> {
    if !matches!(state,ExecutionState::Running|ExecutionState::Unknown) {return Err("invalid initial execution state".into());}
    let auth=&identity.authorization;
    let legacy_collision:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_requests WHERE request_id=?1)",[&auth.request_id],|r|r.get(0)).map_err(db_error)?;
    if legacy_collision {return Err("v2 execution request overlaps legacy attribution".into());}
    tx.execute("INSERT INTO bridge_budget_executions(budget_id,request_id,core_key_id,account_ref,bridge_instance_id,session_ref,step_kind,execution_state,started_at_ms)
        VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![auth.budget_id,auth.request_id,auth.core_key_id,auth.account_ref,auth.bridge_instance_id,identity.session_ref,identity.step_kind,state.as_str(),started]).map_err(db_error)?;
    Ok(())
}

fn read_execution(connection:&Connection,id:&str)->Result<Option<BudgetExecution>,String> {
    let row=connection.query_row("SELECT budget_id,request_id,core_key_id,account_ref,bridge_instance_id,session_ref,step_kind,execution_state,task_ref,started_at_ms,finished_at_ms,result_ciphertext IS NOT NULL
        FROM bridge_budget_executions WHERE budget_id=?1",[id],|r|Ok((BudgetExecution {
            budget_id:r.get(0)?,request_id:r.get(1)?,core_key_id:r.get(2)?,account_ref:r.get(3)?,bridge_instance_id:r.get(4)?,session_ref:r.get(5)?,step_kind:r.get(6)?,
            state:ExecutionState::Running,task_ref:r.get(8)?,started_at_ms:r.get(9)?,finished_at_ms:r.get(10)?,result_available:r.get(11)?},r.get::<_,String>(7)?))).optional().map_err(db_error)?;
    let Some((mut row,state))=row else {return Ok(None)};
    row.state=ExecutionState::parse(&state)?;
    let identity=bridge_prepared::sent_execution_identity(connection,id)?;
    let auth=&identity.authorization;
    if row.request_id!=auth.request_id || row.core_key_id!=auth.core_key_id || row.account_ref!=auth.account_ref || row.bridge_instance_id!=auth.bridge_instance_id
        || row.session_ref!=identity.session_ref || row.step_kind!=identity.step_kind {return Err("execution identity binding mismatch".into());}
    Ok(Some(row))
}
fn required_execution(connection:&Connection,id:&str)->Result<BudgetExecution,String> {read_execution(connection,id)?.ok_or("execution not sent".into())}
fn require_fact_owner(tx:&Transaction<'_>,lease:&BridgeBudgetLease,row:&BudgetExecution)->Result<(),String> {
    bridge_budget::require_fact_lease(tx,lease)?;
    if row.bridge_instance_id!=lease.instance_id() {return Err("execution belongs to another bridge instance".into());} Ok(())
}

#[derive(Serialize,Deserialize)]
struct ProtectedResult {
    version:u8,purpose:String,identity:ExecutionIdentity,execution:BudgetExecution,result:Value,
}
fn load_result(connection:&Connection,id:&str)->Result<Option<Value>,String> {
    let row=required_execution(connection,id)?;
    if !row.state.terminal() {return Ok(None);}
    let (cipher,hash):(Vec<u8>,Vec<u8>)=connection.query_row("SELECT result_ciphertext,result_hash FROM bridge_budget_executions WHERE budget_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).map_err(db_error)?;
    let plain=crate::vault::unprotect_blob(&cipher)?;
    let protected:ProtectedResult=serde_json::from_slice(&plain).map_err(|_|"protected execution result is invalid")?;
    let identity=bridge_prepared::sent_execution_identity(connection,id)?;
    if protected.version!=1 || protected.purpose!="bridge-budget-result" || protected.identity!=identity || protected.execution!=row
        || aiwork_core::canonical_json_hash(&protected.result).as_slice()!=hash.as_slice() {return Err("protected result binding mismatch".into());}
    Ok(Some(protected.result))
}
impl BridgeBillingStore {
    pub(super) fn budget_execution_for_request(&self,request_id:&str)->Result<Option<BudgetExecution>,String> {
        let id:Option<String>=self.connection.query_row("SELECT budget_id FROM bridge_budget_executions WHERE request_id=?1",[request_id],|r|r.get(0)).optional().map_err(db_error)?;
        id.map(|id|required_execution(&self.connection,&id)).transpose()
    }
    pub(super) fn budget_session_attribution(&self,account:&str,session:&str)->Result<Option<super::bridge_billing::CoreSessionLookup>,String> {
        let id:Option<String>=self.connection.query_row("SELECT budget_id FROM bridge_budget_executions WHERE account_ref=?1 AND session_ref=?2",params![account,session],|r|r.get(0)).optional().map_err(db_error)?;
        let Some(id)=id else {return Ok(None)};
        let row=required_execution(&self.connection,&id)?;
        Ok(Some(super::bridge_billing::CoreSessionLookup::Unique {request_id:row.request_id,core_key_id:row.core_key_id}))
    }
    /// Refresh lookup only. Do not use the legacy receipt writer for v2 money.
    pub(super) fn usage_session_for_request(&self,request_id:&str)->Result<super::bridge_billing::CoreBillingSessionLookup,String> {
        use super::bridge_billing::CoreBillingSessionLookup as Lookup;
        let legacy=self.core_session_for_request(request_id)?;
        let id:Option<String>=self.connection.query_row("SELECT budget_id FROM bridge_budget_executions WHERE request_id=?1",[request_id],|r|r.get(0)).optional().map_err(db_error)?;
        let Some(id)=id else {return Ok(legacy)};
        let row=required_execution(&self.connection,&id)?;
        let legacy_request:bool=self.connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_requests WHERE request_id=?1)",[request_id],|r|r.get(0)).map_err(db_error)?;
        if legacy_request || !matches!(legacy,Lookup::Missing) {return Ok(Lookup::Ambiguous);}
        match self.lookup_core_session_attribution(&row.account_ref,&row.session_ref)? {
            Some(super::bridge_billing::CoreSessionLookup::Unique {request_id:request,core_key_id:key}) if request==row.request_id && key==row.core_key_id=>{},
            _=>return Ok(Lookup::Ambiguous),
        }
        Ok(Lookup::Unique {core_key_id:row.core_key_id,account_ref:row.account_ref,session_id:row.session_ref,associated_at_ms:row.started_at_ms})
    }
    pub(super) fn budget_execution(&self,budget_id:&str)->Result<Option<BudgetExecution>,String> {read_execution(&self.connection,budget_id)}
    pub(super) fn bind_budget_task(&mut self,lease:&BridgeBudgetLease,budget_id:&str,task:&str)->Result<(),String> {
        if task.is_empty() || task.len()>256 || task.trim()!=task || task.chars().any(char::is_control) {return Err("invalid task reference".into());}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let row=required_execution(&tx,budget_id)?;require_fact_owner(&tx,lease,&row)?;
        if let Some(old)=&row.task_ref {return if old==task {Ok(())} else {Err("immutable task reference conflict".into())};}
        if row.step_kind!="video" || row.state.terminal() {return Err("task binding is not applicable".into());}
        tx.execute("UPDATE bridge_budget_executions SET task_ref=?1 WHERE budget_id=?2 AND task_ref IS NULL",params![task,budget_id]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
    pub(super) fn finish_budget_result(&mut self,lease:&BridgeBudgetLease,budget_id:&str,state:ExecutionState,result:&Value,now:i64)->Result<(),String> {
        if !state.terminal() || !result.is_object() {return Err("terminal execution requires a structured result".into());}
        let previous=required_execution(&self.connection,budget_id)?;
        if previous.state.terminal() {
            return if previous.state==state && load_result(&self.connection,budget_id)?.as_ref()==Some(result) {Ok(())} else {Err("immutable terminal result conflict".into())};
        }
        if now<previous.started_at_ms || (state==ExecutionState::Succeeded && previous.step_kind=="video" && previous.task_ref.is_none()) {return Err("terminal execution is missing task or valid completion time".into());}
        let mut terminal=previous.clone();terminal.state=state;terminal.finished_at_ms=Some(now);terminal.result_available=true;
        let identity=bridge_prepared::sent_execution_identity(&self.connection,budget_id)?;
        let protected=ProtectedResult {version:1,purpose:"bridge-budget-result".into(),identity,execution:terminal,result:result.clone()};
        let plain=serde_json::to_vec(&protected).map_err(|_|"result encoding failed")?;
        if plain.len()>8*1024*1024 {return Err("result exceeds protected storage limit".into());}
        let cipher=crate::vault::protect_blob(&plain)?;let hash=aiwork_core::canonical_json_hash(result);
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let current=required_execution(&tx,budget_id)?;require_fact_owner(&tx,lease,&current)?;
        if current.state.terminal() {
            return if current.state==state && load_result(&tx,budget_id)?.as_ref()==Some(result) {Ok(())} else {Err("terminal result CAS conflict".into())};
        }
        if current!=previous {return Err("execution changed during result protection; reread required".into());}
        bridge_budget::transition_in_transaction(&tx,lease,budget_id,CapacityTransition::ExecutionTerminal)?;
        tx.execute("UPDATE bridge_budget_executions SET execution_state=?1,finished_at_ms=?2,result_ciphertext=?3,result_hash=?4 WHERE budget_id=?5 AND execution_state IN ('running','unknown')",params![state.as_str(),now,cipher,hash.as_slice(),budget_id]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
    pub(super) fn load_budget_result(&self,budget_id:&str)->Result<Option<Value>,String> {load_result(&self.connection,budget_id)}
    pub(super) fn mark_budget_execution_unknown(&mut self,lease:&BridgeBudgetLease,budget_id:&str)->Result<(),String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let row=required_execution(&tx,budget_id)?;require_fact_owner(&tx,lease,&row)?;
        if row.state.terminal() {return Ok(());}
        tx.execute("UPDATE bridge_budget_executions SET execution_state='unknown' WHERE budget_id=?1 AND execution_state='running'",[budget_id]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome},bridge_budget::CapacityTransition};
    fn send(store:&mut BridgeBillingStore,lease:&BridgeBudgetLease,request:&str)->String {
        let mut value=input();value.request_id=request.into();
        let prepared=store.prepare_budget(lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(lease,&prepared,20).unwrap() else {panic!("first consumer expected")};
        assert!(store.mark_budget_send_intent(lease,&ctx.budget.authorization.budget_id,&ctx.consume_epoch).unwrap());
        prepared.authorization.budget_id
    }
    fn now()->i64 {chrono::Utc::now().timestamp_millis()+1000}

    #[test]
    fn send_intent_persists_unique_session_before_network_permission() {
        let (dir,mut store,lease)=fixture();
        let id=send(&mut store,&lease,"request-video");
        let execution=store.budget_execution(&id).unwrap().expect("send permission requires durable execution");
        assert_eq!(execution.request_id,"request-video");assert_eq!(execution.core_key_id,"key-a");
        assert!(execution.session_ref.starts_with("core-v2-budget_"));assert_eq!(execution.state,ExecutionState::Running);
        assert_eq!(execution.finished_at_ms,None);assert!(!execution.result_available);
        drop(store);let store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.budget_execution(&id).unwrap().unwrap(),execution);
        cleanup(dir,store,lease);
    }
    #[test]
    fn completed_result_delivered_with_receipt_pending_even_when_key_disabled() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        store.bind_budget_task(&lease,&id,"task-1").unwrap();
        store.connection.execute("UPDATE bridge_core_api_keys SET active=0",[]).unwrap();
        let output=serde_json::json!({"video_url":"https://fixture.invalid/result.mp4"});
        let finished=now();store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&output,finished).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().pending,0);
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000);
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.load_budget_result(&id).unwrap(),Some(output.clone()));
        store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&output,finished+100).unwrap();
        assert_eq!(store.budget_execution(&id).unwrap().unwrap().finished_at_ms,Some(finished));
        assert!(store.bind_budget_task(&lease,&id,"task-other").is_err());
        assert!(store.finish_budget_result(&lease,&id,ExecutionState::Failed,&output,finished+200).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn receipt_before_result_keeps_actual_once_and_unknown_preserves_hold() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        store.mark_budget_execution_unknown(&lease,&id).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        assert_eq!(store.budget_execution(&id).unwrap().unwrap().state,ExecutionState::Unknown);
        store.transition_capacity(&lease,&id,CapacityTransition::Actual(50_000_000)).unwrap();
        store.bind_budget_task(&lease,&id,"task-1").unwrap();
        store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&serde_json::json!({"ok":true}),now()).unwrap();
        let totals=store.capacity_totals("account").unwrap();assert_eq!((totals.pending,totals.awaiting_receipt,totals.confirmed),(0,0,50_000_000));
        cleanup(dir,store,lease);
    }
    #[test]
    fn unsent_and_taskless_video_cannot_report_success() {
        let (dir,mut store,lease)=fixture();let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        assert!(store.finish_budget_result(&lease,&prepared.authorization.budget_id,ExecutionState::Succeeded,&serde_json::json!({}),now()).is_err());
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("consume")};
        store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&ctx.consume_epoch).unwrap();
        assert!(store.finish_budget_result(&lease,&prepared.authorization.budget_id,ExecutionState::Succeeded,&serde_json::json!({}),now()).is_err());
        store.finish_budget_result(&lease,&prepared.authorization.budget_id,ExecutionState::Failed,&serde_json::json!({"code":"upstream_failed"}),now()).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000,"failed upstream execution is not proof of no charge");
        cleanup(dir,store,lease);
    }
    #[test]
    fn protected_results_reject_cross_budget_copy_and_never_become_empty_success() {
        let (dir,mut store,lease)=fixture();let a=send(&mut store,&lease,"request-a");let b=send(&mut store,&lease,"request-b");
        for id in [&a,&b] {store.bind_budget_task(&lease,id,&format!("task-{id}")).unwrap();
            store.finish_budget_result(&lease,id,ExecutionState::Succeeded,&serde_json::json!({"secret_output":id}),now()).unwrap();}
        let cipher:Vec<u8>=store.connection.query_row("SELECT result_ciphertext FROM bridge_budget_executions WHERE budget_id=?1",[&a],|r|r.get(0)).unwrap();
        assert!(!String::from_utf8_lossy(&cipher).contains("secret_output"));
        store.connection.execute("UPDATE bridge_budget_executions SET result_ciphertext=(SELECT result_ciphertext FROM bridge_budget_executions WHERE budget_id=?1) WHERE budget_id=?2",rusqlite::params![b,a]).unwrap();
        assert!(store.load_budget_result(&a).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn new_session_is_visible_to_existing_usage_refresh_without_legacy_billing_mode() {
        use super::super::bridge_billing::{match_core_usage_sessions,CoreUsageSessionMatch};
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        let execution=store.budget_execution(&id).unwrap().unwrap();
        let pair=(execution.account_ref.clone(),execution.session_ref.clone());
        let matches=match_core_usage_sessions(&dir,&[pair.clone()]).unwrap();
        assert_eq!(matches.get(&pair),Some(&CoreUsageSessionMatch::Unique {request_id:"request-video".into(),core_key_id:"key-a".into()}));
        let pending=store.pending_core_session_accounts().unwrap();
        assert_eq!(pending,vec![("account".into(),execution.started_at_ms)]);
        let legacy_count:i64=store.connection.query_row("SELECT COUNT(*) FROM bridge_core_requests",[],|r|r.get(0)).unwrap();
        assert_eq!(legacy_count,0,"v2 must not fabricate a legacy billing mode");
        store.transition_capacity(&lease,&id,CapacityTransition::Actual(42_000_000)).unwrap();
        assert!(store.pending_core_session_accounts().unwrap().is_empty());
        // Session ownership remains readable after settlement.
        assert_eq!(match_core_usage_sessions(&dir,&[pair.clone()]).unwrap().get(&pair),matches.get(&pair));
        cleanup(dir,store,lease);
    }
    #[test]
    fn close_blocks_new_sends_but_allows_inflight_results_and_actual_charge() {
        let (dir,mut store,mut lease)=fixture();let id=send(&mut store,&lease,"request-video");
        store.bind_budget_task(&lease,&id,"task-1").unwrap();lease.begin_clean_close();
        let mut another=input();another.request_id="request-next".into();
        assert!(store.prepare_budget(&lease,&another,None,30).is_err());
        store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&serde_json::json!({"ok":true}),now()).unwrap();
        store.transition_capacity(&lease,&id,CapacityTransition::Actual(50_000_000)).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn migration_recovers_existing_send_as_unknown_and_keeps_capacity() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        store.connection.execute_batch("DROP TABLE bridge_budget_receipt_events; DROP TABLE bridge_budget_receipts; DROP TABLE bridge_budget_executions; UPDATE bridge_schema_meta SET schema_version=3").unwrap();
        drop(store);let store=BridgeBillingStore::open(&dir).unwrap();
        let execution=store.budget_execution(&id).unwrap().unwrap();
        assert_eq!(execution.state,ExecutionState::Unknown);assert!(!execution.result_available);
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn legacy_session_collision_is_ambiguous_not_attributed_to_either_key() {
        use super::super::bridge_billing::{CoreSessionLookup,CoreBillingSessionLookup};
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        let execution=store.budget_execution(&id).unwrap().unwrap();
        assert!(matches!(store.usage_session_for_request("request-video").unwrap(),CoreBillingSessionLookup::Unique {..}));
        store.record_core_request("legacy-request","key-other").unwrap();
        store.record_core_session_attempt("legacy-request","account",&execution.session_ref).unwrap();
        assert_eq!(store.lookup_core_session_attribution("account",&execution.session_ref).unwrap(),Some(CoreSessionLookup::Ambiguous));
        assert_eq!(store.usage_session_for_request("request-video").unwrap(),CoreBillingSessionLookup::Ambiguous);
        cleanup(dir,store,lease);
    }
    #[test]
    fn result_write_failure_rolls_back_capacity_transition() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-video");
        store.bind_budget_task(&lease,&id,"task-1").unwrap();
        store.connection.execute_batch("CREATE TRIGGER injected_result_failure BEFORE UPDATE OF result_ciphertext ON bridge_budget_executions BEGIN SELECT RAISE(ABORT,'injected-result-failure'); END").unwrap();
        assert!(store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&serde_json::json!({"ok":true}),now()).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        assert_eq!(store.load_budget_result(&id).unwrap(),None);
        assert_eq!(store.budget_execution(&id).unwrap().unwrap().state,ExecutionState::Running);
        cleanup(dir,store,lease);
    }
}
