//! Durable quiescent capacity reconciliation; never infer coverage from a balance.
use super::{bridge_billing::BridgeBillingStore,bridge_budget::{self,CapacitySnapshot},bridge_budget_lease::BridgeBudgetLease};
use rusqlite::{params,Connection,OptionalExtension,Transaction,TransactionBehavior};
use serde::{Deserialize,Serialize};

#[derive(Clone,Debug,PartialEq,Eq,Serialize,Deserialize)]
pub(super) struct RebaseFence {
    pub account_ref:String,pub owner_nonce:String,pub snapshot_epoch:i64,pub fence_epoch:i64,
    pub event_sequence:i64,pub legacy_sessions:i64,pub started_at_ms:i64,
    pub bridge_instance_id:String,pub generation:String,
}
/// Internal adapter evidence, NOT an HTTP DTO. No caller may manufacture this
/// from two equal balances: upstream/official-client in-flight work must also be
/// excluded and the observation must cover this exact receipt watermark.
pub(super) struct CoveredCapacity {
    pub snapshot:CapacitySnapshot,pub covered_event_sequence:i64,pub coverage_ref:String,
    pub quiescent_since_ms:i64,pub external_activity_excluded:bool,
}
pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[
    ("table","bridge_capacity_rebases","CREATE TABLE bridge_capacity_rebases (
        owner_nonce TEXT PRIMARY KEY NOT NULL CHECK(length(owner_nonce) BETWEEN 1 AND 256),
        account_ref TEXT NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
        fence_json TEXT NOT NULL,
        previous_snapshot_json TEXT NOT NULL,
        state TEXT NOT NULL CHECK(state IN ('pending','committed','aborted')),
        coverage_json TEXT,
        CHECK((state='committed' AND coverage_json IS NOT NULL) OR (state!='committed' AND coverage_json IS NULL))
    )"),
    ("index","bridge_capacity_rebases_pending","CREATE UNIQUE INDEX bridge_capacity_rebases_pending ON bridge_capacity_rebases(account_ref) WHERE state='pending'"),
    ("table","bridge_capacity_covered","CREATE TABLE bridge_capacity_covered (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_capacity_slots(budget_id),
        owner_nonce TEXT NOT NULL REFERENCES bridge_capacity_rebases(owner_nonce),
        actual_microcredits INTEGER NOT NULL CHECK(typeof(actual_microcredits)='integer' AND actual_microcredits>=0),
        receipt_hash BLOB NOT NULL CHECK(typeof(receipt_hash)='blob' AND length(receipt_hash)=32)
    )"),
];
fn db_error(e:rusqlite::Error)->String {format!("capacity rebase database error: {e}")}
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(db_error)?;}Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let actual:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((found,sql))=actual else {return Err(format!("missing capacity rebase object: {name}"))};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?!=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {return Err(format!("invalid capacity rebase schema: {name}"));}
    }Ok(())
}
fn valid_ref(s:&str)->bool {!s.is_empty() && s.len()<=256 && s.trim()==s && !s.chars().any(char::is_control)}
fn snapshot(conn:&Connection,account:&str)->Result<CapacitySnapshot,String> {
    conn.query_row("SELECT snapshot_ref,snapshot_epoch,general_microcredits,work_microcredits,observed_at_ms FROM bridge_capacity_accounts WHERE account_ref=?1",[account],
        |r|Ok(CapacitySnapshot {account_ref:account.into(),snapshot_ref:r.get(0)?,epoch:r.get(1)?,general:r.get(2)?,work:r.get(3)?,observed_at_ms:r.get(4)?})).map_err(db_error)
}
fn has_conflict(conn:&Connection,account:&str)->Result<bool,String> {
    conn.query_row("SELECT EXISTS(SELECT 1 FROM bridge_budget_receipts r JOIN bridge_prepared_budgets p ON p.budget_id=r.budget_id WHERE p.account_ref=?1 AND r.receipt_state='conflict')
        OR EXISTS(SELECT 1 FROM bridge_core_upstream_sessions s LEFT JOIN bridge_billing_receipts r ON r.request_id=s.request_id
        JOIN bridge_core_requests q ON q.request_id=s.request_id WHERE s.account_ref=?1 AND (s.conflict=1 OR q.conflict=1 OR r.status='conflict'))",[account],|r|r.get(0)).map_err(db_error)
}
fn quiet_watermark(conn:&Connection,account:&str)->Result<(i64,i64,i64),String> {
    // Check executions independently from slots: even a premature D cannot be
    // interpreted as proof that an upstream operation has stopped consuming.
    let unsafe_state:bool=conn.query_row("SELECT
        EXISTS(SELECT 1 FROM bridge_capacity_slots WHERE account_ref=?1 AND stage IN ('P','R')) OR
        EXISTS(SELECT 1 FROM bridge_prepared_budgets p LEFT JOIN bridge_budget_executions e ON e.budget_id=p.budget_id
            LEFT JOIN bridge_budget_receipts r ON r.budget_id=p.budget_id WHERE p.account_ref=?1 AND
            (p.state IN ('prepared','consumed') OR (p.state='send_intent' AND
            (e.budget_id IS NULL OR e.execution_state NOT IN ('succeeded','failed') OR r.budget_id IS NULL OR r.receipt_state!='final')))) OR
        EXISTS(SELECT 1 FROM bridge_capacity_slots s LEFT JOIN bridge_budget_receipts r ON r.budget_id=s.budget_id
            LEFT JOIN bridge_budget_executions e ON e.budget_id=s.budget_id WHERE s.account_ref=?1 AND s.stage='D' AND
            (r.budget_id IS NULL OR r.receipt_state!='final' OR r.actual_microcredits!=s.actual_microcredits OR
             r.confirmation_policy!='post-terminal-session-observation-v1' OR r.source_read_started_at_ms IS NULL OR
             e.finished_at_ms IS NULL OR r.source_read_started_at_ms<e.finished_at_ms)) OR
        EXISTS(SELECT 1 FROM bridge_core_upstream_sessions s LEFT JOIN bridge_billing_receipts r ON r.request_id=s.request_id
            WHERE s.account_ref=?1 AND (r.request_id IS NULL OR r.status!='final'))",[account],|r|r.get(0)).map_err(db_error)?;
    if unsafe_state || has_conflict(conn,account)? {return Err("capacity_rebase_not_quiescent".into());}
    let event=conn.query_row("SELECT COALESCE(MAX(e.sequence),0) FROM bridge_budget_receipt_events e JOIN bridge_prepared_budgets p ON p.budget_id=e.budget_id WHERE p.account_ref=?1",[account],|r|r.get(0)).map_err(db_error)?;
    let legacy=conn.query_row("SELECT COUNT(*) FROM bridge_core_upstream_sessions WHERE account_ref=?1",[account],|r|r.get(0)).map_err(db_error)?;
    let latest=conn.query_row("SELECT COALESCE(MAX(r.source_read_started_at_ms),0) FROM bridge_budget_receipts r JOIN bridge_prepared_budgets p ON p.budget_id=r.budget_id WHERE p.account_ref=?1",[account],|r|r.get(0)).map_err(db_error)?;
    Ok((event,legacy,latest))
}
fn check_fence(tx:&Transaction<'_>,lease:&BridgeBudgetLease,fence:&RebaseFence)->Result<(String,Option<String>),String> {
    bridge_budget::require_active_lease(tx,lease)?;
    if fence.bridge_instance_id!=lease.instance_id() || fence.generation!=lease.generation() {return Err("capacity_rebase_owner_changed".into());}
    let (stored,state,coverage):(String,String,Option<String>)=tx.query_row("SELECT fence_json,state,coverage_json FROM bridge_capacity_rebases WHERE owner_nonce=?1 AND account_ref=?2",
        params![fence.owner_nonce,fence.account_ref],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(db_error)?;
    let stored:RebaseFence=serde_json::from_str(&stored).map_err(|_|"invalid stored rebase fence")?;
    if stored!=*fence {return Err("capacity_rebase_owner_changed".into());}Ok((state,coverage))
}
fn require_pending_owner(tx:&Transaction<'_>,fence:&RebaseFence)->Result<(),String> {
    let current:(i64,i64,String,Option<String>)=tx.query_row("SELECT snapshot_epoch,fence_epoch,rebase_state,owner_nonce FROM bridge_capacity_accounts WHERE account_ref=?1",[&fence.account_ref],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(db_error)?;
    if current!=(fence.snapshot_epoch,fence.fence_epoch,"fenced".into(),Some(fence.owner_nonce.clone())) {return Err("capacity_rebase_owner_changed".into());}Ok(())
}
/// Called only inside the exclusive instance recovery transaction. Abort a
/// stalled *rebase*, never a financial reservation or a receipt conflict.
pub(super) fn abort_rebases_for_recovery(tx:&Transaction<'_>)->Result<(),String> {
    let broken:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_rebases b LEFT JOIN bridge_capacity_accounts a ON a.account_ref=b.account_ref
        WHERE b.state='pending' AND (a.account_ref IS NULL OR a.rebase_state!='fenced' OR a.owner_nonce IS NULL OR a.owner_nonce!=b.owner_nonce))",[],|r|r.get(0)).map_err(db_error)?;
    if broken {return Err("capacity_rebase_recovery_binding_invalid".into());}
    let rows={let mut stmt=tx.prepare("SELECT account_ref,owner_nonce FROM bridge_capacity_rebases WHERE state='pending'").map_err(db_error)?;
        let rows=stmt.query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).map_err(db_error)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(db_error)?};
    for (account,owner) in rows {
        if has_conflict(tx,&account)? {
            tx.execute("UPDATE bridge_capacity_accounts SET owner_nonce=?1 WHERE account_ref=?2",params![format!("receipt-conflict:{owner}"),account]).map_err(db_error)?;
        } else {tx.execute("UPDATE bridge_capacity_accounts SET rebase_state='open',owner_nonce=NULL WHERE account_ref=?1",[&account]).map_err(db_error)?;}
        tx.execute("UPDATE bridge_capacity_rebases SET state='aborted' WHERE owner_nonce=?1",[owner]).map_err(db_error)?;
    }Ok(())
}
impl BridgeBillingStore {
    pub(super) fn begin_capacity_rebase(&mut self,lease:&BridgeBudgetLease,account:&str,now:i64)->Result<RebaseFence,String> {
        if !valid_ref(account) || now<0 {return Err("invalid capacity rebase request".into());}
        use rand::RngCore;
        let mut entropy=[0u8;32];rand::rngs::OsRng.try_fill_bytes(&mut entropy).map_err(|_|"rebase entropy unavailable")?;
        use base64::Engine;
        let owner=format!("rebase-{}",base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(entropy));
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        let old=snapshot(&tx,account)?;
        let (event,legacy,latest)=quiet_watermark(&tx,account)?;
        if now<old.observed_at_ms || now<latest {return Err("capacity_rebase_observation_in_future".into());}
        let (epoch,state):(i64,String)=tx.query_row("SELECT fence_epoch,rebase_state FROM bridge_capacity_accounts WHERE account_ref=?1",[account],|r|Ok((r.get(0)?,r.get(1)?))).map_err(db_error)?;
        if state!="open" {return Err("capacity_rebase_already_fenced".into());}
        let fence=RebaseFence {account_ref:account.into(),owner_nonce:owner,snapshot_epoch:old.epoch,fence_epoch:epoch.checked_add(1).ok_or("fence overflow")?,
            event_sequence:event,legacy_sessions:legacy,started_at_ms:now,bridge_instance_id:lease.instance_id().into(),generation:lease.generation().into()};
        tx.execute("UPDATE bridge_capacity_accounts SET rebase_state='fenced',owner_nonce=?1,fence_epoch=?2 WHERE account_ref=?3",params![fence.owner_nonce,fence.fence_epoch,account]).map_err(db_error)?;
        tx.execute("INSERT INTO bridge_capacity_rebases(owner_nonce,account_ref,fence_json,previous_snapshot_json,state) VALUES (?1,?2,?3,?4,'pending')",
            params![fence.owner_nonce,account,serde_json::to_string(&fence).map_err(|_|"fence encoding failed")?,serde_json::to_string(&old).map_err(|_|"snapshot encoding failed")?]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;Ok(fence)
    }
    pub(super) fn commit_capacity_rebase(&mut self,lease:&BridgeBudgetLease,fence:&RebaseFence,coverage:&CoveredCapacity)->Result<(),String> {
        let s=&coverage.snapshot;
        if !coverage.external_activity_excluded || !valid_ref(&coverage.coverage_ref) || !valid_ref(&s.snapshot_ref)
            || coverage.quiescent_since_ms<0 || coverage.quiescent_since_ms>fence.started_at_ms
            || coverage.covered_event_sequence!=fence.event_sequence || s.account_ref!=fence.account_ref
            || s.epoch!=fence.snapshot_epoch.checked_add(1).ok_or("snapshot epoch overflow")?
            || s.observed_at_ms<=fence.started_at_ms || s.general<0 || s.work<0 || s.general.checked_add(s.work).is_none() {
            return Err("capacity_rebase_coverage_unproven".into());
        }
        let encoded=serde_json::to_string(&serde_json::json!({"snapshot":s,"coverage_ref":coverage.coverage_ref,
            "event_sequence":coverage.covered_event_sequence,"quiescent_since_ms":coverage.quiescent_since_ms,
            "external_activity_excluded":true})).map_err(|_|"coverage encoding failed")?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let (state,stored)=check_fence(&tx,lease,fence)?;
        if state=="committed" {return if stored.as_deref()==Some(encoded.as_str()) {Ok(())} else {Err("capacity_rebase_coverage_conflict".into())};}
        if state!="pending" {return Err("capacity_rebase_not_pending".into());}
        require_pending_owner(&tx,fence)?;
        let (event,legacy,latest)=quiet_watermark(&tx,&fence.account_ref)?;
        if event!=fence.event_sequence || legacy!=fence.legacy_sessions || latest>fence.started_at_ms {return Err("capacity_rebase_watermark_changed".into());}
        tx.execute("INSERT INTO bridge_capacity_covered(budget_id,owner_nonce,actual_microcredits,receipt_hash)
            SELECT s.budget_id,?1,s.actual_microcredits,r.semantic_hash FROM bridge_capacity_slots s JOIN bridge_budget_receipts r ON r.budget_id=s.budget_id
            WHERE s.account_ref=?2 AND s.stage='D' AND NOT EXISTS(SELECT 1 FROM bridge_capacity_covered c WHERE c.budget_id=s.budget_id)",params![fence.owner_nonce,fence.account_ref]).map_err(db_error)?;
        tx.execute("UPDATE bridge_capacity_rebases SET state='committed',coverage_json=?1 WHERE owner_nonce=?2",params![encoded,fence.owner_nonce]).map_err(db_error)?;
        tx.execute("UPDATE bridge_capacity_accounts SET snapshot_ref=?1,snapshot_epoch=?2,general_microcredits=?3,work_microcredits=?4,observed_at_ms=?5,rebase_state='open',owner_nonce=NULL WHERE account_ref=?6",
            params![s.snapshot_ref,s.epoch,s.general,s.work,s.observed_at_ms,s.account_ref]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
    pub(super) fn abort_capacity_rebase(&mut self,lease:&BridgeBudgetLease,fence:&RebaseFence)->Result<(),String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let (state,_)=check_fence(&tx,lease,fence)?;
        if state=="aborted" {return Ok(());}
        if state!="pending" {return Err("capacity_rebase_not_pending".into());}
        require_pending_owner(&tx,fence)?;
        if has_conflict(&tx,&fence.account_ref)? {
            tx.execute("UPDATE bridge_capacity_accounts SET owner_nonce=?1 WHERE account_ref=?2",params![format!("receipt-conflict:{}",fence.owner_nonce),fence.account_ref]).map_err(db_error)?;
        } else {
            tx.execute("UPDATE bridge_capacity_accounts SET rebase_state='open',owner_nonce=NULL WHERE account_ref=?1",[&fence.account_ref]).map_err(db_error)?;
        }
        tx.execute("UPDATE bridge_capacity_rebases SET state='aborted' WHERE owner_nonce=?1",[&fence.owner_nonce]).map_err(db_error)?;
        tx.commit().map_err(db_error)
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_billing::BridgeBillingStore,bridge_budget::CapacitySnapshot,
        bridge_prepared::tests::{fixture,input,cleanup},bridge_receipts::tests::{send,cache}};

    fn settled(store:&mut BridgeBillingStore,lease:&super::super::bridge_budget_lease::BridgeBudgetLease,dir:&std::path::Path)->String {
        let id=send(store,lease,"request-rebase");cache(dir,store,&[(&id,"50")],2000);
        store.confirm_budget_usage(lease,&id).unwrap();id
    }
    fn observation(fence:&RebaseFence)->CoveredCapacity {
        CoveredCapacity {snapshot:CapacitySnapshot {account_ref:"account".into(),snapshot_ref:"verified-after-fence".into(),epoch:2,
            general:70_000_000,work:80_000_000,observed_at_ms:fence.started_at_ms+20},
            covered_event_sequence:fence.event_sequence,coverage_ref:"fixture-complete-account-coverage".into(),
            quiescent_since_ms:fence.started_at_ms,external_activity_excluded:true}
    }
    #[test]
    fn rebase_requires_quiescence_and_explicit_complete_coverage() {
        let (dir,mut store,lease)=fixture();let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        assert!(store.begin_capacity_rebase(&lease,"account",30).is_err());
        store.cancel_budget(&lease,&prepared,40).unwrap();
        let id=settled(&mut store,&lease,&dir);
        let fence=store.begin_capacity_rebase(&lease,"account",chrono::Utc::now().timestamp_millis()+10_000).unwrap();
        let mut next=input();next.request_id="request-new".into();
        assert!(store.prepare_budget(&lease,&next,None,50).is_err(),"persisted fence must stop new preparation");
        let mut observed=observation(&fence);observed.external_activity_excluded=false;
        assert!(store.commit_capacity_rebase(&lease,&fence,&observed).is_err());
        observed=observation(&fence);observed.snapshot.observed_at_ms=fence.started_at_ms-1;
        assert!(store.commit_capacity_rebase(&lease,&fence,&observed).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        observed=observation(&fence);store.commit_capacity_rebase(&lease,&fence,&observed).unwrap();
        store.commit_capacity_rebase(&lease,&fence,&observed).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,0);
        let amount:i64=store.connection.query_row("SELECT actual_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[&id],|r|r.get(0)).unwrap();
        assert_eq!(amount,50_000_000,"coverage must not erase the original debit");
        next.snapshot_epoch=2;next.hold_microcredits=150_000_000;
        store.prepare_budget(&lease,&next,None,60).unwrap();
        assert_eq!(store.capacity_totals("account").unwrap().pending,150_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn rebase_survives_reopen_and_conflict_prevents_commit_or_unfencing() {
        let (dir,mut store,lease)=fixture();let id=settled(&mut store,&lease,&dir);
        let fence=store.begin_capacity_rebase(&lease,"account",chrono::Utc::now().timestamp_millis()+10_000).unwrap();
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert!(store.begin_capacity_rebase(&lease,"account",fence.started_at_ms+1).is_err());
        cache(&dir,&store,&[(&id,"51")],3000);store.confirm_budget_usage(&lease,&id).unwrap();
        assert!(store.commit_capacity_rebase(&lease,&fence,&observation(&fence)).is_err());
        store.abort_capacity_rebase(&lease,&fence).unwrap();
        let state:String=store.connection.query_row("SELECT rebase_state FROM bridge_capacity_accounts WHERE account_ref='account'",[],|r|r.get(0)).unwrap();
        assert_eq!(state,"fenced","abort must not reopen an account with a receipt conflict");
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn rebase_commit_is_atomic_and_wrong_owner_cannot_release_funds() {
        let (dir,mut store,lease)=fixture();settled(&mut store,&lease,&dir);
        let fence=store.begin_capacity_rebase(&lease,"account",chrono::Utc::now().timestamp_millis()+10_000).unwrap();
        let mut foreign=fence.clone();foreign.owner_nonce="different-owner".into();
        assert!(store.commit_capacity_rebase(&lease,&foreign,&observation(&foreign)).is_err());
        assert!(store.abort_capacity_rebase(&lease,&foreign).is_err());
        store.connection.execute_batch("CREATE TRIGGER fail_rebase BEFORE INSERT ON bridge_capacity_covered BEGIN SELECT RAISE(ABORT,'fixture'); END").unwrap();
        assert!(store.commit_capacity_rebase(&lease,&fence,&observation(&fence)).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().confirmed,50_000_000);
        let epoch:i64=store.connection.query_row("SELECT snapshot_epoch FROM bridge_capacity_accounts",[],|r|r.get(0)).unwrap();assert_eq!(epoch,1);
        store.connection.execute_batch("DROP TRIGGER fail_rebase").unwrap();
        store.abort_capacity_rebase(&lease,&fence).unwrap();
        let next=store.begin_capacity_rebase(&lease,"account",fence.started_at_ms+30).unwrap();
        assert_ne!(next.owner_nonce,fence.owner_nonce);
        assert!(store.commit_capacity_rebase(&lease,&fence,&observation(&fence)).is_err());
        store.commit_capacity_rebase(&lease,&next,&observation(&next)).unwrap();
        cleanup(dir,store,lease);
    }
    #[test]
    fn completed_video_without_receipt_cannot_rebase_and_late_conflict_stays_fenced() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-waiting-bill");
        let now=chrono::Utc::now().timestamp_millis()+10_000;
        assert!(store.begin_capacity_rebase(&lease,"account",now).is_err(),"execution completion does not prove known fees");
        cache(&dir,&store,&[(&id,"50")],2000);store.confirm_budget_usage(&lease,&id).unwrap();
        let fence=store.begin_capacity_rebase(&lease,"account",now).unwrap();
        store.commit_capacity_rebase(&lease,&fence,&observation(&fence)).unwrap();
        cache(&dir,&store,&[(&id,"51")],3000);store.confirm_budget_usage(&lease,&id).unwrap();
        let mut next=input();next.request_id="request-after-conflict".into();next.snapshot_epoch=2;
        assert!(store.prepare_budget(&lease,&next,None,30).is_err(),"a late conflict must fence even already covered receipts");
        assert!(store.begin_capacity_rebase(&lease,"account",now+30).is_err());
        let debit:i64=store.connection.query_row("SELECT actual_microcredits FROM bridge_capacity_covered WHERE budget_id=?1",[&id],|r|r.get(0)).unwrap();
        assert_eq!(debit,50_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn recovery_failure_rolls_back_rebase_abort_and_generation_together() {
        use super::super::bridge_budget_lease::BridgeBudgetLease;
        let (dir,mut store,lease)=fixture();
        let fence=store.begin_capacity_rebase(&lease,"account",30).unwrap();
        drop(lease);let mut lease=BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        let generation=lease.generation().to_owned();
        store.connection.execute_batch("CREATE TRIGGER fail_recovery BEFORE UPDATE ON bridge_schema_meta BEGIN SELECT RAISE(ABORT,'fixture-recovery'); END").unwrap();
        assert!(store.rotate_event_generation_for_recovery(&mut lease,||Ok(())).is_err());
        assert!(!lease.charge_ready());assert_eq!(lease.generation(),generation);
        let state:String=store.connection.query_row("SELECT state FROM bridge_capacity_rebases WHERE owner_nonce=?1",[&fence.owner_nonce],|r|r.get(0)).unwrap();
        assert_eq!(state,"pending","a failed generation CAS must not commit an earlier unfence");
        let owner:String=store.connection.query_row("SELECT owner_nonce FROM bridge_capacity_accounts WHERE account_ref='account'",[],|r|r.get(0)).unwrap();assert_eq!(owner,fence.owner_nonce);
        store.connection.execute_batch("DROP TRIGGER fail_recovery").unwrap();
        store.rotate_event_generation_for_recovery(&mut lease,||Ok(())).unwrap();
        assert!(lease.charge_ready());
        assert!(store.commit_capacity_rebase(&lease,&fence,&observation(&fence)).is_err());
        cleanup(dir,store,lease);
    }
}
