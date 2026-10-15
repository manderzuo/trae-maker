//! Server-owned activity lease; workers must retain an Arc until all I/O ends.
use std::{path::{Path,PathBuf},sync::{Arc,Mutex}};
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease};

pub(super) struct BridgeBudgetRuntime { data_dir:PathBuf, lease:Mutex<BridgeBudgetLease>, planning:Mutex<std::collections::HashSet<String>>, receipt_cursors:Mutex<std::collections::HashMap<String,(String,String)>>,video_diagnostic_cursor:Mutex<String>,pub(super) chunks:super::bridge_chunks::Hub }
pub(super) struct PlanningGuard<'a> {runtime:&'a BridgeBudgetRuntime,request:String}
/// Explicit maintenance entry, never part of normal startup. It starts no HTTP
/// listener, accesses no credentials and preserves all terminal unknown costs.
pub(crate) fn recover_quiet_local_instance(data_dir:&Path,acknowledge_retained_unknowns:bool)->Result<serde_json::Value,String> {
    if !acknowledge_retained_unknowns {return Err("bridge_recovery_confirmation_required".into());}
    if !data_dir.is_absolute() || !data_dir.join("bridge-billing.sqlite3").is_file() {return Err("bridge_local_recovery_invalid_root".into());}
    let quiet=|db:&rusqlite::Connection|->Result<(),String> {
        let blocked:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_slots WHERE stage='P') OR EXISTS(SELECT 1 FROM bridge_budget_executions WHERE execution_state IN ('running','unknown') OR finished_at_ms IS NULL)",[],|r|r.get(0)).map_err(|_|"bridge_local_recovery_check_failed")?;
        if blocked {Err("bridge_local_recovery_not_quiet".into())} else {Ok(())}
    };
    {
        let db=rusqlite::Connection::open_with_flags(data_dir.join("bridge-billing.sqlite3"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(|_|"bridge_local_recovery_read_failed")?;
        db.busy_timeout(std::time::Duration::from_secs(5)).map_err(|_|"bridge_local_recovery_read_failed")?;
        db.execute_batch("PRAGMA query_only=ON").map_err(|_|"bridge_local_recovery_read_failed")?;
        let schema:i64=db.query_row("SELECT schema_version FROM bridge_schema_meta WHERE singleton=1",[],|r|r.get(0)).map_err(|_|"bridge_local_recovery_read_failed")?;
        if !matches!(schema,8|9|10|11) {return Err("bridge_local_recovery_schema_requires_review".into());}
        quiet(&db)?;
    }
    let runtime=BridgeBudgetRuntime::start(data_dir).map_err(|_|"bridge_local_recovery_start_failed")?;
    // Repeat the preflight after both exclusive OS leases have been acquired.
    runtime.with_store(|s,_|quiet(&s.connection)).map_err(|e|if e=="bridge_local_recovery_not_quiet" {e} else {"bridge_local_recovery_check_failed".into()})?;
    let before=runtime.recovery_status().map_err(|_|"bridge_local_recovery_check_failed")?;
    let retained=before.retained_unresolved_budgets;
    let recovered=before.recovery_required;
    if recovered {
        let after=runtime.recover_local_instance(&before.instance_id,&before.generation,true).map_err(|_|"bridge_local_recovery_failed")?;
        if after.recovery_required || !after.charge_ready || after.retained_unresolved_budgets!=retained {
            return Err("bridge_local_recovery_verification_failed".into());
        }
    }
    drop(runtime);
    let store=BridgeBillingStore::open(data_dir).map_err(|_|"bridge_local_recovery_close_failed")?;
    if super::bridge_billing::is_dirty_bridge_generation(&store.bridge_identity().map_err(|_|"bridge_local_recovery_close_failed")?.1) {return Err("bridge_local_recovery_close_failed".into());}
    Ok(serde_json::json!({"operation":"explicit_quiet_local_bridge_recovery","recovered":recovered,
        "retained_unresolved_budgets":retained,"charged_dispatches":0,"network_calls":0,"credentials_read":false,"lease_closed":true,
        "capacity_grants_enabled":super::bridge_capacity_grants::enabled_for_process()}))
}
#[derive(serde::Serialize)]
pub(super) struct RecoveryStatus {
    pub instance_id:String,pub generation:String,pub recovery_required:bool,pub charge_ready:bool,
    pub retained_unresolved_budgets:i64,pub fenced_accounts:i64,
}
fn recovery_status(store:&BridgeBillingStore,lease:&BridgeBudgetLease)->Result<RecoveryStatus,String> {
    let retained=store.connection.query_row("SELECT COUNT(*) FROM bridge_capacity_slots WHERE stage IN ('P','R')",[],|r|r.get(0)).map_err(|_|"recovery status unavailable")?;
    let fenced=store.connection.query_row("SELECT COUNT(*) FROM bridge_capacity_accounts WHERE rebase_state='fenced'",[],|r|r.get(0)).map_err(|_|"recovery status unavailable")?;
    Ok(RecoveryStatus {instance_id:lease.instance_id().into(),generation:lease.generation().into(),recovery_required:lease.recovery_required(),
        charge_ready:lease.charge_ready(),retained_unresolved_budgets:retained,fenced_accounts:fenced})
}
impl Drop for PlanningGuard<'_> {
    fn drop(&mut self) {if let Ok(mut busy)=self.runtime.planning.lock() {busy.remove(&self.request);}}
}
impl BridgeBudgetRuntime {
    pub(super) fn data_dir(&self)->&Path {&self.data_dir}
    pub(super) fn start(data_dir:&Path)->Result<Arc<Self>,String> {
        let store=BridgeBillingStore::open(data_dir)?;
        let lease=BridgeBudgetLease::try_acquire(&store)?.ok_or("bridge activity already owned by another server")?;
        Ok(Arc::new(Self {data_dir:data_dir.into(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default()),video_diagnostic_cursor:Mutex::new(String::new()),chunks:Default::default()}))
    }
    pub(super) fn begin_planning(&self,request:&str)->Result<PlanningGuard<'_>,String> {
        let mut busy=self.planning.lock().map_err(|_|"budget planner unavailable")?;
        if busy.len()>=4 || !busy.insert(request.to_owned()) {return Err("budget_preparation_busy".into());}
        Ok(PlanningGuard {runtime:self,request:request.into()})
    }
    pub(super) fn begin_close(&self) {
        self.lease.lock().unwrap_or_else(|e|e.into_inner()).begin_clean_close();
    }
    pub(super) fn recovery_status(&self)->Result<RecoveryStatus,String> {
        self.with_store(|store,lease|recovery_status(store,lease))
    }
    pub(super) fn recover_local_instance(&self,instance:&str,generation:&str,acknowledge_retained_unknowns:bool)->Result<RecoveryStatus,String> {
        if !acknowledge_retained_unknowns {return Err("bridge_recovery_confirmation_required".into());}
        let planning=self.planning.lock().map_err(|_|"bridge recovery planner unavailable")?;
        if !planning.is_empty() {return Err("bridge_recovery_busy".into());}
        let mut lease=self.lease.lock().map_err(|_|"bridge recovery lease unavailable")?;
        if lease.instance_id()!=instance || lease.generation()!=generation || !lease.recovery_required()
            || lease.charge_ready() || !lease.is_active() || lease.is_closed() || lease.is_closing() {
            return Err("bridge_recovery_identity_or_state_changed".into());
        }
        let mut store=BridgeBillingStore::open(&self.data_dir)?;
        // This runtime could only be constructed after acquiring BOTH OS locks.
        // Any prior runtime's paid worker holds its Arc/lease until I/O exits,
        // so it cannot coexist with this recovery-owned runtime on this host.
        // No worker of THIS runtime has charge permission in recovery state;
        // the planning guard above excludes concurrent admission. This is an
        // explicit local recovery operation, never automatic startup or a claim
        // that a restored backup's old Prepared records were definitely unsent.
        store.rotate_event_generation_for_recovery(&mut lease,||Ok(()))?;
        recovery_status(&store,&lease)
    }
    pub(super) fn sweep_expired_budgets(&self,now:i64)->Result<usize,String> {
        self.with_store(|s,l|s.expire_unconsumed_budgets(l,now))
    }
    pub(super) fn sweep_interrupted_text(&self,now:i64)->Result<usize,String> {
        let requests=self.with_store(|store,lease| {
            if !lease.charge_ready() || lease.is_closing() {return Ok(Vec::new());}
            // The exclusive host/instance lease outlives every local worker.
            // A different preparation generation therefore proves loss of its
            // LOCAL text execution, not upstream cancellation or free usage.
            // Videos remain unknown: their asynchronous task may still run.
            let ids={
                let mut stmt=store.connection.prepare("SELECT e.budget_id FROM bridge_budget_executions e
                    JOIN bridge_prepared_budgets p ON p.budget_id=e.budget_id
                    WHERE e.execution_state IN ('running','unknown') AND e.finished_at_ms IS NULL
                      AND e.step_kind IN ('assist','chat') AND e.bridge_instance_id=?1
                      AND p.event_generation<>?2 ORDER BY e.budget_id LIMIT 64").map_err(|e|e.to_string())?;
                let rows=stmt.query_map(rusqlite::params![lease.instance_id(),lease.generation()],|r|r.get::<_,String>(0)).map_err(|e|e.to_string())?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?
            };
            let mut ended=Vec::new();let mut failed=false;
            for id in ids {
                let result=serde_json::json!({"status":"failed","error":{"code":"bridge_execution_interrupted",
                    "message":"原网关进程已退出，文字步骤的执行结果丢失；不会自动重发。费用继续按原会话真实账单核对。"}});
                // Uses the same immutable encrypted result and P->R transaction
                // as the executor. No receipts/amounts are manufactured here.
                match store.finish_budget_result(lease,&id,super::bridge_execution::ExecutionState::Failed,&result,now) {
                    Ok(())=>ended.push(store.budget_execution(&id)?.ok_or("recovered execution missing")?.request_id),Err(_)=>failed=true,
                }
            }
            if failed {Err("interrupted text recovery retained invalid evidence for review".into())} else {Ok(ended)}
        })?;
        // Wake the existing bounded usage queue after releasing the lease/DB
        // lock. No network here; startup discovery remains the durable fallback.
        for request in &requests {let _=super::usage_refresh::request_refresh_for_dir(&self.data_dir,request);}
        Ok(requests.len())
    }
    pub(super) fn diagnose_interrupted_videos(&self)->Result<usize,String> {
        let mut cursor=self.video_diagnostic_cursor.lock().map_err(|_|"video diagnostic scan unavailable")?;
        let tasks=self.with_store(|store,lease| {
            if !lease.charge_ready() || lease.is_closing() { return Ok(Vec::new()); }
            let rows={
                let mut query=store.connection.prepare("SELECT e.budget_id,e.task_ref FROM bridge_budget_executions e
                    JOIN bridge_prepared_budgets p ON p.budget_id=e.budget_id
                    WHERE e.step_kind='video' AND p.state='send_intent' AND e.execution_state IN ('running','unknown') AND e.finished_at_ms IS NULL
                      AND e.bridge_instance_id=?1 AND p.event_generation<>?2 AND e.budget_id>?3
                    ORDER BY e.budget_id LIMIT 64").map_err(|e|e.to_string())?;
                let rows=query.query_map(rusqlite::params![lease.instance_id(),lease.generation(),cursor.as_str()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?))).map_err(|e|e.to_string())?;
                rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?
            };
            let next=if rows.len()==64 {rows.last().unwrap().0.clone()} else {String::new()};
            let mut tasks=Vec::new();
            for (id,task) in rows {
                // Losing the local worker does not establish upstream failure,
                // cancellation, success or cost. Preserve the paid hold intact.
                let existing=store.budget_diagnostic(&id)?;
                let fresh=existing.is_none();
                let diagnostic=existing.unwrap_or_else(||super::bridge_diagnostics::Diagnostic {code:"video_execution_interrupted".into(),stage:"video_stream".into(),outcome_known:false,upstream_error:None,transport:None});
                if fresh {store.save_budget_diagnostic(lease,&id,&diagnostic)?;}
                // The exclusive process lease fences every worker in the old
                // generation. Record local exit without inventing a video result.
                store.record_budget_worker_exit(lease,&id,chrono::Utc::now().timestamp_millis())?;
                if let Some(task)=task {
                    let stale=super::video::get(&task).is_some_and(|t| !matches!(t.status.as_str(),"completed"|"failed"|"canceled"|"timeout") && (t.status!="unknown" || t.execution_diagnostic.as_ref()!=Some(&diagnostic)));
                    if fresh || stale {tasks.push((task,diagnostic));}
                }
            }
            *cursor=next;
            Ok(tasks)
        })?;
        for (task,diagnostic) in &tasks {super::video::mark_native_worker_diagnostic(task,diagnostic.clone());}
        Ok(tasks.len())
    }
    pub(super) fn start_maintenance(runtime:&Arc<Self>,stop:Arc<std::sync::atomic::AtomicBool>) {
        let weak=Arc::downgrade(runtime);
        tokio::spawn(async move {
            let mut tick=tokio::time::interval(std::time::Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if stop.load(std::sync::atomic::Ordering::Acquire) {break;}
                let Some(runtime)=weak.upgrade() else {break};
                match tokio::task::spawn_blocking(move || {
                    let now=chrono::Utc::now().timestamp_millis();
                    runtime.sweep_interrupted_text(now)?;
                    runtime.diagnose_interrupted_videos()?;
                    runtime.sweep_expired_budgets(now)
                }).await {
                    Ok(Ok(_))=>{},_=>eprintln!("[bridge-v2] expiry recovery requires attention; no unknown sends were released"),
                }
            }
        });
    }
    pub(super) fn with_store<T>(&self, action:impl FnOnce(&mut BridgeBillingStore,&BridgeBudgetLease)->Result<T,String>)->Result<T,String> {
        let lease=self.lease.lock().map_err(|_|"bridge lease lock poisoned")?;
        let mut store=BridgeBillingStore::open(&self.data_dir)?;
        action(&mut store,&lease)
    }
    pub(super) fn confirm_account(&self,account:&str)->Result<(),String> {
        let after=self.receipt_cursors.lock().map_err(|_|"receipt cursor unavailable")?.get(account).cloned().unwrap_or_default();
        let (candidates,next) = self.with_store(|store,_| {
            let mut candidates=Vec::new();
            let mut next=(String::new(),String::new());
            // Give pending receipts their own lane. A large settled history must
            // never consume the whole page ahead of newly available money facts.
            // Keep a smaller independent history lane to detect later conflicts.
            for (history,cursor,limit) in [(0,after.0.as_str(),192),(1,after.1.as_str(),64)] {
                let mut stmt=store.connection.prepare("SELECT p.budget_id,p.state FROM bridge_prepared_budgets p
                    LEFT JOIN bridge_budget_executions e ON e.budget_id=p.budget_id
                    LEFT JOIN bridge_budget_receipts r ON r.budget_id=p.budget_id
                    WHERE p.account_ref=?1 AND p.budget_id>?2
                      AND (e.finished_at_ms IS NOT NULL OR p.state IN ('canceled','no_send')
                        OR (e.execution_state='unknown' AND EXISTS(SELECT 1 FROM bridge_budget_worker_exits w WHERE w.budget_id=e.budget_id)))
                      AND ((?3=0 AND r.budget_id IS NULL) OR (?3=1 AND r.receipt_state='final'))
                    ORDER BY p.budget_id LIMIT ?4").map_err(|e|e.to_string())?;
                let rows=stmt.query_map(rusqlite::params![account,cursor,history,limit],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(|e|e.to_string())?;
                let page=rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())?;
                if page.len()==limit as usize {
                    let value=page.last().unwrap().0.clone();
                    if history==0 {next.0=value;} else {next.1=value;}
                }
                candidates.extend(page);
            }
            Ok((candidates,next))
        })?;
        // Exited unknowns have no terminal timestamp. Include them in both lanes;
        // confirm_budget_usage independently checks age, quiet time and exact bills.
        // Cursors schedule reads only; money facts/outbox remain transactional.
        let mut failed=false;
        for (id,state) in candidates {
            let result=self.with_store(|store,lease| if matches!(state.as_str(),"canceled"|"no_send") {
                store.confirm_budget_no_send(lease,&id)
            } else {store.confirm_budget_usage(lease,&id)});
            if result.is_err() {failed=true;}
        }
        self.receipt_cursors.lock().map_err(|_|"receipt cursor unavailable")?.insert(account.to_owned(),next);
        if failed {Err("one or more budget receipts could not be confirmed; retained for retry".into())} else {Ok(())}
    }
}
impl Drop for BridgeBudgetRuntime {
    fn drop(&mut self) {
        // The last Arc is gone: no execution/refresh worker can still use this lease.
        // On recovery/poison/disk failure retain the dirty marker; never claim a clean stop.
        if let Ok(lease)=self.lease.get_mut() {
            if let Ok(store)=BridgeBillingStore::open(&self.data_dir) {
                let _=store.finish_activity_lease_cleanly(lease,||Ok(()));
            }
        }
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_prepared::tests::fixture,bridge_receipts::tests::{send,cache}};
    #[test]
    fn account_refresh_confirms_stopped_unknowns_and_retains_unproven_or_recent_work() {
        use super::super::{bridge_prepared::{tests::input,ConsumeOutcome},bridge_execution::ExecutionState};
        let (dir,store,lease)=fixture();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();
        if status.recovery_required {runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();}
        let now=chrono::Utc::now().timestamp_millis();
        let ids=runtime.with_store(|s,l| {
            let mut ids=Vec::new();let mut rows=serde_json::Map::new();let mut observations=serde_json::Map::new();
            for (index,(unknown,age,exit))in [(true,1_900_000,Some(120_000)),(true,1_900_000,None),(true,1_900_000,Some(30_000)),(true,120_000,Some(90_000)),(false,1_900_000,None)].into_iter().enumerate() {
                let mut value=input();value.step_kind="video".into();value.request_id=format!("account-unknown-{index}");value.hold_microcredits=1_000_000;
                let p=s.prepare_budget(l,&value,None,10)?;let ConsumeOutcome::Granted(c)=s.consume_budget(l,&p,20)?else{panic!("consume")};
                let id=p.authorization.budget_id;s.mark_budget_send_intent(l,&id,&c.consume_epoch)?;
                s.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",rusqlite::params![now-age,&id]).unwrap();
                if unknown {s.mark_budget_execution_unknown(l,&id)?;}
                if let Some(elapsed)=exit {s.record_budget_worker_exit(l,&id,now-elapsed)?;}
                let e=s.budget_execution(&id)?.unwrap();
                let row=serde_json::json!({"session_id":e.session_ref,"usage_time":now/1000,"date":"2026-10-15","model_name":"seedance","credits_float":"0.5","core_request_id":e.request_id,"core_key_id":e.core_key_id,"ambiguous":false,"core_attribution_ambiguous":false});
                rows.insert(e.session_ref.clone(),row.clone());observations.insert(e.session_ref,serde_json::json!({"row":row,"complete":true,"read_started_at_ms":now-1000,"completed_at_ms":now,"query_end_ms":now+300_000}));ids.push(id);
            }
            let path=dir.join("data/usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            crate::fs_utils::write_json(&path,&serde_json::json!({"accounts":{"account":{"name":"fixture","daily":{},"session_usage":rows,"session_observations":observations}}})).unwrap();Ok(ids)
        }).unwrap();
        runtime.confirm_account("account").unwrap();runtime.confirm_account("account").unwrap();
        runtime.with_store(|s,l| {
            let event=s.latest_budget_receipt_event(&ids[0])?.expect("background account refresh must include an eligible stopped unknown");
            assert_eq!(event.receipt.unwrap().actual_credits.unwrap().as_microcredits(),500_000);
            assert_eq!(event.confirmation_policy,"post-worker-exit-session-observation-v1");
            assert_eq!(s.budget_receipt_events(l.generation(),0,20)?.len(),1,"repeat background scans cannot double charge");
            for (index,id)in ids.iter().enumerate() {
                let e=s.budget_execution(id)?.unwrap();assert!(e.finished_at_ms.is_none());assert!(s.load_budget_result(id)?.is_none());
                assert_eq!(e.state,if index==4{ExecutionState::Running}else{ExecutionState::Unknown});
                if index>0 {assert!(s.latest_budget_receipt_event(id)?.is_none(),"unproven, recent and running work must retain its hold");}
            }Ok(())
        }).unwrap();drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn prior_host_text_execution_ends_without_zero_bill_and_late_receipt_settles_once() {
        use super::super::{bridge_prepared::{tests::input,ConsumeOutcome},bridge_execution::ExecutionState};
        let (dir,mut store,lease)=fixture();let mut value=input();
        value.step_kind="assist".into();value.model="glm-5.3-flash".into();
        value.request_id="old-helper".into();
        let helper=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&helper,20).unwrap() else {panic!("consume")};
        store.mark_budget_send_intent(&lease,&helper.authorization.budget_id,&ctx.consume_epoch).unwrap();
        value.step_kind="video".into();value.request_id="old-video".into();
        let video=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&video,20).unwrap() else {panic!("consume")};
        store.mark_budget_send_intent(&lease,&video.authorization.budget_id,&ctx.consume_epoch).unwrap();
        drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();
        runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();
        assert_eq!(runtime.sweep_interrupted_text(chrono::Utc::now().timestamp_millis()).unwrap(),1);
        assert_eq!(runtime.sweep_interrupted_text(chrono::Utc::now().timestamp_millis()).unwrap(),0);
        runtime.with_store(|s,l| {
            let h=&helper.authorization.budget_id;
            assert_eq!(s.budget_execution(h)?.unwrap().state,ExecutionState::Failed);
            assert_eq!(s.load_budget_result(h)?.unwrap()["error"]["code"],"bridge_execution_interrupted");
            assert_eq!(s.budget_execution(&video.authorization.budget_id)?.unwrap().state,ExecutionState::Unknown);
            assert!(s.latest_budget_receipt_event(h)?.is_none(),"host loss is not a zero-cost receipt");
            assert_eq!(s.capacity_totals("account")?.awaiting_receipt,40_000_000);
            cache(&dir,s,&[(h,"0.0968")],2000);
            s.confirm_budget_usage(l,h)?;s.confirm_budget_usage(l,h)?;
            assert_eq!(s.latest_budget_receipt_event(h)?.unwrap().receipt.unwrap().actual_credits.unwrap().as_microcredits(),96_800);
            assert_eq!(s.budget_receipt_events(l.generation(),0,10)?.len(),1);
            assert_eq!(s.capacity_totals("account")?.awaiting_receipt,0);
            value.step_kind="chat".into();value.request_id="current-chat".into();
            let current=s.prepare_budget(l,&value,None,10)?;
            let ConsumeOutcome::Granted(ctx)=s.consume_budget(l,&current,20)? else {panic!("consume")};
            s.mark_budget_send_intent(l,&current.authorization.budget_id,&ctx.consume_epoch)?;
            s.mark_budget_execution_unknown(l,&current.authorization.budget_id)?;Ok(())
        }).unwrap();
        assert_eq!(runtime.sweep_interrupted_text(chrono::Utc::now().timestamp_millis()).unwrap(),0,"current generation may still have a worker");
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn interrupted_video_diagnostic_retains_hold_identity_and_accepts_late_result() {
        use super::super::{bridge_prepared::{tests::input,ConsumeOutcome},bridge_execution::ExecutionState};
        let (dir,mut store,lease)=fixture();let mut value=input();value.step_kind="video".into();value.request_id="old-video-diagnostic".into();
        let prepared=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("consume")};
        let id=prepared.authorization.budget_id;
        store.mark_budget_send_intent(&lease,&id,&ctx.consume_epoch).unwrap();store.bind_budget_task(&lease,&id,"video-retained").unwrap();
        value.request_id="old-unsent-video".into();let unsent=store.prepare_budget(&lease,&value,None,10).unwrap();
        assert!(matches!(store.consume_budget(&lease,&unsent,20).unwrap(),ConsumeOutcome::Granted(_)));
        let before=store.capacity_totals("account").unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);
        runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),1);assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);
        runtime.with_store(|s,l| {
            assert_eq!(s.capacity_totals("account")?,before);assert!(s.latest_budget_receipt_event(&id)?.is_none());assert!(s.load_budget_result(&id)?.is_none());
            assert!(s.budget_diagnostic(&unsent.authorization.budget_id)?.is_none(),"unsent consumed budgets cannot block diagnostic recovery");
            let diagnostic=s.budget_diagnostic(&id)?.unwrap();assert_eq!(diagnostic.code,"video_execution_interrupted");assert!(!diagnostic.outcome_known);
            let execution=s.budget_execution(&id)?.unwrap();assert_eq!(execution.state,ExecutionState::Unknown);assert_eq!(execution.task_ref.as_deref(),Some("video-retained"));assert!(execution.finished_at_ms.is_none());
            assert!(s.mark_budget_send_intent(l,&id,&ctx.consume_epoch).is_err());
            let result=serde_json::json!({"id":"video-retained","status":"completed","resource_uri":"tos://late"});
            s.finish_budget_result(l,&id,ExecutionState::Succeeded,&result,chrono::Utc::now().timestamp_millis())?;assert_eq!(s.load_budget_result(&id)?,Some(result));
            value.request_id="current-video-diagnostic".into();let current=s.prepare_budget(l,&value,None,10)?;
            let ConsumeOutcome::Granted(ctx)=s.consume_budget(l,&current,20)? else {panic!("consume")};
            s.mark_budget_send_intent(l,&current.authorization.budget_id,&ctx.consume_epoch)?;s.bind_budget_task(l,&current.authorization.budget_id,"video-current")?;
            Ok(())
        }).unwrap();
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0,"current workers must not be interrupted by maintenance");
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn existing_video_diagnostic_repairs_stale_task_dto_after_restart() {
        use super::super::bridge_prepared::{tests::input,ConsumeOutcome};
        let (dir,mut store,lease)=fixture();let mut value=input();value.step_kind="video".into();value.request_id="stale-dto-video".into();
        let prepared=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("consume")};
        let id=prepared.authorization.budget_id;store.mark_budget_send_intent(&lease,&id,&ctx.consume_epoch).unwrap();
        let task=super::super::video::create_pending("seedance".into(),"fixture".into());store.bind_budget_task(&lease,&id,&task.id).unwrap();
        let diagnostic=super::super::video_stream::Fault::Read.diagnostic();store.save_budget_diagnostic(&lease,&id,&diagnostic).unwrap();
        let before=store.capacity_totals("account").unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),1);assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);
        let task=super::super::video::get(&task.id).unwrap();assert_eq!(task.status,"unknown");assert_eq!(task.execution_diagnostic,Some(diagnostic.clone()));
        runtime.with_store(|s,_| {assert_eq!(s.capacity_totals("account")?,before);assert_eq!(s.budget_diagnostic(&id)?,Some(diagnostic));assert!(s.latest_budget_receipt_event(&id)?.is_none());Ok(())}).unwrap();
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn video_diagnostic_scan_advances_past_sixty_four_existing_records() {
        use super::super::bridge_prepared::{tests::input,ConsumeOutcome};
        let (dir,mut store,lease)=fixture();let diagnostic=super::super::video_stream::Fault::Read.diagnostic();let mut ids=Vec::new();
        for index in 0..65 {
            let mut value=input();value.step_kind="video".into();value.request_id=format!("paged-video-{index}");value.hold_microcredits=1_000_000;
            let prepared=store.prepare_budget(&lease,&value,None,10).unwrap();
            let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("consume")};
            let id=prepared.authorization.budget_id;store.mark_budget_send_intent(&lease,&id,&ctx.consume_epoch).unwrap();store.save_budget_diagnostic(&lease,&id,&diagnostic).unwrap();ids.push(id);
        }
        ids.sort();let task=super::super::video::create_pending("seedance".into(),"fixture".into());store.bind_budget_task(&lease,ids.last().unwrap(),&task.id).unwrap();
        let before=store.capacity_totals("account").unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);assert_eq!(super::super::video::get(&task.id).unwrap().status,"queued");
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),1);assert_eq!(super::super::video::get(&task.id).unwrap().status,"unknown");
        assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);assert_eq!(runtime.diagnose_interrupted_videos().unwrap(),0);
        runtime.with_store(|s,_| {assert_eq!(s.capacity_totals("account")?,before);assert_eq!(s.budget_diagnostic(ids.last().unwrap())?,Some(diagnostic));assert!(s.latest_budget_receipt_event(ids.last().unwrap())?.is_none());Ok(())}).unwrap();
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn explicit_runtime_recovery_retains_unknown_holds_and_fences_old_tokens() {
        use super::super::{bridge_prepared::{tests::input,ConsumeOutcome},bridge_budget::CapacitySnapshot};
        let (dir,mut store,lease)=fixture();
        let old=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let mut value=input();value.request_id="request-old-consumed".into();
        let consumed=store.prepare_budget(&lease,&value,None,10).unwrap();
        assert!(matches!(store.consume_budget(&lease,&consumed,20).unwrap(),ConsumeOutcome::Granted(_)));
        value.request_id="request-old-sent".into();
        let sent=store.prepare_budget(&lease,&value,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&sent,20).unwrap() else {panic!("consume")};
        store.mark_budget_send_intent(&lease,&sent.authorization.budget_id,&ctx.consume_epoch).unwrap();
        store.initialize_capacity(&lease,&CapacitySnapshot {account_ref:"idle-account".into(),snapshot_ref:"idle".into(),epoch:1,general:20,work:0,observed_at_ms:1}).unwrap();
        store.begin_capacity_rebase(&lease,"idle-account",30).unwrap();
        drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let status=runtime.recovery_status().unwrap();
        assert!(status.recovery_required);assert!(!status.charge_ready);
        assert!(runtime.recover_local_instance(&status.instance_id,"wrong-generation",true).is_err());
        assert!(runtime.recover_local_instance(&status.instance_id,&status.generation,false).is_err());
        let activated=runtime.recover_local_instance(&status.instance_id,&status.generation,true).unwrap();
        assert!(activated.charge_ready);assert!(!activated.recovery_required);assert_ne!(activated.generation,status.generation);
        assert_eq!(activated.retained_unresolved_budgets,3);
        runtime.with_store(|s,l| {
            assert_eq!(s.capacity_totals("account")?.pending,120_000_000);
            assert!(s.consume_budget(l,&old,40).is_err());
            assert!(!matches!(s.consume_budget(l,&consumed,40),Ok(ConsumeOutcome::Granted(_))));
            assert!(s.mark_budget_send_intent(l,&sent.authorization.budget_id,&ctx.consume_epoch).is_err());
            assert_eq!(s.budget_execution(&sent.authorization.budget_id)?.unwrap().state,super::super::bridge_execution::ExecutionState::Unknown);
            let state:String=s.connection.query_row("SELECT rebase_state FROM bridge_capacity_accounts WHERE account_ref='idle-account'",[],|r|r.get(0)).unwrap();assert_eq!(state,"open");
            value.request_id="request-new-after-recovery".into();s.prepare_budget(l,&value,None,40)?;Ok(())
        }).unwrap();
        assert!(runtime.recover_local_instance(&status.instance_id,&status.generation,true).is_err());
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn qa61_local_recovery_preserves_terminal_unknown_costs_and_closes_cleanly() {
        let (dir,mut store,lease)=fixture();
        let budget=super::super::bridge_receipts::tests::send(&mut store,&lease,"request-terminal-unbilled");
        let before=store.capacity_totals("account").unwrap();
        drop(lease);drop(store);
        let result=super::recover_quiet_local_instance(&dir,true).expect("explicit quiet recovery must use existing lease recovery");
        assert_eq!(result["retained_unresolved_budgets"],1);
        assert_eq!(result["charged_dispatches"],0);
        let store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap(),before);
        let facts:(String,i64,Option<i64>)=store.connection.query_row("SELECT stage,amount_microcredits,actual_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[budget],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
        assert_eq!(facts,("R".into(),40_000_000,None));
        let receipts:i64=store.connection.query_row("SELECT COUNT(*) FROM bridge_budget_receipts",[],|r|r.get(0)).unwrap();assert_eq!(receipts,0);
        assert!(!super::super::bridge_billing::is_dirty_bridge_generation(&store.bridge_identity().unwrap().1),"one-shot recovery must close its own lease before the GUI starts");
        drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn qa61_local_recovery_rejects_missing_acknowledgment_without_mutation() {
        let (dir,store,lease)=fixture();let identity=store.bridge_identity().unwrap();
        assert_eq!(super::recover_quiet_local_instance(&dir,false).unwrap_err(),"bridge_recovery_confirmation_required");
        assert_eq!(store.bridge_identity().unwrap(),identity);
        drop(lease);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn qa61_local_recovery_rejects_live_or_unsent_executions_without_lease_rotation() {
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&super::super::bridge_prepared::tests::input(),None,10).unwrap();
        let identity=store.bridge_identity().unwrap();
        assert_eq!(super::recover_quiet_local_instance(&dir,true).unwrap_err(),"bridge_local_recovery_not_quiet");
        assert_eq!(store.bridge_identity().unwrap(),identity);
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        let _=prepared;drop(lease);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn runtime_recovery_cannot_reenable_a_closing_server() {
        let (dir,store,lease)=fixture();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let before=runtime.recovery_status().unwrap();
        runtime.begin_close();
        assert!(runtime.recover_local_instance(&before.instance_id,&before.generation,true).is_err(),"shutdown must not be reversed by a late recovery request");
        assert!(!runtime.recovery_status().unwrap().charge_ready);
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn stale_generation_preparations_do_not_starve_current_expiry_queue() {
        use super::super::bridge_prepared::tests::input;
        let (dir,mut store,mut lease)=fixture();let mut value=input();value.hold_microcredits=1;
        for i in 0..128 {value.request_id=format!("request-old-{i}");store.prepare_budget(&lease,&value,None,10).unwrap();}
        store.finish_activity_lease_cleanly(&mut lease,||Ok(())).unwrap();drop(lease);
        let lease=BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        value.request_id="request-current-expired".into();let current=store.prepare_budget(&lease,&value,None,20).unwrap();
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default()),video_diagnostic_cursor:Mutex::new(String::new()),chunks:Default::default()});
        assert_eq!(runtime.sweep_expired_budgets(100_000).unwrap(),128,"current expiry is processed before bounded prior-generation recovery");
        assert_eq!(store.capacity_totals("account").unwrap().pending,1,"only fenced, never-consumed preparations may be released");
        assert_eq!(store.latest_budget_receipt_event(&current.authorization.budget_id).unwrap().unwrap().kind,"failed_no_charge");
        assert_eq!(runtime.sweep_expired_budgets(100_000).unwrap(),1);
        assert_eq!(store.capacity_totals("account").unwrap().pending,0);
        drop(runtime);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn new_receipt_is_not_queued_behind_a_full_page_of_settled_history() {
        use super::super::bridge_prepared::tests::input;
        let (dir,mut store,lease)=fixture();
        let mut ids=Vec::new();
        for i in 0..257 {
            let mut value=input();value.request_id=format!("request-priority-{i}");
            let budget=store.prepare_budget(&lease,&value,None,10).unwrap();
            store.cancel_budget(&lease,&budget,20).unwrap();
            ids.push(budget.authorization.budget_id);
        }
        ids.sort();
        for id in &ids[..256] {store.confirm_budget_no_send(&lease,id).unwrap();}
        let latest=&ids[256];
        assert!(store.latest_budget_receipt_event(latest).unwrap().is_none());
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default()),video_diagnostic_cursor:Mutex::new(String::new()),chunks:Default::default()});
        runtime.confirm_account("account").unwrap();
        assert!(store.latest_budget_receipt_event(latest).unwrap().is_some(),
            "newly available receipts must not wait for a full historical sweep");
        drop(runtime);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn expired_unused_preparation_releases_capacity_but_never_consumed_or_sent_budgets() {
        use super::super::bridge_prepared::{tests::input,ConsumeOutcome};
        let (dir,mut store,lease)=fixture();
        let first=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let mut other=input();other.request_id="request-consumed".into();
        let consumed=store.prepare_budget(&lease,&other,None,10).unwrap();
        assert!(matches!(store.consume_budget(&lease,&consumed,20).unwrap(),ConsumeOutcome::Granted(_)));
        other.request_id="request-sent".into();
        let sent=store.prepare_budget(&lease,&other,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(&lease,&sent,20).unwrap() else {panic!("consume")};
        store.mark_budget_send_intent(&lease,&sent.authorization.budget_id,&ctx.consume_epoch).unwrap();
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default()),video_diagnostic_cursor:Mutex::new(String::new()),chunks:Default::default()});
        assert_eq!(runtime.sweep_expired_budgets(100_000).unwrap(),1);
        assert_eq!(store.capacity_totals("account").unwrap().pending,80_000_000);
        assert_eq!(store.latest_budget_receipt_event(&first.authorization.budget_id).unwrap().unwrap().kind,"failed_no_charge");
        assert!(store.latest_budget_receipt_event(&consumed.authorization.budget_id).unwrap().is_none());
        assert!(store.latest_budget_receipt_event(&sent.authorization.budget_id).unwrap().is_none());
        assert_eq!(runtime.sweep_expired_budgets(200_000).unwrap(),0);
        drop(runtime);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn runtime_confirms_ready_account_receipts_and_retains_lease_until_last_worker() {
        let (dir,mut store,lease)=fixture();
        let id=send(&mut store,&lease,"request-runtime");
        cache(&dir,&store,&[(&id,"11.234567")],2000);
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default()),video_diagnostic_cursor:Mutex::new(String::new()),chunks:Default::default()});
        runtime.confirm_account("account").unwrap();
        assert_eq!(store.latest_budget_receipt_event(&id).unwrap().unwrap().receipt.unwrap().actual_credits.unwrap().to_string(),"11.234567");
        cache(&dir,&store,&[(&id,"12.234567")],3000);
        runtime.confirm_account("account").unwrap();
        assert_eq!(store.latest_budget_receipt_event(&id).unwrap().unwrap().kind,"conflict","later source reads must still check already-settled requests");
        let worker=runtime.clone();runtime.begin_close();drop(runtime);
        assert!(BridgeBudgetLease::try_acquire(&store).unwrap().is_none());
        worker.confirm_account("account").unwrap();drop(worker);
        let new=BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        assert!(new.charge_ready(),"last worker should perform clean lease close");
        drop(new);drop(store);std::fs::remove_dir_all(dir).unwrap();
    }
}
