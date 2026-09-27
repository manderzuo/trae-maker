//! Server-owned activity lease; workers must retain an Arc until all I/O ends.
use std::{path::{Path,PathBuf},sync::{Arc,Mutex}};
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease};

pub(super) struct BridgeBudgetRuntime { data_dir:PathBuf, lease:Mutex<BridgeBudgetLease>, planning:Mutex<std::collections::HashSet<String>>, receipt_cursors:Mutex<std::collections::HashMap<String,(String,String)>> }
pub(super) struct PlanningGuard<'a> {runtime:&'a BridgeBudgetRuntime,request:String}
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
    pub(super) fn start(data_dir:&Path)->Result<Arc<Self>,String> {
        let store=BridgeBillingStore::open(data_dir)?;
        let lease=BridgeBudgetLease::try_acquire(&store)?.ok_or("bridge activity already owned by another server")?;
        Ok(Arc::new(Self {data_dir:data_dir.into(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default())}))
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
    pub(super) fn start_maintenance(runtime:&Arc<Self>,stop:Arc<std::sync::atomic::AtomicBool>) {
        let weak=Arc::downgrade(runtime);
        tokio::spawn(async move {
            let mut tick=tokio::time::interval(std::time::Duration::from_secs(5));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if stop.load(std::sync::atomic::Ordering::Acquire) {break;}
                let Some(runtime)=weak.upgrade() else {break};
                match tokio::task::spawn_blocking(move ||runtime.sweep_expired_budgets(chrono::Utc::now().timestamp_millis())).await {
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
                      AND (e.finished_at_ms IS NOT NULL OR p.state IN ('canceled','no_send'))
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
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default())});
        assert_eq!(runtime.sweep_expired_budgets(100_000).unwrap(),1,"old-generation unknowns must not occupy the entire expiry page");
        assert_eq!(store.capacity_totals("account").unwrap().pending,128,"do not reinterpret old preparation as a refund proof");
        assert_eq!(store.latest_budget_receipt_event(&current.authorization.budget_id).unwrap().unwrap().kind,"failed_no_charge");
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
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default())});
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
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default())});
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
        let runtime=Arc::new(BridgeBudgetRuntime {data_dir:dir.clone(),lease:Mutex::new(lease),planning:Mutex::new(Default::default()),receipt_cursors:Mutex::new(Default::default())});
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
