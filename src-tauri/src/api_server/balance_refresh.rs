//! Read-only native balance reads plus fenced growth reconciliation for opted-in
//! dedicated accounts. No financial receipts or charged requests are created.
use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
use super::pool::ApiPool;

#[cfg(test)]
fn refresh_one<F>(pool:&ApiPool,uid:&str,read:F)->Result<(),String>
where F:FnMut(&super::pool::PickedAccount)->Result<Vec<serde_json::Value>,String> {
    refresh_one_with_runtime(pool,uid,None,read)
}
fn refresh_one_with_runtime<F>(pool:&ApiPool,uid:&str,runtime:Option<&super::bridge_runtime::BridgeBudgetRuntime>,mut read:F)->Result<(),String>
where F:FnMut(&super::pool::PickedAccount)->Result<Vec<serde_json::Value>,String> {
    let Some(account)=pool.completed_resource_credentials(uid) else {return Ok(())};
    let observed=chrono::Utc::now().timestamp_millis();
    let packs=read(&account)?;
    // Financial reconciliation uses the unmodified source response and its
    // explicit UNUSED contract, independently of dashboard interpretation.
    let native=if let Some(runtime)=runtime {
        if super::bridge_capacity_source::dedicated_enabled(runtime.data_dir(),uid)? {
            Some(super::bridge_capacity_source::PackObservation::parse(&packs,observed)?.balances()?)
        } else {None}
    } else {None};
    let mut display=packs;
    // The native dashboard counts an explicit empty usage object as an unused
    // reward pack. Apply that display contract to this local copy only. The
    // admission/rebase parser keeps its stricter independent evidence rules;
    // no request receipt or financial row is synthesized here.
    for pack in &mut display {
        if pack.pointer("/entitlement_base_info/quota/credits_limit").is_some()
            && pack.get("usage").and_then(serde_json::Value::as_object).is_some_and(|m|m.is_empty()) {
            pack["usage"]=serde_json::json!({"credits_amount":"0"});
        }
    }
    let (general,work)=super::bridge_capacity_source::PackObservation::parse(&display,observed)?.balances()?;
    pool.observe_verified_capacity(uid,general,work,observed)?;
    if let (Some(runtime),Some((general,work)))=(runtime,native) {
        // The existing rebase enforces quietness, original debit proof, lease
        // identity and atomic anchor/snapshot updates. Its second source read
        // occurs after fencing, without holding a runtime or SQLite lock.
        if super::bridge_capacity_source::reconcile_increased_capacity(runtime,uid,general,work,||read(&account))? {
            crate::fs_utils::app_log(runtime.data_dir(),"Core 专用账号新增容量已通过权益包核对；费用回执未改写");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use serde_json::json;
    fn pool()->ApiPool {
        let pool=ApiPool::new();
        let account=crate::models::RawAccount {name:"fixture".into(),user_id:Some("a".into()),jwt:"fixture-not-real".into(),refresh_token:None,dc_id:None,added_at:None,updated_at:None,credential_source:None};
        pool.sync_from_accounts_with_balances(&[account],&["a".into()],&[],&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new());pool
    }
    #[test]
    fn successful_read_updates_both_classes_but_failure_does_not_refresh_age() {
        let pool=pool();
        refresh_one(&pool,"a",|_|Ok(vec![
            json!({"entitlement_base_info":{"entitlement_id":"g","product_id":208,"quota":{"credits_limit":"10"}},"usage":{"credits_amount":"1.234567"}}),
            json!({"entitlement_base_info":{"entitlement_id":"w","product_id":209,"quota":{"credits_limit":"5"}},"usage":{"credits_amount":"2"}})
        ])).unwrap();
        let before=pool.bridge_credit_snapshots();
        assert_eq!(before[0].1,Some(8.765433));assert_eq!(before[0].2,Some(3.0));assert!(before[0].3.is_some());
        assert!(refresh_one(&pool,"a",|_|Err("network failure".into())).is_err());
        assert_eq!(pool.bridge_credit_snapshots(),before);
        assert!(refresh_one(&pool,"a",|_|Ok(vec![json!({"entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"10"}},"usage":null})])).is_err());
        assert_eq!(pool.bridge_credit_snapshots(),before);
    }
    #[test]
    fn sparse_unused_reward_is_visible_without_becoming_settlement_evidence() {
        let pool=pool();
        let packs=vec![json!({"entitlement_base_info":{"entitlement_id":"unused-reward","product_id":208,"quota":{"credits_limit":"150"}},"usage":{}})];
        refresh_one(&pool,"a",|_|Ok(packs.clone())).unwrap();
        assert_eq!(pool.bridge_credit_snapshots()[0].1,Some(150.0),"dashboard must match native available-credit display");
        assert_eq!(super::super::bridge_capacity_source::PackObservation::parse(&packs,1).unwrap().balances().unwrap(),(0,0),"display interpretation must not change debit evidence");
    }
    #[test]
    fn disabled_account_does_not_trigger_upstream_request() {
        let pool=pool();pool.note_error("a",super::super::ErrKind::Forbidden);
        refresh_one(&pool,"a",|_|panic!("disabled account must not be read")).unwrap();
    }
    #[test]
    fn qa61_idle_unused_rewards_refresh_capacity_without_a_receipt_or_charged_request() {
        use super::super::{bridge_prepared::tests::fixture,bridge_capacity_source::PackObservation,bridge_runtime::BridgeBudgetRuntime};
        let (dir,mut store,mut lease)=fixture();
        let old=json!({"id":"old-month","entitlement_base_info":{"product_id":221,"quota":{"credits_limit":"500"},"start_time":0},"usage":{"credits_amount":"500"},"expire_time":1});
        let work=json!({"id":"work","entitlement_base_info":{"product_id":209,"quota":{"credits_limit":"2000"},"start_time":0},"usage":{"credits_amount":"1989.2356"}});
        store.connection.execute("UPDATE bridge_capacity_accounts SET general_microcredits=0,work_microcredits=10764400",[]).unwrap();
        store.initialize_capacity_anchor(&lease,"account",&PackObservation::parse(&[old,work.clone()],1).unwrap()).unwrap();
        std::fs::write(dir.join("bridge-dedicated-accounts.json"),serde_json::to_vec(&json!({"version":1,"accounts":["account"]})).unwrap()).unwrap();
        store.finish_activity_lease_cleanly(&mut lease,||Ok(())).unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        let pool=ApiPool::new();let account=crate::models::RawAccount {name:"fixture".into(),user_id:Some("account".into()),jwt:"fixture-not-real".into(),refresh_token:None,dc_id:None,added_at:None,updated_at:None,credential_source:None};
        pool.sync_from_accounts_with_balances(&[account],&["account".into()],&[],&std::collections::HashMap::new(),&std::collections::HashMap::new(),&std::collections::HashMap::new(),&std::collections::HashMap::new(),&std::collections::HashMap::new(),&std::collections::HashMap::new(),&std::collections::HashMap::new());
        let reward=json!({"id":"reward","status":1,"entitlement_base_info":{"product_id":208,"ent_status":0,"quota":{"credits_limit":"150"},"start_time":1},"usage":{},"expire_time":4102444800_i64});
        let mut reads=0;
        refresh_one_with_runtime(&pool,"account",Some(&runtime),|_|{reads+=1;Ok(vec![work.clone(),reward.clone()])}).unwrap();
        runtime.with_store(|s,_| {
            let facts:(i64,i64,i64)=s.connection.query_row("SELECT general_microcredits,work_microcredits,snapshot_epoch FROM bridge_capacity_accounts WHERE account_ref='account'",[],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(facts,(150_000_000,10_764_400,2),"idle reward growth must reach the fenced verified snapshot");
            assert_eq!(s.capacity_totals("account")?.awaiting_receipt,0);
            let receipts:i64=s.connection.query_row("SELECT COUNT(*) FROM bridge_budget_receipts",[],|r|r.get(0)).unwrap();assert_eq!(receipts,0,"capacity refresh is not a financial receipt");Ok(())
        }).unwrap();
        assert_eq!(reads,2,"source must be read again after the quiescent fence");
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn qa61_reward_refresh_cannot_release_or_hide_an_awaiting_receipt_budget() {
        use super::super::{bridge_prepared::tests::fixture,bridge_capacity_source::PackObservation,bridge_runtime::BridgeBudgetRuntime,bridge_receipts::tests::send};
        let (dir,mut store,mut lease)=fixture();
        let g=serde_json::json!({"id":"g","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"},"start_time":0},"usage":{"credits_amount":"0"}});
        let w=serde_json::json!({"id":"w","entitlement_base_info":{"product_id":209,"quota":{"credits_limit":"100"},"start_time":0},"usage":{"credits_amount":"0"}});
        store.initialize_capacity_anchor(&lease,"account",&PackObservation::parse(&[g.clone(),w.clone()],1).unwrap()).unwrap();
        let budget=send(&mut store,&lease,"request-unknown-cost");
        std::fs::write(dir.join("bridge-dedicated-accounts.json"),serde_json::to_vec(&json!({"version":1,"accounts":["account"]})).unwrap()).unwrap();
        store.finish_activity_lease_cleanly(&mut lease,||Ok(())).unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let pool=ApiPool::new();
        let account=crate::models::RawAccount {name:"fixture".into(),user_id:Some("account".into()),jwt:"fixture-not-real".into(),refresh_token:None,dc_id:None,added_at:None,updated_at:None,credential_source:None};
        pool.sync_from_accounts_with_balances(&[account],&["account".into()],&[],&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new(),&HashMap::new());
        let reward=json!({"id":"reward","status":1,"entitlement_base_info":{"product_id":208,"ent_status":0,"quota":{"credits_limit":"150"},"start_time":1},"usage":{},"expire_time":4102444800_i64});let mut reads=0;
        assert_eq!(refresh_one_with_runtime(&pool,"account",Some(&runtime),|_|{reads+=1;Ok(vec![g.clone(),w.clone(),reward.clone()])}).unwrap_err(),"capacity_rebase_not_quiescent");
        assert_eq!(reads,1,"an unresolved hold must prevent the second reconciliation source read");
        runtime.with_store(|s,_| {
            assert_eq!(s.capacity_totals("account")?.awaiting_receipt,40_000_000);
            let slot:(String,i64,Option<i64>)=s.connection.query_row("SELECT stage,amount_microcredits,actual_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[&budget],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(slot,("R".into(),40_000_000,None));
            let epoch:i64=s.connection.query_row("SELECT snapshot_epoch FROM bridge_capacity_accounts WHERE account_ref='account'",[],|r|r.get(0)).unwrap();assert_eq!(epoch,1);
            let receipts:i64=s.connection.query_row("SELECT COUNT(*) FROM bridge_budget_receipts",[],|r|r.get(0)).unwrap();assert_eq!(receipts,0);Ok(())
        }).unwrap();drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
}

pub(super) fn start(pool:ApiPool,stop:Arc<AtomicBool>,runtime:Arc<super::bridge_runtime::BridgeBudgetRuntime>) {
    tokio::spawn(async move {
        while !stop.load(Ordering::Acquire) {
            let ids:Vec<_>=pool.status_list().into_iter().filter(|s|!s.disabled).map(|s|s.uid).collect();
            for batch in ids.chunks(4) {
                if stop.load(Ordering::Acquire) {break;}
                let mut jobs=Vec::new();
                for uid in batch {
                    let pool=pool.clone();let uid=uid.clone();let stop=stop.clone();let runtime=runtime.clone();
                    jobs.push(tokio::task::spawn_blocking(move || {
                        if stop.load(Ordering::Acquire) {return;}
                        // No pool/config/SQLite lock crosses the network call.
                        let result=refresh_one_with_runtime(&pool,&uid,Some(&runtime),|account| {
                            if stop.load(Ordering::Acquire) {return Err("capacity_refresh_stopped".into());}
                            crate::commands::accounts::query_ent_packs_for_bridge(&account.jwt,
                                &crate::models::DeviceEntry {device_id:account.device_id.clone(),..Default::default()})
                                .map_err(|_|"upstream_capacity_unavailable".into())
                        });
                        if let Err(error)=result {
                            if !stop.load(Ordering::Acquire) {
                                crate::fs_utils::app_log(runtime.data_dir(),&format!("Core 专用账号容量刷新未更新准入：{}；保留费用事实",super::bridge_capacity_source::safe_error_code(&error)));
                            }
                        }
                    }));
                }
                for job in jobs {let _=job.await;}
            }
            for _ in 0..60 {
                if stop.load(Ordering::Acquire) {return;}
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    });
}
