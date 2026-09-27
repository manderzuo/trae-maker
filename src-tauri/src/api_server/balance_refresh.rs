//! Read-only display snapshots; never settles or rebases a financial ledger.
use std::sync::{Arc,atomic::{AtomicBool,Ordering}};
use super::pool::ApiPool;

fn refresh_one<F>(pool:&ApiPool,uid:&str,read:F)->Result<(),String>
where F:FnOnce(&super::pool::PickedAccount)->Result<Vec<serde_json::Value>,String> {
    let Some(account)=pool.completed_resource_credentials(uid) else {return Ok(())};
    let observed=chrono::Utc::now().timestamp_millis();
    let packs=read(&account)?;
    let (general,work)=super::bridge_capacity_source::PackObservation::parse(&packs,observed)?.balances()?;
    pool.observe_verified_capacity(uid,general,work,observed)
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
    fn disabled_account_does_not_trigger_upstream_request() {
        let pool=pool();pool.note_error("a",super::super::ErrKind::Forbidden);
        refresh_one(&pool,"a",|_|panic!("disabled account must not be read")).unwrap();
    }
}

pub(super) fn start(pool:ApiPool,stop:Arc<AtomicBool>) {
    tokio::spawn(async move {
        while !stop.load(Ordering::Acquire) {
            let ids:Vec<_>=pool.status_list().into_iter().filter(|s|!s.disabled).map(|s|s.uid).collect();
            for batch in ids.chunks(4) {
                if stop.load(Ordering::Acquire) {break;}
                let mut jobs=Vec::new();
                for uid in batch {
                    let pool=pool.clone();let uid=uid.clone();let stop=stop.clone();
                    jobs.push(tokio::task::spawn_blocking(move || {
                        if stop.load(Ordering::Acquire) {return;}
                        // No pool/config/SQLite lock crosses the network call.
                        let _=refresh_one(&pool,&uid,|account|crate::commands::accounts::query_ent_packs_for_bridge(&account.jwt,
                            &crate::models::DeviceEntry {device_id:account.device_id.clone(),..Default::default()}));
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
