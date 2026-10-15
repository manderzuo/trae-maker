//! Local worker retirement is not an upstream terminal result or a zero bill.
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease};
use rusqlite::{params,OptionalExtension,Transaction,TransactionBehavior};
use super::bridge_execution::ExecutionState;

pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[("table","bridge_budget_worker_exits",
    "CREATE TABLE bridge_budget_worker_exits (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_budget_executions(budget_id),
        generation TEXT NOT NULL CHECK(length(generation) BETWEEN 1 AND 256),
        stopped_at_ms INTEGER NOT NULL CHECK(typeof(stopped_at_ms)='integer' AND stopped_at_ms>0)
    )")];
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql)in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(|e|e.to_string())?;}Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,sql)in SCHEMA_OBJECTS {
        let actual:(String,String)=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|_|"worker exit schema missing")?;
        if actual.0!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&actual.1)?!=super::bridge_billing::tokenize_sqlite_schema_sql(sql)? {return Err("worker exit schema invalid".into());}
    }Ok(())
}

impl BridgeBillingStore {
    /// Only the worker's final Drop, or the fenced prior-generation sweep, calls
    /// this. An observer disconnect or billing read cannot produce this fact.
    pub(super) fn record_budget_worker_exit(&mut self,lease:&BridgeBudgetLease,id:&str,stopped:i64)->Result<bool,String> {
        let execution=self.budget_execution(id)?.ok_or("worker execution missing")?;
        if execution.state!=ExecutionState::Unknown {return Ok(false);}
        if stopped<execution.started_at_ms || stopped<=0 || stopped>chrono::Utc::now().timestamp_millis()+60_000 {return Err("worker exit time invalid".into());}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
        super::bridge_budget::require_fact_lease(&tx,lease)?;
        let changed=tx.execute("INSERT INTO bridge_budget_worker_exits(budget_id,generation,stopped_at_ms)
            SELECT budget_id,?2,?3 FROM bridge_budget_executions WHERE budget_id=?1 AND execution_state='unknown' AND bridge_instance_id=?4
            ON CONFLICT(budget_id) DO NOTHING",params![id,lease.generation(),stopped,lease.instance_id()]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e|e.to_string())?;Ok(changed==1)
    }
    pub(super) fn budget_worker_exit_proof(&self,id:&str,now:i64)->Result<Option<serde_json::Value>,String> {
        let Some(e)=self.budget_execution(id)? else{return Ok(None)};
        if e.state!=ExecutionState::Unknown || now.saturating_sub(e.started_at_ms)<1_800_000 {return Ok(None);}
        let exit:Option<(String,i64)>=self.connection.query_row("SELECT generation,stopped_at_ms FROM bridge_budget_worker_exits WHERE budget_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
        let Some((generation,stopped))=exit else{return Ok(None)};
        if stopped<e.started_at_ms || now.saturating_sub(stopped)<60_000 {return Ok(None);}
        Ok(Some(serde_json::json!({"request_id":e.request_id,"core_key_id":e.core_key_id,"budget_id":e.budget_id,"account_ref":e.account_ref,
            "bridge_instance_id":e.bridge_instance_id,"task_ref":e.task_ref,"worker_absent_since_ms":stopped,"observed_at_ms":now,
            "review_ref":format!("bridge-worker-exit-v1:{generation}")})))
    }
    pub(super) fn unknown_budget_accounting_anchor(&self,id:&str,now:i64)->Result<Option<i64>,String> {
        Ok(self.budget_worker_exit_proof(id,now)?.and_then(|v|v["worker_absent_since_ms"].as_i64()).and_then(|v|v.checked_add(60_000)))
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_execution::ExecutionState,bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome}};
    #[test]
    #[ignore = "explicit read-only production backup migration rehearsal"]
    fn schema11_migrates_a_readonly_snapshot_without_changing_existing_facts() {
        use sha2::{Digest,Sha256};
        fn facts(db:&rusqlite::Connection)->std::collections::BTreeMap<String,Vec<Vec<u8>>> {
            let names:Vec<String>=db.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name<>'bridge_schema_meta'").unwrap().query_map([],|r|r.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
            names.into_iter().map(|name| {
                let mut stmt=db.prepare(&format!("SELECT * FROM \"{name}\"")).unwrap();let cols=stmt.column_count();let mut rows=stmt.query([]).unwrap();let mut hashes=Vec::new();
                while let Some(row)=rows.next().unwrap() {let mut h=Sha256::new();for i in 0..cols {h.update(format!("{:?}\0",row.get_ref(i).unwrap()).as_bytes());}hashes.push(h.finalize().to_vec());}
                hashes.sort();(name,hashes)
            }).collect()
        }
        assert_eq!(std::env::var("BRIDGE_MIGRATION_SNAPSHOT_ACK").as_deref(),Ok("1"));
        let source=std::path::PathBuf::from(std::env::var("BRIDGE_MIGRATION_SNAPSHOT").unwrap());
        assert!(source.is_absolute() && source.is_file());assert!(source.file_name().unwrap().to_string_lossy().contains("snapshot"));
        let original=std::fs::read(&source).unwrap();let dir=std::env::temp_dir().join(format!("worker-exit-migration-{:032x}",rand::random::<u128>()));std::fs::create_dir_all(&dir).unwrap();
        let path=dir.join("bridge-billing.sqlite3");std::fs::write(&path,&original).unwrap();
        let db=rusqlite::Connection::open_with_flags(&path,rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();let before=facts(&db);let identity:(String,String)=db.query_row("SELECT bridge_instance_id,event_generation FROM bridge_schema_meta WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();drop(db);
        let store=BridgeBillingStore::open(&dir).unwrap();let mut after=facts(&store.connection);let added=after.remove("bridge_budget_worker_exits").unwrap();assert!(added.is_empty());
        assert!(before==after,"migration changed existing rows");assert_eq!(store.bridge_identity().unwrap(),identity);
        assert_eq!(store.connection.query_row("SELECT schema_version FROM bridge_schema_meta WHERE singleton=1",[],|r|r.get::<_,i64>(0)).unwrap(),11);
        assert_eq!(store.connection.query_row("PRAGMA integrity_check",[],|r|r.get::<_,String>(0)).unwrap(),"ok");
        assert!(std::fs::read(&source).unwrap()==original,"input snapshot changed");
        println!("schema11 snapshot rehearsal: {} protected tables unchanged; source unchanged; no paid dispatch, lease or receipt creation",before.len());
        drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stopped_unknown_has_independent_proof_without_result_or_financial_release() {
        let (dir,mut store,lease)=fixture();let mut value=input();value.request_id="worker-stopped".into();
        let p=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(c)=store.consume_budget(&lease,&p,20).unwrap()else{panic!("consume")};
        let id=&p.authorization.budget_id;store.mark_budget_send_intent(&lease,id,&c.consume_epoch).unwrap();store.bind_budget_task(&lease,id,"worker-task").unwrap();
        let now=chrono::Utc::now().timestamp_millis();
        store.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",rusqlite::params![now-1_900_000,id]).unwrap();
        assert!(!store.record_budget_worker_exit(&lease,id,now-61_000).unwrap(),"a running worker cannot retire itself before finishing I/O");
        store.mark_budget_execution_unknown(&lease,id).unwrap();
        assert!(store.record_budget_worker_exit(&lease,id,now-61_000).unwrap());
        let proof=store.budget_worker_exit_proof(id,now).unwrap().unwrap();
        assert_eq!(proof["request_id"],"worker-stopped");assert_eq!(proof["task_ref"],"worker-task");
        assert!(store.budget_worker_exit_proof(id,now-60_000).unwrap().is_none(),"require a second observation after the quiet interval");
        assert_eq!(store.budget_execution(id).unwrap().unwrap().state,ExecutionState::Unknown);
        assert!(store.load_budget_result(id).unwrap().is_none());assert!(store.latest_budget_receipt_event(id).unwrap().is_none());
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        assert!(!store.record_budget_worker_exit(&lease,id,now).unwrap(),"repeat observation must preserve the first exit time");
        assert_eq!(store.budget_worker_exit_proof(id,now).unwrap().unwrap()["worker_absent_since_ms"],now-61_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn unknown_exact_usage_settles_once_after_worker_exit_without_faking_completion() {
        use super::super::bridge_receipts::ReceiptChange;
        let (dir,mut store,lease)=fixture();let mut value=input();value.request_id="unknown-accounting".into();
        let p=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(c)=store.consume_budget(&lease,&p,20).unwrap()else{panic!("consume")};
        let id=&p.authorization.budget_id;store.mark_budget_send_intent(&lease,id,&c.consume_epoch).unwrap();store.bind_budget_task(&lease,id,"unknown-task").unwrap();
        let now=chrono::Utc::now().timestamp_millis();
        store.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",rusqlite::params![now-1_900_000,id]).unwrap();
        store.mark_budget_execution_unknown(&lease,id).unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Pending);
        store.record_budget_worker_exit(&lease,id,now-120_000).unwrap();
        assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Pending,"no bill is not a zero bill");
        let e=store.budget_execution(id).unwrap().unwrap();
        let write=|request:&str,read:i64,amount:&str| {
            let row=serde_json::json!({"session_id":e.session_ref,"usage_time":now/1000,"date":"2026-10-15","model_name":"seedance","credits_float":amount,"core_request_id":request,"core_key_id":e.core_key_id,"ambiguous":false,"core_attribution_ambiguous":false});
            let file=dir.join("data/usage_history.json");std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            crate::fs_utils::write_json(&file,&serde_json::json!({"accounts":{"account":{"name":"fixture","daily":{},"session_usage":{e.session_ref.clone():row.clone()},"session_observations":{e.session_ref.clone():{"row":row,"complete":true,"read_started_at_ms":read,"completed_at_ms":read+1,"query_end_ms":now+300_000}}}}})).unwrap();
        };
        write(&e.request_id,now-121_000,"12.3456");assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Pending,"pre-exit cache is insufficient");
        write("other-request",now-1000,"12.3456");assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Pending);
        write(&e.request_id,now-1000,"0");assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Pending,"unknown work cannot be zero-settled from a provisional row");
        write(&e.request_id,now-1000,"12.3456");assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Created);
        assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Duplicate);
        assert_eq!(store.budget_execution(id).unwrap().unwrap().state,ExecutionState::Unknown);
        assert!(store.budget_execution(id).unwrap().unwrap().finished_at_ms.is_none());
        assert!(store.load_budget_result(id).unwrap().is_none());
        assert_eq!(store.capacity_totals("account").unwrap().pending,0);
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,12_345_600);
        assert_eq!(store.latest_budget_receipt_event(id).unwrap().unwrap().confirmation_policy,"post-worker-exit-session-observation-v1");
        let before=store.budget_worker_exit_proof(id,now).unwrap();drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.budget_worker_exit_proof(id,now).unwrap(),before);
        assert_eq!(store.confirm_budget_usage(&lease,id).unwrap(),ReceiptChange::Duplicate);
        cleanup(dir,store,lease);
    }
}
