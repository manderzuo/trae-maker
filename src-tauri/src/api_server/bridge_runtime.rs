//! Server-owned activity lease; workers must retain an Arc until all I/O ends.
use std::{path::{Path,PathBuf},sync::{Arc,Mutex}};
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease};

pub(super) struct BridgeBudgetRuntime { data_dir:PathBuf, lease:Mutex<BridgeBudgetLease>, planning:Mutex<std::collections::HashSet<String>>, receipt_cursors:Mutex<std::collections::HashMap<String,String>> }
pub(super) struct PlanningGuard<'a> {runtime:&'a BridgeBudgetRuntime,request:String}
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
        let candidates = self.with_store(|store,_| {
            let mut stmt=store.connection.prepare("SELECT p.budget_id,p.state FROM bridge_prepared_budgets p
                LEFT JOIN bridge_budget_executions e ON e.budget_id=p.budget_id
                WHERE p.account_ref=?1 AND p.budget_id>?2
                  AND (e.finished_at_ms IS NOT NULL OR p.state IN ('canceled','no_send'))
                ORDER BY p.budget_id LIMIT 256").map_err(|e|e.to_string())?;
            let rows=stmt.query_map([account,after.as_str()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(|e|e.to_string())?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map_err(|e|e.to_string())
        })?;
        // Revisit settled facts on later source observations as well. Bound work
        // and rotate by immutable ID so neither old pending nor settled rows can
        // permanently starve later requests. This cursor schedules reads only;
        // financial facts and the event outbox are persisted transactionally.
        let next=if candidates.len()==256 {candidates.last().unwrap().0.clone()} else {String::new()};
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
