//! Internal account capacity facts. These are not Core Key balances or upstream prices.
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use super::{bridge_billing::BridgeBillingStore, bridge_budget_lease::BridgeBudgetLease};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all="snake_case")]
pub(super) enum CapacityEligibility { GeneralOnly, GeneralOrWork }

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CapacitySnapshot {
    pub account_ref: String,
    pub snapshot_ref: String,
    pub epoch: i64,
    pub general: i64,
    pub work: i64,
    pub observed_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CapacityReservation {
    pub budget_id: String,
    pub core_key_id: String,
    pub account_ref: String,
    pub snapshot_epoch: i64,
    pub eligibility: CapacityEligibility,
    pub hold: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapacityMutation { Created, Changed, Duplicate }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapacityTransition { ExecutionTerminal, Actual(i64), CancelUnsent }

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct CapacityTotals { pub pending: i64, pub awaiting_receipt: i64, pub confirmed: i64 }

impl BridgeBillingStore {
    /// Initial observation only. A newer number is not proof that it covers D;
    /// replacing an existing observation requires the later fenced rebase flow.
    pub(super) fn initialize_capacity(&mut self, lease: &BridgeBudgetLease, snapshot: &CapacitySnapshot) -> Result<CapacityMutation, String> {
        if !valid_ref(&snapshot.account_ref) || !valid_ref(&snapshot.snapshot_ref)
            || snapshot.epoch <= 0 || snapshot.general < 0 || snapshot.work < 0 || snapshot.observed_at_ms < 0 {
            return Err("invalid capacity snapshot".into());
        }
        checked_add(snapshot.general, snapshot.work)?;
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        require_active_lease(&tx, lease)?;
        let existing = tx.query_row(
            "SELECT snapshot_ref,snapshot_epoch,general_microcredits,work_microcredits,observed_at_ms
             FROM bridge_capacity_accounts WHERE account_ref=?1", [&snapshot.account_ref],
            |row| Ok(CapacitySnapshot { account_ref:snapshot.account_ref.clone(), snapshot_ref:row.get(0)?,epoch:row.get(1)?,general:row.get(2)?,work:row.get(3)?,observed_at_ms:row.get(4)? }),
        ).optional().map_err(db_error)?;
        if let Some(existing) = existing {
            return if existing == *snapshot { Ok(CapacityMutation::Duplicate) }
                else { Err("capacity snapshot replacement requires verified coverage and a rebase fence".into()) };
        }
        tx.execute("INSERT INTO bridge_capacity_accounts
            (account_ref,snapshot_ref,snapshot_epoch,general_microcredits,work_microcredits,observed_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6)",
            params![snapshot.account_ref,snapshot.snapshot_ref,snapshot.epoch,snapshot.general,snapshot.work,snapshot.observed_at_ms]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(CapacityMutation::Created)
    }
    pub(super) fn reserve_capacity(&mut self, lease: &BridgeBudgetLease, input: &CapacityReservation) -> Result<CapacityMutation, String> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let result = reserve_in_transaction(&tx, lease, input)?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    }
    pub(super) fn transition_capacity(&mut self, lease: &BridgeBudgetLease, budget_id: &str, transition: CapacityTransition) -> Result<CapacityMutation, String> {
        let tx = self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        let result = transition_in_transaction(&tx, lease, budget_id, transition)?;
        tx.commit().map_err(db_error)?;
        Ok(result)
    }
    pub(super) fn capacity_totals(&self, account_ref: &str) -> Result<CapacityTotals, String> {
        let mut result = CapacityTotals::default();
        let mut statement = self.connection.prepare(
            "SELECT stage,amount_microcredits FROM bridge_capacity_slots WHERE account_ref=?1 AND stage!='released'"
        ).map_err(db_error)?;
        let rows = statement.query_map([account_ref], |r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?))).map_err(db_error)?;
        for row in rows {
            let (stage, amount) = row.map_err(db_error)?;
            let slot = match stage.as_str() {
                "P" => &mut result.pending, "R" => &mut result.awaiting_receipt, "D" => &mut result.confirmed,
                _ => return Err("invalid stored capacity stage".into()),
            };
            *slot = checked_add(*slot, amount)?;
        }
        Ok(result)
    }
}

fn db_error(error: rusqlite::Error) -> String { format!("bridge capacity database error: {error}") }
fn valid_ref(value: &str) -> bool { !value.is_empty() && value.len() <= 256 && value.trim() == value && !value.chars().any(char::is_control) }
fn checked_add(a: i64, b: i64) -> Result<i64, String> {
    if a < 0 || b < 0 { return Err("negative capacity amount".into()); }
    a.checked_add(b).ok_or_else(|| "capacity amount overflow".into())
}
impl CapacityEligibility {
    fn as_str(self) -> &'static str { match self { Self::GeneralOnly => "general", Self::GeneralOrWork => "general_or_work" } }
}

pub(super) fn require_active_lease(tx: &Transaction<'_>, lease: &BridgeBudgetLease) -> Result<(), String> {
    if !lease.charge_ready() { return Err("active charging lease required".into()); }
    require_fact_lease(tx,lease)
}

// Closing admission must not prevent already-sent workers from persisting facts.
pub(super) fn require_fact_lease(tx:&Transaction<'_>,lease:&BridgeBudgetLease)->Result<(),String> {
    if !lease.is_active() || lease.is_closed() {return Err("active bridge fact owner required".into());}
    let current: (String,String) = tx.query_row("SELECT bridge_instance_id,event_generation FROM bridge_schema_meta WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?))).map_err(db_error)?;
    if current.0 != lease.instance_id() || current.1 != lease.generation() {
        return Err("bridge capacity lease generation changed".into());
    }
    Ok(())
}

// These transaction-level helpers allow the immutable preparation/dispatch
// state and its financial occupancy to commit together in the next layer.
pub(super) fn reserve_in_transaction(tx: &Transaction<'_>, lease: &BridgeBudgetLease, input: &CapacityReservation) -> Result<CapacityMutation,String> {
    require_active_lease(tx,lease)?;
    if !valid_ref(&input.budget_id) || !valid_ref(&input.core_key_id) || !valid_ref(&input.account_ref)
        || input.hold <= 0 || input.snapshot_epoch <= 0 { return Err("invalid capacity reservation".into()); }
    let existing = tx.query_row(
        "SELECT core_key_id,account_ref,snapshot_epoch,eligibility,hold_microcredits,bridge_instance_id,event_generation,stage
         FROM bridge_capacity_slots WHERE budget_id=?1",[&input.budget_id],
        |r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?,r.get::<_,String>(3)?,r.get::<_,i64>(4)?,r.get::<_,String>(5)?,r.get::<_,String>(6)?,r.get::<_,String>(7)?)),
    ).optional().map_err(db_error)?;
    if let Some((key,account,epoch,eligibility,hold,instance,generation,stage))=existing {
        return if key==input.core_key_id && account==input.account_ref && epoch==input.snapshot_epoch
            && eligibility==input.eligibility.as_str() && hold==input.hold && instance==lease.instance_id()
            && generation==lease.generation() && stage!="released" { Ok(CapacityMutation::Duplicate) }
            else { Err("capacity budget identity conflict or already released".into()) };
    }
    let enabled:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_api_keys WHERE key_id=?1 AND active=1 AND snapshot_version>0)",[&input.core_key_id],|r|r.get(0)).map_err(db_error)?;
    if !enabled { return Err("Core Key is not active in bridge registry".into()); }
    check_capacity(tx,input)?;
    tx.execute("INSERT INTO bridge_capacity_slots
        (budget_id,core_key_id,account_ref,snapshot_epoch,bridge_instance_id,event_generation,eligibility,stage,hold_microcredits,amount_microcredits)
        VALUES (?1,?2,?3,?4,?5,?6,?7,'P',?8,?8)",
        params![input.budget_id,input.core_key_id,input.account_ref,input.snapshot_epoch,lease.instance_id(),lease.generation(),input.eligibility.as_str(),input.hold]).map_err(db_error)?;
    Ok(CapacityMutation::Created)
}

pub(super) fn check_capacity(connection: &Connection, input: &CapacityReservation) -> Result<(),String> {
    let (general,work,epoch,state):(i64,i64,i64,String)=connection.query_row(
        "SELECT general_microcredits,work_microcredits,snapshot_epoch,rebase_state FROM bridge_capacity_accounts WHERE account_ref=?1",
        [&input.account_ref],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(db_error)?;
    if epoch!=input.snapshot_epoch || state!="open" { return Err("capacity snapshot stale or account fenced".into()); }
    let (mut q,mut f,mut d)=(0_i64,0_i64,0_i64);
    let mut stmt=connection.prepare("SELECT eligibility,stage,amount_microcredits FROM bridge_capacity_slots WHERE account_ref=?1 AND budget_id!=?2 AND stage!='released'").map_err(db_error)?;
    let rows=stmt.query_map(params![input.account_ref,input.budget_id],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,i64>(2)?))).map_err(db_error)?;
    for row in rows {
        let (eligibility,stage,amount)=row.map_err(db_error)?;
        if stage=="D" { d=checked_add(d,amount)?; }
        else if stage=="P" || stage=="R" {
            if eligibility=="general" { q=checked_add(q,amount)?; }
            else if eligibility=="general_or_work" { f=checked_add(f,amount)?; }
            else { return Err("invalid capacity eligibility".into()); }
        } else { return Err("invalid capacity stage".into()); }
    }
    let total_safe=checked_add(general,work)?.saturating_sub(d).max(0);
    let general_safe=general.saturating_sub(d).max(0);
    let needed=checked_add(checked_add(q,f)?,input.hold)?;
    if needed>total_safe || ((input.eligibility==CapacityEligibility::GeneralOnly || q>0) && needed>general_safe) {
        return Err("upstream_capacity_unavailable".into());
    }
    Ok(())
}

pub(super) fn transition_in_transaction(tx: &Transaction<'_>,lease: &BridgeBudgetLease,budget_id:&str,transition:CapacityTransition)->Result<CapacityMutation,String> {
    if transition==CapacityTransition::CancelUnsent {require_active_lease(tx,lease)?;} else {require_fact_lease(tx,lease)?;}
    let (stage,actual,instance,generation):(String,Option<i64>,String,String)=tx.query_row(
        "SELECT stage,actual_microcredits,bridge_instance_id,event_generation FROM bridge_capacity_slots WHERE budget_id=?1",
        [budget_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(db_error)?;
    if instance!=lease.instance_id() || (transition==CapacityTransition::CancelUnsent && generation!=lease.generation()) { return Err("capacity budget belongs to another lease generation".into()); }
    let (next,amount,actual)=match transition {
        CapacityTransition::ExecutionTerminal => match stage.as_str() {
            "P" => ("R",None,None), "R"|"D"=>return Ok(CapacityMutation::Duplicate),
            _=>return Err("released capacity has no running execution".into()),
        },
        CapacityTransition::Actual(value) => {
            if value<0 { return Err("negative actual capacity consumption".into()); }
            match stage.as_str() {
                "P"|"R" => ("D",Some(value),Some(value)),
                "D" if actual==Some(value) => return Ok(CapacityMutation::Duplicate),
                _=>return Err("capacity settlement conflicts with prior decision".into()),
            }
        },
        CapacityTransition::CancelUnsent => match stage.as_str() {
            "P"=>("released",Some(0),None), "released"=>return Ok(CapacityMutation::Duplicate),
            _=>return Err("capacity cannot be released after execution or settlement".into()),
        },
    };
    let changed=tx.execute("UPDATE bridge_capacity_slots SET stage=?1,amount_microcredits=COALESCE(?2,amount_microcredits),actual_microcredits=?3 WHERE budget_id=?4 AND stage=?5",
        params![next,amount,actual,budget_id,stage]).map_err(db_error)?;
    if changed!=1 { return Err("capacity stage CAS failed".into()); }
    Ok(CapacityMutation::Changed)
}

pub(super) const SCHEMA_OBJECTS: &[(&str, &str, &str)] = &[
    ("table", "bridge_capacity_accounts", "CREATE TABLE bridge_capacity_accounts (
        account_ref TEXT PRIMARY KEY NOT NULL CHECK(length(account_ref) BETWEEN 1 AND 256),
        snapshot_ref TEXT NOT NULL CHECK(length(snapshot_ref) BETWEEN 1 AND 256),
        snapshot_epoch INTEGER NOT NULL CHECK(typeof(snapshot_epoch) = 'integer' AND snapshot_epoch > 0),
        general_microcredits INTEGER NOT NULL CHECK(typeof(general_microcredits) = 'integer' AND general_microcredits >= 0),
        work_microcredits INTEGER NOT NULL CHECK(typeof(work_microcredits) = 'integer' AND work_microcredits >= 0),
        observed_at_ms INTEGER NOT NULL CHECK(typeof(observed_at_ms) = 'integer' AND observed_at_ms >= 0),
        fence_epoch INTEGER NOT NULL DEFAULT 0 CHECK(typeof(fence_epoch) = 'integer' AND fence_epoch >= 0),
        rebase_state TEXT NOT NULL DEFAULT 'open' CHECK(rebase_state IN ('open','fenced')),
        owner_nonce TEXT,
        CHECK((rebase_state = 'open' AND owner_nonce IS NULL) OR
              (rebase_state = 'fenced' AND length(owner_nonce) > 0)),
        CHECK(general_microcredits <= 9223372036854775807 - work_microcredits)
    )"),
    ("table", "bridge_capacity_slots", "CREATE TABLE bridge_capacity_slots (
        budget_id TEXT PRIMARY KEY NOT NULL CHECK(length(budget_id) BETWEEN 1 AND 256),
        core_key_id TEXT NOT NULL REFERENCES bridge_core_api_keys(key_id),
        account_ref TEXT NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
        snapshot_epoch INTEGER NOT NULL CHECK(typeof(snapshot_epoch) = 'integer' AND snapshot_epoch > 0),
        bridge_instance_id TEXT NOT NULL CHECK(length(bridge_instance_id) BETWEEN 1 AND 128),
        event_generation TEXT NOT NULL CHECK(length(event_generation) BETWEEN 1 AND 128),
        eligibility TEXT NOT NULL CHECK(eligibility IN ('general','general_or_work')),
        stage TEXT NOT NULL CHECK(stage IN ('P','R','D','released')),
        hold_microcredits INTEGER NOT NULL CHECK(typeof(hold_microcredits) = 'integer' AND hold_microcredits > 0),
        amount_microcredits INTEGER NOT NULL CHECK(typeof(amount_microcredits) = 'integer' AND amount_microcredits >= 0),
        actual_microcredits INTEGER CHECK(actual_microcredits IS NULL OR
              (typeof(actual_microcredits) = 'integer' AND actual_microcredits >= 0)),
        CHECK((stage IN ('P','R') AND amount_microcredits = hold_microcredits AND actual_microcredits IS NULL) OR
              (stage = 'D' AND actual_microcredits IS NOT NULL AND amount_microcredits = actual_microcredits) OR
              (stage = 'released' AND amount_microcredits = 0 AND actual_microcredits IS NULL))
    )"),
    ("index", "bridge_capacity_slots_by_account_stage", "CREATE INDEX bridge_capacity_slots_by_account_stage
        ON bridge_capacity_slots(account_ref, stage, eligibility)"),
];

pub(super) fn create_schema(transaction: &Transaction<'_>) -> Result<(), String> {
    for (_, name, sql) in SCHEMA_OBJECTS {
        transaction.execute_batch(sql).map_err(|error| format!("bridge capacity {name} creation failed: {error}"))?;
    }
    Ok(())
}

pub(super) fn validate_schema(transaction: &Transaction<'_>) -> Result<(), String> {
    // These objects are new and owned by this migration. Compare their complete
    // tokenized definitions so omitted constraints, foreign keys, or changed
    // collations cannot pass a column-name-only check. No write probes on open.
    for (kind, name, expected) in SCHEMA_OBJECTS {
        let actual: Option<(String, String)> = transaction.query_row(
            "SELECT type, sql FROM sqlite_master WHERE name = ?1", [name],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).optional().map_err(|error| format!("bridge capacity schema lookup failed: {error}"))?;
        let Some((actual_kind, sql)) = actual else { return Err(format!("bridge capacity object missing: {name}")); };
        if actual_kind != *kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?
            != super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {
            return Err(format!("bridge capacity schema differs from supported definition: {name}"));
        }
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{path::PathBuf, sync::{Arc, Barrier}, thread};

    struct Fixture { dir: PathBuf, store: BridgeBillingStore, lease: BridgeBudgetLease }
    fn fixture() -> Fixture {
        let dir = std::env::temp_dir().join(format!("aiwork-capacity-{:032x}", rand::random::<u128>()));
        let mut store = BridgeBillingStore::open(&dir).unwrap();
        store.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1),('key-b','B',1,1)").unwrap();
        let lease = BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        store.initialize_capacity(&lease, &snapshot()).unwrap();
        Fixture { dir, store, lease }
    }
    fn cleanup(f: Fixture) {
        drop(f.lease); drop(f.store); std::fs::remove_dir_all(f.dir).unwrap();
    }
    fn snapshot() -> CapacitySnapshot {
        CapacitySnapshot { account_ref:"account".into(), snapshot_ref:"observation-1".into(), epoch:1, general:100, work:100, observed_at_ms:1 }
    }
    fn reservation(id: &str, key: &str, eligibility: CapacityEligibility, hold: i64) -> CapacityReservation {
        CapacityReservation { budget_id:id.into(), core_key_id:key.into(), account_ref:"account".into(), snapshot_epoch:1, eligibility, hold }
    }

    #[test]
    fn capacity_protects_shared_general_and_video_funds() {
        let mut f = fixture();
        let chat = reservation("chat", "key-a", CapacityEligibility::GeneralOnly, 80);
        assert_eq!(f.store.reserve_capacity(&f.lease, &chat).unwrap(), CapacityMutation::Created);
        assert!(f.store.reserve_capacity(&f.lease, &reservation("video-large", "key-b", CapacityEligibility::GeneralOrWork, 100)).is_err());
        assert!(f.store.reserve_capacity(&f.lease, &reservation("video-small", "key-b", CapacityEligibility::GeneralOrWork, 20)).is_ok());
        assert_eq!(f.store.capacity_totals("account").unwrap().pending,100);
        assert_eq!(f.store.reserve_capacity(&f.lease, &chat).unwrap(), CapacityMutation::Duplicate);
        let mut wrong = chat.clone(); wrong.core_key_id = "key-b".into();
        assert!(f.store.reserve_capacity(&f.lease, &wrong).is_err());
        cleanup(f);
        let mut f = fixture();
        assert!(f.store.reserve_capacity(&f.lease, &reservation("video", "key-b", CapacityEligibility::GeneralOrWork, 150)).is_ok());
        assert!(f.store.reserve_capacity(&f.lease, &reservation("chat", "key-a", CapacityEligibility::GeneralOnly, 10)).is_err());
        cleanup(f);
    }

    #[test]
    fn capacity_phases_are_idempotent_and_overspend_is_not_truncated() {
        let mut f = fixture();
        f.store.reserve_capacity(&f.lease, &reservation("video", "key-a", CapacityEligibility::GeneralOrWork, 40)).unwrap();
        f.store.transition_capacity(&f.lease,"video", CapacityTransition::ExecutionTerminal).unwrap();
        assert_eq!(f.store.capacity_totals("account").unwrap(), CapacityTotals { pending:0,awaiting_receipt:40,confirmed:0 });
        f.store.transition_capacity(&f.lease,"video", CapacityTransition::Actual(70)).unwrap();
        assert_eq!(f.store.transition_capacity(&f.lease,"video", CapacityTransition::Actual(70)).unwrap(),CapacityMutation::Duplicate);
        assert!(f.store.transition_capacity(&f.lease,"video", CapacityTransition::Actual(71)).is_err());
        f.store.transition_capacity(&f.lease,"video", CapacityTransition::ExecutionTerminal).unwrap();
        assert_eq!(f.store.capacity_totals("account").unwrap(), CapacityTotals { pending:0,awaiting_receipt:0,confirmed:70 });
        let mut newer = snapshot(); newer.epoch = 2; newer.snapshot_ref = "later".into();
        assert!(f.store.initialize_capacity(&f.lease,&newer).is_err(),"new balance without coverage must not erase D");
        f.store.reserve_capacity(&f.lease, &reservation("second", "key-b", CapacityEligibility::GeneralOrWork, 50)).unwrap();
        f.store.transition_capacity(&f.lease,"second", CapacityTransition::Actual(60)).unwrap();
        f.store.transition_capacity(&f.lease,"second", CapacityTransition::ExecutionTerminal).unwrap();
        assert_eq!(f.store.capacity_totals("account").unwrap().confirmed,130);
        cleanup(f);
    }

    #[test]
    fn capacity_two_keys_compete_atomically_and_release_cannot_also_charge() {
        let mut f = fixture();
        let barrier = Arc::new(Barrier::new(2));
        let outcomes = thread::scope(|scope| {
            let mut handles = vec![];
            for (id,key) in [("a","key-a"),("b","key-b")] {
                let barrier=barrier.clone(); let dir=&f.dir; let lease=&f.lease;
                handles.push(scope.spawn(move || {
                    let mut store=BridgeBillingStore::open(dir).unwrap(); barrier.wait();
                    store.reserve_capacity(lease,&reservation(id,key,CapacityEligibility::GeneralOnly,80)).is_ok()
                }));
            }
            handles.into_iter().map(|h|h.join().unwrap()).collect::<Vec<_>>()
        });
        assert_eq!(outcomes.into_iter().filter(|v|*v).count(),1);
        assert_eq!(f.store.capacity_totals("account").unwrap().pending,80);
        let id:String=f.store.connection.query_row("SELECT budget_id FROM bridge_capacity_slots",[],|r|r.get(0)).unwrap();
        f.store.transition_capacity(&f.lease,&id,CapacityTransition::CancelUnsent).unwrap();
        assert!(f.store.transition_capacity(&f.lease,&id,CapacityTransition::Actual(80)).is_err());
        assert!(f.store.reserve_capacity(&f.lease,&reservation(&id,"key-a",CapacityEligibility::GeneralOnly,80)).is_err());
        assert_eq!(f.store.capacity_totals("account").unwrap(),CapacityTotals::default());
        cleanup(f);
    }

    #[test]
    fn capacity_rejects_stale_generation_disabled_keys_and_overflow() {
        let mut f=fixture();
        let mut bad=snapshot(); bad.account_ref="overflow".into(); bad.general=i64::MAX; bad.work=1;
        assert!(f.store.initialize_capacity(&f.lease,&bad).is_err());
        f.store.connection.execute("UPDATE bridge_core_api_keys SET active=0 WHERE key_id='key-b'",[]).unwrap();
        assert!(f.store.reserve_capacity(&f.lease,&reservation("disabled","key-b",CapacityEligibility::GeneralOnly,10)).is_err());
        f.store.connection.execute("UPDATE bridge_schema_meta SET event_generation='recovery-required-audit'",[]).unwrap();
        assert!(f.store.reserve_capacity(&f.lease,&reservation("stale","key-a",CapacityEligibility::GeneralOnly,10)).is_err());
        assert_eq!(f.store.capacity_totals("account").unwrap(),CapacityTotals::default());
        cleanup(f);
    }
}
