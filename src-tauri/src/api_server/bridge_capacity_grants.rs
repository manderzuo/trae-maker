//! Additive native UNUSED package witnesses. Never rebase or release old fees.
//! Principal is immutable; current package confirmation is bounded to a 60s
//! preparation window and rechecked by reserve, consume and send-intent.
use std::sync::atomic::{AtomicBool,Ordering};
use std::collections::BTreeMap;
use rusqlite::{params,Connection,OptionalExtension,Transaction,TransactionBehavior};
use serde_json::Value;
use sha2::{Digest,Sha256};
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease,
    bridge_capacity_source::{self,PackObservation,PackUse}};

static ENABLED:AtomicBool=AtomicBool::new(true);
pub(crate) fn disable_for_process() {ENABLED.store(false,Ordering::Release);}
pub(super) fn enabled_for_process()->bool {ENABLED.load(Ordering::Acquire)}
const CURRENT_VALIDITY_MS:i64=60_000;

pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[("table","bridge_capacity_grant_observations","CREATE TABLE bridge_capacity_grant_observations (
    account_ref TEXT PRIMARY KEY NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
    last_observed_at_ms INTEGER NOT NULL CHECK(typeof(last_observed_at_ms)='integer' AND last_observed_at_ms>=0)
)"),("table","bridge_capacity_grants","CREATE TABLE bridge_capacity_grants (
    account_ref TEXT NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
    pack_ref TEXT NOT NULL CHECK(length(pack_ref)=64 AND pack_ref NOT GLOB '*[^0-9a-f]*'),
    product INTEGER NOT NULL CHECK(product IN (208,209,221)),
    limit_microcredits INTEGER NOT NULL CHECK(typeof(limit_microcredits)='integer' AND limit_microcredits>0),
    start_ms INTEGER NOT NULL CHECK(typeof(start_ms)='integer' AND start_ms>=0),
    end_ms INTEGER NOT NULL CHECK(typeof(end_ms)='integer' AND end_ms>start_ms),
    first_observed_at_ms INTEGER NOT NULL CHECK(typeof(first_observed_at_ms)='integer' AND first_observed_at_ms>=start_ms AND first_observed_at_ms<end_ms),
    evidence_hash TEXT NOT NULL CHECK(length(evidence_hash)=64 AND evidence_hash NOT GLOB '*[^0-9a-f]*'),
    last_observed_at_ms INTEGER NOT NULL CHECK(typeof(last_observed_at_ms)='integer' AND last_observed_at_ms>=first_observed_at_ms),
    current_valid INTEGER NOT NULL CHECK(current_valid IN (0,1)),
    PRIMARY KEY(account_ref,pack_ref)
)"),("trigger","bridge_capacity_grants_immutable","CREATE TRIGGER bridge_capacity_grants_immutable BEFORE UPDATE OF account_ref,pack_ref,product,limit_microcredits,start_ms,end_ms,first_observed_at_ms,evidence_hash ON bridge_capacity_grants
    WHEN OLD.account_ref!=NEW.account_ref OR OLD.pack_ref!=NEW.pack_ref OR OLD.product!=NEW.product OR OLD.limit_microcredits!=NEW.limit_microcredits OR OLD.start_ms!=NEW.start_ms OR OLD.end_ms!=NEW.end_ms OR OLD.first_observed_at_ms!=NEW.first_observed_at_ms OR OLD.evidence_hash!=NEW.evidence_hash
    BEGIN SELECT RAISE(ABORT,'immutable native package witness'); END")];
fn db_error(_:rusqlite::Error)->String {"capacity_grant_storage_unavailable".into()}
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(db_error)?;}Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let found:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((actual,sql))=found else{return Err("capacity_grant_schema_missing".into())};
        if actual!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)?!=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {return Err("capacity_grant_schema_invalid".into());}
    }Ok(())
}

struct CurrentPack {facts:PackUse,unused:bool,active:bool,evidence_hash:String,numeric_alias:Option<String>}
struct CurrentPacks {packages:BTreeMap<String,CurrentPack>,source:PackObservation}
fn current_packs(packs:&[Value],started_at:i64)->Result<CurrentPacks,String> {
    let parsed=PackObservation::parse(packs,started_at)?;
    let mut current=BTreeMap::new();
    for raw in packs {
        // Do not invent aliases for numeric/string ID migrations. Existing
        // anchors retain their original hashing; new witnesses need native IDs.
        let Some(id)=raw.pointer("/entitlement_base_info/entitlement_id").and_then(Value::as_str)
            .filter(|s|!s.is_empty() && s.len()<=256) else {continue};
        let encoded=serde_json::to_vec(&serde_json::json!({"id":id})).map_err(|_|"capacity_grant_source_invalid")?;
        let key=format!("{:x}",Sha256::digest(encoded));
        let numeric_alias=if id.bytes().all(|b|b.is_ascii_digit()) {
            id.parse::<u64>().ok().map(|n|format!("{:x}",Sha256::digest(serde_json::to_vec(&serde_json::json!({"id":n})).unwrap())))
        } else {None};
        let Some(facts)=parsed.packs.get(&key).cloned() else {continue};
        let active=raw["status"].as_i64()==Some(1) && raw.pointer("/entitlement_base_info/ent_status").and_then(Value::as_i64)==Some(0)
            && facts.start_ms.is_some_and(|v|v<=started_at) && facts.end_ms.is_some_and(|v|v>started_at);
        let evidence_hash=format!("{:x}",Sha256::digest(serde_json::to_vec(raw).map_err(|_|"capacity_grant_source_invalid")?));
        if current.insert(key,CurrentPack {facts,unused:bridge_capacity_source::native_unused_credit(raw),active,evidence_hash,numeric_alias}).is_some() {
            return Err("capacity_grant_source_ambiguous".into());
        }
    }Ok(CurrentPacks {packages:current,source:parsed})
}

// Called only after native read success and while the current runtime owns its
// lease. Network I/O is outside this transaction; older reads cannot overwrite
// later package confirmation even if the old request completes last.
impl BridgeBillingStore {
    pub(super) fn refresh_native_grants(&mut self,lease:&BridgeBudgetLease,account:&str,packs:&[Value],started_at:i64)->Result<(),String> {
        if !ENABLED.load(Ordering::Acquire) || !bridge_capacity_source::dedicated_enabled(&self.data_dir,account)? {return Ok(());}
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        super::bridge_budget::require_active_lease(&tx,lease)?;
        let result=match current_packs(packs,started_at) {
            Ok(current)=>refresh_in_tx(&tx,account,&current,started_at),
            Err(_)=>invalidate_current(&tx,account,started_at).map(|_|()),
        };
        if let Err(error)=result {
            tx.rollback().map_err(db_error)?;
            self.invalidate_native_grants(lease,account,started_at)?;
            return Err(error);
        }
        tx.commit().map_err(db_error)
    }
    pub(super) fn invalidate_native_grants(&mut self,lease:&BridgeBudgetLease,account:&str,started_at:i64)->Result<(),String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        super::bridge_budget::require_active_lease(&tx,lease)?;
        invalidate_current(&tx,account,started_at)?;tx.commit().map_err(db_error)
    }
}
fn invalidate_current(tx:&Transaction<'_>,account:&str,started_at:i64)->Result<bool,String> {
    if started_at<0 || started_at>chrono::Utc::now().timestamp_millis() {return Err("capacity_grant_observation_stale".into());}
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_accounts WHERE account_ref=?1)",[account],|r|r.get(0)).map_err(db_error)?;
    if !exists {return Ok(false);}
    let newest:Option<i64>=tx.query_row("SELECT last_observed_at_ms FROM bridge_capacity_grant_observations WHERE account_ref=?1",[account],|r|r.get(0)).optional().map_err(db_error)?;
    if newest.is_some_and(|t|t>=started_at) {
        // Millisecond-equal reads have no defensible order. Fail closed rather
        // than allow a conflicting response to silently keep a package valid.
        if newest==Some(started_at) {tx.execute("UPDATE bridge_capacity_grants SET current_valid=0 WHERE account_ref=?1",[account]).map_err(db_error)?;}
        return Ok(false);
    }
    tx.execute("INSERT INTO bridge_capacity_grant_observations(account_ref,last_observed_at_ms) VALUES(?1,?2) ON CONFLICT(account_ref) DO UPDATE SET last_observed_at_ms=excluded.last_observed_at_ms",params![account,started_at]).map_err(db_error)?;
    tx.execute("UPDATE bridge_capacity_grants SET current_valid=0,last_observed_at_ms=?1 WHERE account_ref=?2 AND last_observed_at_ms<=?1",params![started_at,account]).map_err(db_error)?;
    Ok(true)
}
fn refresh_in_tx(tx:&Transaction<'_>,account:&str,current:&CurrentPacks,started_at:i64)->Result<(),String> {
    let binding:Option<(i64,i64,String,String)>=tx.query_row("SELECT s.snapshot_epoch,a.snapshot_epoch,s.rebase_state,a.observation_json FROM bridge_capacity_accounts s JOIN bridge_capacity_anchors a ON a.account_ref=s.account_ref WHERE s.account_ref=?1",
        [account],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(db_error)?;
    let Some((snapshot,anchor,state,encoded))=binding else {return Ok(())};
    if snapshot!=anchor || state!="open" {return Err("capacity_grant_anchor_unavailable".into());}
    let previous:PackObservation=serde_json::from_str(&encoded).map_err(|_|"capacity_grant_anchor_invalid")?;
    previous.validate()?;
    if started_at<previous.observed_at_ms || started_at>chrono::Utc::now().timestamp_millis() {return Err("capacity_grant_observation_stale".into());}
    let original=super::bridge_rebase::snapshot(tx,account)?;
    // The account row may already be ahead of the anchor: a reconciliation pass
    // updates the live balance before it finishes the rebase, and a rebase that
    // does not commit leaves the two permanently unequal. Requiring exact
    // equality therefore fenced every such account out of grant refresh forever.
    // The anchor stays the baseline this comparison is anchored on; only an
    // account row OLDER than the anchor is a genuine inconsistency.
    if original.observed_at_ms<previous.observed_at_ms {return Err("capacity_grant_anchor_invalid".into());}
    for (key,old) in &previous.packs {
        match current.source.packs.get(key) {
            // (1) Same package period: consumption may only grow. A used drop
            //     inside one unchanged period is the only per-period tamper signal.
            Some(next) if old.product==next.product && old.limit==next.limit && old.start_ms==next.start_ms && old.end_ms==next.end_ms && next.used>=old.used=>{},
            // (2) Same product, next renewal period, or a larger limit. Upstream
            //     monthly packs roll over and get upgraded; treating that as an
            //     authorization change fenced every account out of grant refresh
            //     at each rollover, which surfaced to callers as a 503. A smaller
            //     limit or a different product is still rejected below.
            Some(next) if old.product==next.product && next.limit>=old.limit
                && (old.start_ms!=next.start_ms || old.end_ms!=next.end_ms || next.limit>old.limit)=>{},
            // (3) The old package expired and is no longer reported. Lifetime end
            //     does not require the package to have been fully consumed.
            None if old.end_ms.is_some_and(|t|t<=started_at)=>{},
            _=>return Err("capacity_grant_source_binding_changed".into()),
        }
    }
    // Missing/unknown/revoked packages are invalidated, without deleting their
    // initial UNUSED proof or changing any P/R/D slot or billing receipt.
    if !invalidate_current(tx,account,started_at)? {return Ok(());}
    for (key,pack) in &current.packages {
        let f=&pack.facts;
        let existing:Option<(i64,i64,i64,i64)>=tx.query_row("SELECT product,limit_microcredits,start_ms,end_ms FROM bridge_capacity_grants WHERE account_ref=?1 AND pack_ref=?2",params![account,key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(db_error)?;
        if let Some((product,limit,start,end))=existing {
            let valid=pack.active && product==f.product && limit==f.limit && Some(start)==f.start_ms && Some(end)==f.end_ms;
            tx.execute("UPDATE bridge_capacity_grants SET current_valid=?1,last_observed_at_ms=?2 WHERE account_ref=?3 AND pack_ref=?4 AND last_observed_at_ms<=?2",
                params![i64::from(valid),started_at,account,key]).map_err(db_error)?;
        } else if pack.active && pack.unused && f.limit>0 && !previous.packs.contains_key(key)
            && !pack.numeric_alias.as_ref().is_some_and(|alias|previous.packs.contains_key(alias)) {
            tx.execute("INSERT INTO bridge_capacity_grants(account_ref,pack_ref,product,limit_microcredits,start_ms,end_ms,first_observed_at_ms,evidence_hash,last_observed_at_ms,current_valid) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?7,1)",
                params![account,key,f.product,f.limit,f.start_ms,f.end_ms,started_at,pack.evidence_hash]).map_err(db_error)?;
        }
    }Ok(())
}

pub(super) fn usable_grants(conn:&Connection,account:&str,now:i64)->Result<(i64,i64),String> {
    if !ENABLED.load(Ordering::Acquire) {return Ok((0,0));}
    let Some(root)=conn.path().and_then(|p|std::path::Path::new(p).parent()) else {return Ok((0,0))};
    // Revoking dedicated-account authorization takes effect at every reserve,
    // consume/send check, not only when a future native refresh is scheduled.
    if !bridge_capacity_source::dedicated_enabled(root,account)? {return Ok((0,0));}
    usable_grants_at(conn,account,now)
}
fn usable_grants_at(conn:&Connection,account:&str,now:i64)->Result<(i64,i64),String> {
    if now<0 {return Err("capacity_grant_clock_invalid".into());}
    let mut stmt=conn.prepare("SELECT g.product,g.limit_microcredits FROM bridge_capacity_grants g JOIN bridge_capacity_grant_observations o ON o.account_ref=g.account_ref JOIN bridge_capacity_anchors a ON a.account_ref=g.account_ref JOIN bridge_capacity_accounts s ON s.account_ref=g.account_ref
        WHERE g.account_ref=?1 AND s.snapshot_epoch=a.snapshot_epoch AND s.rebase_state='open'
        AND g.current_valid=1 AND g.start_ms<=?2 AND g.end_ms>?2
        AND g.last_observed_at_ms=o.last_observed_at_ms AND g.last_observed_at_ms<=?2 AND g.last_observed_at_ms>=?3
        AND NOT EXISTS(SELECT 1 FROM json_each(a.observation_json,'$.packs') p WHERE p.key=g.pack_ref)").map_err(db_error)?;
    let rows=stmt.query_map(params![account,now,now.saturating_sub(CURRENT_VALIDITY_MS)],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?))).map_err(db_error)?;
    let (mut general,mut work)=(0i64,0i64);
    for row in rows {
        let (product,amount)=row.map_err(db_error)?;
        if amount<=0 || !matches!(product,208|209|221) {return Err("capacity_grant_invalid_amount".into());}
        let total=if product==209 {&mut work} else {&mut general};
        *total=total.checked_add(amount).ok_or("capacity_grant_overflow")?;
    }general.checked_add(work).ok_or("capacity_grant_overflow")?;Ok((general,work))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn pack(id:&str,used:Option<&str>,now:i64)->Value {
        serde_json::json!({"status":1,"entitlement_base_info":{"entitlement_id":id,"ent_status":0,"product_id":221,"quota":{"credits_limit":"200"},"start_time":(now-10_000)/1000,"end_time":(now+600_000)/1000},"usage":used.map(|v|serde_json::json!({"credits_amount":v})).unwrap_or_else(||serde_json::json!({}))})
    }
    fn fixture(now:i64)->Connection {
        let mut conn=Connection::open_in_memory().unwrap();
        let tx=conn.transaction().unwrap();
        super::super::bridge_budget::create_schema(&tx).unwrap();
        bridge_capacity_source::create_schema(&tx).unwrap();
        create_schema(&tx).unwrap();
        let mut baseline=pack("baseline",Some("20"),now);
        baseline["entitlement_base_info"]["quota"]["credits_limit"]=serde_json::json!("100");
        let previous=PackObservation::parse(&[baseline],now-1000).unwrap();
        tx.execute("INSERT INTO bridge_capacity_accounts(account_ref,snapshot_ref,snapshot_epoch,general_microcredits,work_microcredits,observed_at_ms) VALUES('account','baseline',1,80000000,0,?1)",[now-1000]).unwrap();
        tx.execute("INSERT INTO bridge_capacity_anchors VALUES('account',1,0,0,?1)",[serde_json::to_string(&previous).unwrap()]).unwrap();
        tx.commit().unwrap();conn
    }
    fn refresh(conn:&mut Connection,packs:&[Value],started:i64) {
        let encoded:String=conn.query_row("SELECT observation_json FROM bridge_capacity_anchors",[],|r|r.get(0)).unwrap();
        let anchor:PackObservation=serde_json::from_str(&encoded).unwrap();
        let baseline_key=format!("{:x}",Sha256::digest(b"{\"id\":\"baseline\"}"));
        let mut complete=packs.to_vec();
        if let Some(base)=anchor.packs.get(&baseline_key) {
            complete.push(serde_json::json!({"status":1,"entitlement_base_info":{"entitlement_id":"baseline","ent_status":0,"product_id":base.product,"quota":{"credits_limit":"100"},"start_time":base.start_ms.unwrap()/1000,"end_time":base.end_ms.unwrap()/1000},"usage":{"credits_amount":"20"}}));
        }
        let tx=conn.transaction().unwrap();
        match current_packs(&complete,started) {
            Ok(current)=>refresh_in_tx(&tx,"account",&current,started).unwrap(),
            Err(_)=>{invalidate_current(&tx,"account",started).unwrap();},
        }tx.commit().unwrap();
    }
    #[test]
    fn native_unused_principal_is_credited_once_and_survives_accounted_usage() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        let unused=pack("new-credit",None,now);refresh(&mut conn,&[unused.clone()],now);
        refresh(&mut conn,&[unused],now+1);
        assert_eq!(usable_grants_at(&conn,"account",now+2).unwrap(),(200_000_000,0));
        refresh(&mut conn,&[pack("new-credit",Some("10"),now)],now+3);
        assert_eq!(usable_grants_at(&conn,"account",now+4).unwrap(),(200_000_000,0),"D separately accounts for use; do not double-debit the grant");
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM bridge_capacity_grants",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert!(conn.execute("UPDATE bridge_capacity_grants SET limit_microcredits=201000000",[]).is_err());
        assert_eq!(conn.query_row("SELECT general_microcredits FROM bridge_capacity_accounts",[],|r|r.get::<_,i64>(0)).unwrap(),80_000_000);
    }
    #[test]
    fn missing_revoked_unknown_and_changed_native_packages_stop_authorizing() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        let original=pack("new-credit",None,now);refresh(&mut conn,&[original.clone()],now);
        for (i,kind) in ["missing","revoked","unknown","quota_changed","numeric_id"].iter().enumerate() {
            let mut v=original.clone();match *kind {
                "revoked"=>v["entitlement_base_info"]["ent_status"]=serde_json::json!(1),
                "unknown"=>v["usage"]=Value::Null,
                "quota_changed"=>v["entitlement_base_info"]["quota"]["credits_limit"]=serde_json::json!("201"),
                "numeric_id"=>v["entitlement_base_info"]["entitlement_id"]=serde_json::json!(12345),_=>{},
            }
            let when=now+10+i as i64*2;
            let values=if *kind=="missing" {vec![]} else {vec![v]};
            refresh(&mut conn,&values,when);
            assert_eq!(usable_grants_at(&conn,"account",when+1).unwrap(),(0,0),"{kind}");
        }
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM bridge_capacity_grants",[],|r|r.get::<_,i64>(0)).unwrap(),1);
        assert_eq!(conn.query_row("SELECT limit_microcredits FROM bridge_capacity_grants",[],|r|r.get::<_,i64>(0)).unwrap(),200_000_000);
    }
    #[test]
    fn older_absent_or_unused_reads_cannot_resurrect_a_newer_rejection() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        refresh(&mut conn,&[],now+20);
        refresh(&mut conn,&[pack("new-credit",None,now)],now+10);
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM bridge_capacity_grants",[],|r|r.get::<_,i64>(0)).unwrap(),0);
        refresh(&mut conn,&[pack("new-credit",None,now)],now+30);
        refresh(&mut conn,&[],now+40);
        refresh(&mut conn,&[pack("new-credit",None,now)],now+35);
        assert_eq!(usable_grants_at(&conn,"account",now+41).unwrap(),(0,0));
    }
    #[test]
    fn equal_read_timestamps_do_not_keep_an_ambiguous_package_usable() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        refresh(&mut conn,&[pack("new-credit",None,now)],now);
        refresh(&mut conn,&[],now);
        assert_eq!(usable_grants_at(&conn,"account",now+1).unwrap(),(0,0));
    }
    #[test]
    fn a_package_already_used_before_first_observation_is_not_a_new_grant() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        refresh(&mut conn,&[pack("new-credit",Some("1"),now)],now);
        assert_eq!(usable_grants_at(&conn,"account",now+1).unwrap(),(0,0));
    }
    #[test]
    fn clock_rollback_stale_confirmation_and_expiration_are_closed() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        refresh(&mut conn,&[pack("new-credit",None,now)],now);
        assert_eq!(usable_grants_at(&conn,"account",now-1).unwrap(),(0,0));
        assert_eq!(usable_grants_at(&conn,"account",now+60_001).unwrap(),(0,0));
        assert_eq!(usable_grants_at(&conn,"account",now+600_000).unwrap(),(0,0));
    }
    #[test]
    fn a_new_anchor_absorbs_the_witness_without_counting_it_twice() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        let grant=pack("new-credit",None,now);refresh(&mut conn,&[grant.clone()],now);
        let raw:String=conn.query_row("SELECT observation_json FROM bridge_capacity_anchors",[],|r|r.get(0)).unwrap();
        let mut next:PackObservation=serde_json::from_str(&raw).unwrap();
        next.packs.extend(PackObservation::parse(&[grant],now).unwrap().packs);next.observed_at_ms=now+1;
        conn.execute("UPDATE bridge_capacity_accounts SET snapshot_epoch=2,general_microcredits=280000000,observed_at_ms=?1",[now+1]).unwrap();
        conn.execute("UPDATE bridge_capacity_anchors SET snapshot_epoch=2,observation_json=?1",[serde_json::to_string(&next).unwrap()]).unwrap();
        assert_eq!(usable_grants_at(&conn,"account",now+2).unwrap(),(0,0));
    }
    #[test]
    fn numeric_legacy_id_does_not_become_a_second_string_id_grant() {
        let now=chrono::Utc::now().timestamp_millis()-100;let mut conn=fixture(now);
        let mut old=pack("12345",None,now);old["entitlement_base_info"]["entitlement_id"]=serde_json::json!(12345);
        let previous=PackObservation::parse(&[old],now-1000).unwrap();
        conn.execute("UPDATE bridge_capacity_accounts SET general_microcredits=200000000",[]).unwrap();
        conn.execute("UPDATE bridge_capacity_anchors SET observation_json=?1",[serde_json::to_string(&previous).unwrap()]).unwrap();
        let current=current_packs(&[pack("12345",None,now)],now).unwrap();
        let tx=conn.transaction().unwrap();
        assert!(refresh_in_tx(&tx,"account",&current,now).is_err(),"an ID-type shift must be rejected, not treated as new credit");
        tx.rollback().unwrap();
        assert_eq!(usable_grants_at(&conn,"account",now+1).unwrap(),(0,0));
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM bridge_capacity_grants",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    }
    #[cfg(windows)]
    #[test]
    fn native_grants_are_checked_at_prepare_consume_and_before_send_with_unknown_costs_retained() {
        use super::super::{bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome},bridge_execution::ExecutionState};
        for revoked in [false,true] {
            let (dir,mut store,lease)=fixture();
            std::fs::write(dir.join("bridge-dedicated-accounts.json"),b"{\"version\":1,\"accounts\":[\"account\"]}").unwrap();
            let base=|id:&str,product:i64|serde_json::json!({"status":1,"entitlement_base_info":{"entitlement_id":id,"ent_status":0,"product_id":product,"quota":{"credits_limit":"100"},"start_time":0,"end_time":9999999999_i64},"usage":{"credits_amount":"0"}});
            let bases=vec![base("base-general",208),base("base-work",209)];
            let previous=PackObservation::parse(&bases,1).unwrap();
            store.initialize_capacity_anchor(&lease,"account",&previous).unwrap();
            let original=store.prepare_budget(&lease,&input(),None,10).unwrap();
            let ConsumeOutcome::Granted(old)=store.consume_budget(&lease,&original,20).unwrap() else {panic!("original consume")};
            store.mark_budget_send_intent(&lease,&original.authorization.budget_id,&old.consume_epoch).unwrap();
            store.finish_budget_result(&lease,&original.authorization.budget_id,ExecutionState::Failed,&serde_json::json!({"error":"fixture-only"}),chrono::Utc::now().timestamp_millis()).unwrap();
            let now=chrono::Utc::now().timestamp_millis()-50;
            let mut grant=pack("new-credit",None,now);grant["entitlement_base_info"]["quota"]["credits_limit"]=serde_json::json!("1000");
            let mut current=bases.clone();current.push(grant);
            store.refresh_native_grants(&lease,"account",&current,now).unwrap();
            let mut next=input();next.request_id="request-new-grant".into();next.hold_microcredits=400_000_000;next.expires_at_ms=now+60_000;
            let prepared=store.prepare_budget(&lease,&next,None,now+1).unwrap();
            let ConsumeOutcome::Granted(consumed)=store.consume_budget(&lease,&prepared,now+2).unwrap() else {panic!("new package consume")};
            if revoked {store.refresh_native_grants(&lease,"account",&bases,now+3).unwrap();}
            let sent=store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&consumed.consume_epoch);
            if revoked {
                assert!(sent.is_err(),"a disappeared package cannot authorize paid send after consume");
                assert!(store.budget_execution(&prepared.authorization.budget_id).unwrap().is_none());
                store.fail_budget_before_send(&lease,&prepared.authorization.budget_id,&consumed.consume_epoch,now+4).unwrap();
            } else {assert!(sent.unwrap());}
            let old:(String,i64,Option<i64>)=store.connection.query_row("SELECT stage,amount_microcredits,actual_microcredits FROM bridge_capacity_slots WHERE budget_id=?1",[&original.authorization.budget_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).unwrap();
            assert_eq!(old,("R".into(),40_000_000,None));
            cleanup(dir,store,lease);
        }
    }
}
