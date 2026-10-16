//! Production recovery regression tests: isolated fake ledgers, no paid calls.
use super::{bridge_runtime::BridgeBudgetRuntime,bridge_prepared::{tests::{fixture,input},ConsumeOutcome},bridge_execution::ExecutionState};
use serde_json::json;

#[test]
fn audit_prior_generation_unconsumed_preparation_keeps_native_hold_after_expiry() {
    let (dir,mut store,lease)=fixture();let mut value=input();let claim=json!({"fixture":"original"});value.body=json!({"business_claim":claim,"upstream":{}});
    let prepared=store.prepare_budget(&lease,&value,None,10).unwrap();let id=prepared.authorization.budget_id.clone();drop(lease);drop(store);
    let runtime=BridgeBudgetRuntime::start(&dir).unwrap();let r=runtime.recovery_status().unwrap();runtime.recover_local_instance(&r.instance_id,&r.generation,true).unwrap();
    assert_eq!(runtime.sweep_expired_budgets(chrono::Utc::now().timestamp_millis()).unwrap(),1);
    runtime.with_store(|s,l| {
        assert_eq!(s.capacity_totals("account")?.pending,0);assert!(s.budget_execution(&id)?.is_none());
        assert!(s.latest_budget_receipt_event(&id)?.is_some());Ok(())
    }).unwrap();
    println!("AUDIT restart: expired never-sent old generation stays P with 40 held; replay and sweeper cannot recover it");
    drop(runtime);std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_worker_exit_receipt_keeps_capacity_blocked_even_after_late_terminal_result() {
    settled_unknown_rebase(false);
    settled_unknown_rebase(true);
}
fn settled_unknown_rebase(anchored:bool) {
    let (dir,mut s,lease)=fixture();let mut value=input();value.request_id="audit-unknown-settled".into();
    if anchored {
        let packs=[208,209].map(|product|json!({"id":format!("pack-{product}"),"entitlement_base_info":{"product_id":product,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"0"}}));
        s.initialize_capacity_anchor(&lease,"account",&super::bridge_capacity_source::PackObservation::parse(&packs,1).unwrap()).unwrap();
    }
    let prepared=s.prepare_budget(&lease,&value,None,10).unwrap();let id=&prepared.authorization.budget_id;
    let ConsumeOutcome::Granted(c)=s.consume_budget(&lease,&prepared,20).unwrap()else{panic!("consume")};
    s.mark_budget_send_intent(&lease,id,&c.consume_epoch).unwrap();s.bind_budget_task(&lease,id,"audit-video").unwrap();
    let now=chrono::Utc::now().timestamp_millis();s.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",rusqlite::params![now-1_900_000,id]).unwrap();
    s.mark_budget_execution_unknown(&lease,id).unwrap();s.record_budget_worker_exit(&lease,id,now-120_000).unwrap();
    let e=s.budget_execution(id).unwrap().unwrap();let row=json!({"session_id":e.session_ref,"usage_time":now/1000,"date":"2026-10-16","model_name":"seedance","credits_float":"0.5","core_request_id":e.request_id,"core_key_id":e.core_key_id,"ambiguous":false,"core_attribution_ambiguous":false});
    let path=dir.join("data/usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    crate::fs_utils::write_json(&path,&json!({"accounts":{"account":{"name":"fixture","daily":{},"session_usage":{e.session_ref.clone():row.clone()},"session_observations":{e.session_ref.clone():{"row":row,"complete":true,"read_started_at_ms":now-1000,"completed_at_ms":now,"query_end_ms":now+300_000}}}}})).unwrap();
    s.confirm_budget_usage(&lease,id).unwrap();assert_eq!(s.capacity_totals("account").unwrap().pending,0);
    if anchored {
        let fence=s.begin_capacity_rebase(&lease,"account",now).unwrap();
        let packs=[208,209].map(|product|json!({"id":format!("pack-{product}"),"entitlement_base_info":{"product_id":product,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":if product==208 {"0.5"}else{"0"}}}));
        s.commit_dedicated_capacity(&lease,&fence,&super::bridge_capacity_source::PackObservation::parse(&packs,now+1).unwrap()).unwrap();
        assert_eq!(s.capacity_totals("account").unwrap().confirmed,0);
        assert_eq!(s.budget_execution(id).unwrap().unwrap().state,ExecutionState::Unknown);
    } else {assert_eq!(s.begin_capacity_rebase(&lease,"account",now).unwrap_err(),"capacity_rebase_not_quiescent");}
    s.finish_budget_result(&lease,id,ExecutionState::Succeeded,&json!({"id":"audit-video","status":"completed","resource_uri":"tos://fake"}),now-5000).unwrap();
    s.confirm_budget_usage(&lease,id).unwrap();
    assert_eq!(s.budget_execution(id).unwrap().unwrap().state,ExecutionState::Succeeded);
    let policy:String=s.connection.query_row("SELECT confirmation_policy FROM bridge_budget_receipts WHERE budget_id=?1",[id],|r|r.get(0)).unwrap();
    assert_eq!(policy,"post-terminal-session-observation-v1");
    assert_eq!(s.budget_receipt_events(lease.generation(),0,10).unwrap().len(),1,"money outbox is immutable");
    assert!(s.begin_capacity_rebase(&lease,"account",now+2).is_ok());
    println!("AUDIT capacity: pending=0, final receipt and late succeeded result still cannot rebase because receipt policy never upgrades");
    drop(lease);drop(s);std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_expired_partially_unused_package_blocks_new_package_rebase() {
    use super::bridge_capacity_source::PackObservation;
    let old=json!({"id":"old","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"},"start_time":1,"end_time":20},"usage":{"credits_amount":"20"}});
    let new=json!({"id":"new","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"},"start_time":21,"end_time":200},"usage":{"credits_amount":"0"}});
    let before=PackObservation::parse(&[old],10_000).unwrap();let after=PackObservation::parse(&[new],30_000).unwrap();
    assert_eq!(after.balances().unwrap(),(100_000_000,0));after.proves(&before,0).unwrap();
    println!("AUDIT rollover: expired partially unused package disappears; verified new package rejected at old anchor");
}
#[test]
fn expired_package_disposal_covers_only_known_debits_within_its_old_remainder() {
    use super::bridge_capacity_source::PackObservation;
    let old=json!({"id":"old","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"},"end_time":20},"usage":{"credits_amount":"20"}});
    let new=json!({"id":"new","entitlement_base_info":{"product_id":208,"quota":{"credits_limit":"100"}},"usage":{"credits_amount":"0"}});
    let before=PackObservation::parse(&[old],10_000).unwrap();let after=PackObservation::parse(&[new],30_000).unwrap();
    after.proves(&before,40_000_000).unwrap();after.proves(&before,80_000_000).unwrap();
    assert!(after.proves(&before,80_000_001).is_err());
}

#[test]
fn expired_package_allowance_survives_reopen_for_a_late_exact_bill() {
    use super::bridge_capacity_source::PackObservation;
    let (dir,mut s,lease)=fixture();
    let packs=|prefix:&str,expiry:Option<i64>|[208,209].map(|product|json!({"id":format!("{prefix}-{product}"),"entitlement_base_info":{"product_id":product,"quota":{"credits_limit":"100"},"end_time":expiry},"usage":{"credits_amount":"0"}}));
    s.initialize_capacity_anchor(&lease,"account",&PackObservation::parse(&packs("old",Some(20)),1).unwrap()).unwrap();
    let now=chrono::Utc::now().timestamp_millis();let fence=s.begin_capacity_rebase(&lease,"account",now).unwrap();
    s.commit_dedicated_capacity(&lease,&fence,&PackObservation::parse(&packs("new",None),now+1).unwrap()).unwrap();
    drop(s);let mut s=super::bridge_billing::BridgeBillingStore::open(&dir).unwrap();
    let mut value=input();value.request_id="late-expired-pack-bill".into();value.snapshot_epoch=2;value.expires_at_ms=now+60_000;
    let prepared=s.prepare_budget(&lease,&value,None,now+2).unwrap();let id=&prepared.authorization.budget_id;
    let ConsumeOutcome::Granted(c)=s.consume_budget(&lease,&prepared,now+3).unwrap()else{panic!("consume")};
    s.mark_budget_send_intent(&lease,id,&c.consume_epoch).unwrap();s.bind_budget_task(&lease,id,"late-task").unwrap();
    s.finish_budget_result(&lease,id,ExecutionState::Succeeded,&json!({"status":"completed","id":"late-task"}),now+1000).unwrap();
    super::bridge_receipts::tests::cache(&dir,&s,&[(id,"50")],2000);s.confirm_budget_usage(&lease,id).unwrap();
    let fence=s.begin_capacity_rebase(&lease,"account",now+10_000).unwrap();
    s.commit_dedicated_capacity(&lease,&fence,&PackObservation::parse(&packs("new",None),now+10_001).unwrap()).unwrap();
    s.commit_dedicated_capacity(&lease,&fence,&PackObservation::parse(&packs("new",None),now+10_001).unwrap()).unwrap();
    assert_eq!(s.capacity_totals("account").unwrap().confirmed,0);
    let actual:i64=s.connection.query_row("SELECT actual_microcredits FROM bridge_budget_receipts WHERE budget_id=?1",[id],|r|r.get(0)).unwrap();assert_eq!(actual,50_000_000);
    let remaining:i64=s.connection.query_row("SELECT json_extract(coverage_json,'$.expired_allowance_microcredits') FROM bridge_capacity_rebases WHERE owner_nonce=?1",[&fence.owner_nonce],|r|r.get(0)).unwrap();assert_eq!(remaining,150_000_000);
    let next=s.begin_capacity_rebase(&lease,"account",now+20_000).unwrap();
    let mut resurrected=packs("new",None).to_vec();resurrected.extend(packs("old",Some(20)));
    assert!(s.commit_dedicated_capacity(&lease,&next,&PackObservation::parse(&resurrected,now+20_001).unwrap()).is_err(),"expired package identity cannot recreate coverage allowance");
    s.abort_capacity_rebase(&lease,&next).unwrap();
    drop(lease);drop(s);std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn audit_verified_failed_hold_release_still_blocks_account_capacity_refresh() {
    let (dir,mut s,lease)=fixture();let mut value=input();value.request_id="audit-failed-absent".into();
    let prepared=s.prepare_budget(&lease,&value,None,10).unwrap();let id=&prepared.authorization.budget_id;
    let ConsumeOutcome::Granted(c)=s.consume_budget(&lease,&prepared,20).unwrap()else{panic!("consume")};
    s.mark_budget_send_intent(&lease,id,&c.consume_epoch).unwrap();s.bind_budget_task(&lease,id,"audit-failed-task").unwrap();
    let now=chrono::Utc::now().timestamp_millis();s.connection.execute("UPDATE bridge_budget_executions SET started_at_ms=?1 WHERE budget_id=?2",rusqlite::params![now-2_000_000,id]).unwrap();
    s.finish_budget_result(&lease,id,ExecutionState::Failed,&json!({"status":"failed","upstream_error":{"code":"3003"}}),now-1_800_000).unwrap();
    let e=s.budget_execution(id).unwrap().unwrap();let path=dir.join("data/usage_history.json");std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    crate::fs_utils::write_json(&path,&json!({"accounts":{"account":{"name":"fixture","daily":{},"absent_sessions":{e.session_ref:{"started_at_ms":e.started_at_ms,"finished_at_ms":e.finished_at_ms,"query_start_ms":1,"query_end_ms":now+300_000,"first_read_at_ms":now-61_000,"last_read_at_ms":now,"observed_at_ms":now}}}}})).unwrap();
    assert!(s.release_absent_budget_hold(&lease,id,None).unwrap());
    assert_eq!(s.capacity_totals("account").unwrap().awaiting_receipt,0);
    assert!(s.budget_hold_release(id).unwrap().is_some());assert!(s.latest_budget_receipt_event(id).unwrap().is_none());
    assert!(s.begin_capacity_rebase(&lease,"account",now).is_ok());
    println!("AUDIT released failure: explicit terminal failure and durable absence-release proof clear hold but account refresh remains blocked indefinitely without a later bill");
    drop(lease);drop(s);std::fs::remove_dir_all(dir).unwrap();
}
