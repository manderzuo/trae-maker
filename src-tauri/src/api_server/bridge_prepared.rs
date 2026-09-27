//! Durable single-send preparation, private to the authenticated bridge adapter.
use super::{bridge_billing::BridgeBillingStore, bridge_budget::{self, CapacityEligibility, CapacityReservation, CapacityTransition}, bridge_budget_lease::BridgeBudgetLease};
use aiwork_core::CreditAmount;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::{rngs::OsRng,RngCore};
use rusqlite::{params,Connection,OptionalExtension,Transaction,TransactionBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest,Sha256};

pub(super) const SCHEMA_OBJECTS: &[(&str,&str,&str)] = &[
    ("table","bridge_prepared_budgets","CREATE TABLE bridge_prepared_budgets (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_capacity_slots(budget_id),
        request_id TEXT NOT NULL CHECK(length(request_id) BETWEEN 1 AND 256),
        core_key_id TEXT NOT NULL REFERENCES bridge_core_api_keys(key_id),
        account_ref TEXT NOT NULL REFERENCES bridge_capacity_accounts(account_ref),
        revision INTEGER NOT NULL CHECK(typeof(revision)='integer' AND revision>0),
        event_generation TEXT NOT NULL,
        authorization_json TEXT NOT NULL,
        protected_payload BLOB NOT NULL CHECK(typeof(protected_payload)='blob' AND length(protected_payload)>0),
        token_hash BLOB NOT NULL CHECK(typeof(token_hash)='blob' AND length(token_hash)=32),
        state TEXT NOT NULL CHECK(state IN ('prepared','consumed','send_intent','canceled','no_send')),
        consume_epoch TEXT,
        disposition_ref TEXT,
        decided_at_ms INTEGER CHECK(decided_at_ms IS NULL OR (typeof(decided_at_ms)='integer' AND decided_at_ms>=0)),
        created_at_ms INTEGER NOT NULL CHECK(typeof(created_at_ms)='integer' AND created_at_ms>=0),
        UNIQUE(request_id,revision),
        CHECK((state='prepared' AND consume_epoch IS NULL AND disposition_ref IS NULL AND decided_at_ms IS NULL) OR
              (state IN ('consumed','send_intent') AND consume_epoch IS NOT NULL AND length(consume_epoch)>0 AND disposition_ref IS NULL AND decided_at_ms IS NULL) OR
              (state='canceled' AND consume_epoch IS NULL AND disposition_ref IS NOT NULL AND length(disposition_ref)>0 AND decided_at_ms IS NOT NULL) OR
              (state='no_send' AND disposition_ref IS NOT NULL AND length(disposition_ref)>0 AND decided_at_ms IS NOT NULL))
    )"),
    ("index","bridge_prepared_one_active_request","CREATE UNIQUE INDEX bridge_prepared_one_active_request
        ON bridge_prepared_budgets(request_id) WHERE state NOT IN ('canceled','no_send')"),
    ("trigger","bridge_capacity_fence_owner_insert","CREATE TRIGGER bridge_capacity_fence_owner_insert
        BEFORE INSERT ON bridge_capacity_accounts
        WHEN NEW.rebase_state='fenced' AND (NEW.owner_nonce IS NULL OR length(NEW.owner_nonce)=0)
        BEGIN SELECT RAISE(ABORT,'capacity fence owner required'); END"),
    ("trigger","bridge_capacity_fence_owner_update","CREATE TRIGGER bridge_capacity_fence_owner_update
        BEFORE UPDATE ON bridge_capacity_accounts
        WHEN NEW.rebase_state='fenced' AND (NEW.owner_nonce IS NULL OR length(NEW.owner_nonce)=0)
        BEGIN SELECT RAISE(ABORT,'capacity fence owner required'); END"),
];

pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    let invalid:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM bridge_capacity_accounts WHERE rebase_state='fenced' AND (owner_nonce IS NULL OR length(owner_nonce)=0))",[],|r|r.get(0)).map_err(db_error)?;
    if invalid {return Err("existing capacity fence has no owner; migration refused".into());}
    for (_,_,sql) in SCHEMA_OBJECTS { tx.execute_batch(sql).map_err(db_error)?; } Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,expected) in SCHEMA_OBJECTS {
        let value:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(db_error)?;
        let Some((found,sql))=value else{return Err(format!("missing prepared budget object: {name}"))};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&sql)? !=super::bridge_billing::tokenize_sqlite_schema_sql(expected)? {
            return Err(format!("invalid prepared budget schema: {name}"));
        }
    } Ok(())
}
fn db_error(error:rusqlite::Error)->String {format!("prepared budget database error: {error}")}
fn random_id(prefix:&str)->Result<String,String> {
    let mut bytes=[0u8;32];OsRng.try_fill_bytes(&mut bytes).map_err(|_|"secure random source unavailable".to_string())?;
    Ok(format!("{prefix}{}",URL_SAFE_NO_PAD.encode(bytes)))
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct PreparedAuthorization {
    pub budget_id:String, pub parent_request_id:String, pub request_id:String, pub core_key_id:String,
    pub request_fingerprint:String, pub endpoint:String, pub model:String, pub account_ref:String,
    pub bridge_instance_id:String, pub profile_fingerprint:String, pub policy_version:String,
    pub hold_credits:CreditAmount, pub expires_at_ms:i64,
}

// Never deserialize this from HTTP: amount, account and policy must be supplied
// by the server's validated planner, not by a caller or language model.
#[derive(Clone,PartialEq,Serialize,Deserialize)]
pub(super) struct TrustedPreparation {
    pub parent_request_id:String, pub request_id:String, pub core_key_id:String,
    pub request_fingerprint:String, pub endpoint:String, pub model:String, pub step_kind:String,
    pub account_ref:String, pub snapshot_epoch:i64, pub eligibility:CapacityEligibility,
    pub policy_version:String, pub pricing_profile_key:String, pub evidence_level:String,
    pub hold_microcredits:i64, pub expires_at_ms:i64, pub body:Value,
}

#[derive(Clone, Serialize)]
pub(super) struct PreparedBudget {
    pub wire_version:u8, pub authorization:PreparedAuthorization,
    pub dispatch_token:String, pub evidence_level:String, pub prepared_at_ms:i64,
    pub revision:i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct NoSendProof {
    pub budget_id:String, pub request_id:String, pub bridge_instance_id:String,
    pub cancel_ref:String, pub revision:i64, pub canceled_at_ms:i64,
}

pub(super) struct ConsumedBudget {
    pub budget:PreparedBudget, pub consume_epoch:String, pub account_ref:String,
    pub session_ref:String, pub body:Value,
}
pub(super) enum ConsumeOutcome { Granted(ConsumedBudget), Existing(String), Rejected(NoSendProof) }

impl BridgeBillingStore {
    pub(super) fn prepare_budget(&mut self,lease:&BridgeBudgetLease,input:&TrustedPreparation,replacement:Option<&str>,now:i64)->Result<PreparedBudget,String> {
        validate_input(input,now)?;
        let previous=last_row(&self.connection,&input.request_id)?;
        let revision=previous.as_ref().map_or(Some(1),|row|row.revision.checked_add(1)).ok_or("budget revision overflow")?;
        let budget_id=random_id("budget_")?;
        let token=random_id("dispatch_")?;
        let session_ref=format!("core-v2-{budget_id}");
        let fingerprint=URL_SAFE_NO_PAD.encode(aiwork_core::canonical_json_hash(&serde_json::json!({
            "body":input.body,"account_ref":input.account_ref,"session_ref":session_ref,
            "policy_version":input.policy_version,"pricing_profile_key":input.pricing_profile_key,
        })));
        let authorization=PreparedAuthorization {
            budget_id:budget_id.clone(),parent_request_id:input.parent_request_id.clone(),request_id:input.request_id.clone(),
            core_key_id:input.core_key_id.clone(),request_fingerprint:input.request_fingerprint.clone(),endpoint:input.endpoint.clone(),
            model:input.model.clone(),account_ref:input.account_ref.clone(),bridge_instance_id:lease.instance_id().into(),
            profile_fingerprint:fingerprint,policy_version:input.policy_version.clone(),
            hold_credits:CreditAmount::parse(&format!("{}.{:06}",input.hold_microcredits/1_000_000,input.hold_microcredits%1_000_000),"credits")?,
            expires_at_ms:input.expires_at_ms,
        };
        let envelope=ProtectedPreparation {version:1,purpose:"bridge-budget-preparation".into(),authorization,input:input.clone(),generation:lease.generation().into(),revision,
            dispatch_token:token,session_ref,prepared_at_ms:now};
        let plaintext=serde_json::to_vec(&envelope).map_err(|_|"budget protection encoding failed")?;
        if plaintext.len()>8*1024*1024 {return Err("prepared body exceeds protected storage limit".into());}
        let protected=crate::vault::protect_blob(&plaintext)?;
        let token_hash=Sha256::digest(envelope.dispatch_token.as_bytes());
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        if let Some(current)=last_row(&tx,&input.request_id)? {
            let previous=current.decrypt()?;
            if !same_original(&previous.input,input) {return Err("prepared request identity conflict".into());}
            if current.state!="canceled" && current.state!="no_send" {
                let mut comparable=input.clone();comparable.expires_at_ms=previous.input.expires_at_ms;
                if previous.input!=comparable || previous.generation!=lease.generation() {return Err("prepared profile or generation conflict".into());}
                return Ok(previous.response());
            }
            if replacement!=current.disposition_ref.as_deref() || replacement.is_none() || current.revision.checked_add(1)!=Some(revision) {
                return Err("replacement requires the latest durable cancellation proof".into());
            }
        } else if replacement.is_some() {return Err("replacement references a missing preparation".into());}
        let reservation=envelope.capacity_reservation();
        bridge_budget::reserve_in_transaction(&tx,lease,&reservation)?;
        tx.execute("INSERT INTO bridge_prepared_budgets
            (budget_id,request_id,core_key_id,account_ref,revision,event_generation,authorization_json,protected_payload,token_hash,state,created_at_ms)
            VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,'prepared',?10)",
            params![budget_id,input.request_id,input.core_key_id,input.account_ref,revision,lease.generation(),
                serde_json::to_string(&envelope.authorization).map_err(|_|"budget authorization encoding failed")?,protected,token_hash.as_slice(),now]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(envelope.response())
    }
    pub(super) fn cancel_budget(&mut self,lease:&BridgeBudgetLease,budget:&PreparedBudget,now:i64)->Result<NoSendProof,String> {
        let verified=verified_budget(&self.connection,budget)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        let row=required_row(&tx,&budget.authorization.budget_id)?;
        require_same_ciphertext(&row,&verified)?;
        if row.state=="canceled" || row.state=="no_send" {return row.proof();}
        if row.state!="prepared" {return Err("consumed budget cannot be canceled".into());}
        let proof=decide_no_send(&tx,lease,&row,"canceled",now)?;
        tx.commit().map_err(db_error)?; Ok(proof)
    }
    pub(super) fn consume_budget(&mut self,lease:&BridgeBudgetLease,budget:&PreparedBudget,now:i64)->Result<ConsumeOutcome,String> {
        let verified=verified_budget(&self.connection,budget)?;
        let envelope=verified.decrypt()?;
        let epoch=random_id("consume_")?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        let row=required_row(&tx,&budget.authorization.budget_id)?;
        require_same_ciphertext(&row,&verified)?;
        if row.state=="canceled" || row.state=="no_send" {return Ok(ConsumeOutcome::Rejected(row.proof()?));}
        if row.state!="prepared" {return Ok(ConsumeOutcome::Existing(row.state));}
        if row.generation!=lease.generation() || envelope.authorization.bridge_instance_id!=lease.instance_id() {
            return Err("prepared budget generation requires recovery".into());
        }
        let active=key_active(&tx,&row.core_key_id)?;
        let capacity=bridge_budget::check_capacity(&tx,&envelope.capacity_reservation());
        if now<0 {return Err("invalid consume time".into());}
        if now>=envelope.authorization.expires_at_ms || !active || capacity.is_err() {
            let proof=decide_no_send(&tx,lease,&row,"no_send",now)?;
            tx.commit().map_err(db_error)?; return Ok(ConsumeOutcome::Rejected(proof));
        }
        tx.execute("UPDATE bridge_prepared_budgets SET state='consumed',consume_epoch=?1 WHERE budget_id=?2 AND state='prepared'",params![epoch,row.budget_id]).map_err(db_error)?;
        tx.commit().map_err(db_error)?;
        Ok(ConsumeOutcome::Granted(ConsumedBudget {budget:envelope.response(),consume_epoch:epoch,account_ref:envelope.input.account_ref,
            session_ref:envelope.session_ref,body:envelope.input.body}))
    }
    pub(super) fn mark_budget_send_intent(&mut self,lease:&BridgeBudgetLease,budget_id:&str,consume_epoch:&str)->Result<bool,String> {
        let verified=required_row(&self.connection,budget_id)?;
        let envelope=verified.decrypt()?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        let row=required_row(&tx,budget_id)?; require_same_ciphertext(&row,&verified)?;
        if row.generation!=lease.generation() || row.consume_epoch.as_deref()!=Some(consume_epoch) {return Err("send epoch or generation mismatch".into());}
        if row.state!="consumed" {return Ok(false);}
        if !key_active(&tx,&row.core_key_id)? {return Err("Core Key disabled before send".into());}
        bridge_budget::check_capacity(&tx,&envelope.capacity_reservation())?;
        let updated=tx.execute("UPDATE bridge_prepared_budgets SET state='send_intent' WHERE budget_id=?1 AND state='consumed' AND consume_epoch=?2",params![budget_id,consume_epoch]).map_err(db_error)?;
        if updated==1 {
            super::bridge_execution::register_send(&tx,&envelope.execution_identity(),chrono::Utc::now().timestamp_millis(),super::bridge_execution::ExecutionState::Running)?;
        }
        tx.commit().map_err(db_error)?; Ok(updated==1)
    }
    pub(super) fn fail_budget_before_send(&mut self,lease:&BridgeBudgetLease,budget_id:&str,consume_epoch:&str,now:i64)->Result<NoSendProof,String> {
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db_error)?;
        bridge_budget::require_active_lease(&tx,lease)?;
        let row=required_row(&tx,budget_id)?;
        if row.consume_epoch.as_deref()!=Some(consume_epoch) {return Err("no-send epoch mismatch".into());}
        if row.state=="no_send" {return row.proof();}
        if row.state!="consumed" {return Err("send may have occurred; no-send proof unavailable".into());}
        let proof=decide_no_send(&tx,lease,&row,"no_send",now)?;
        tx.commit().map_err(db_error)?; Ok(proof)
    }
}

#[derive(Serialize,Deserialize)]
struct ProtectedPreparation {
    version:u8,purpose:String,authorization:PreparedAuthorization,input:TrustedPreparation,
    generation:String,revision:i64,dispatch_token:String,session_ref:String,prepared_at_ms:i64,
}
impl ProtectedPreparation {
    fn execution_identity(&self)->super::bridge_execution::ExecutionIdentity {
        super::bridge_execution::ExecutionIdentity {authorization:self.authorization.clone(),session_ref:self.session_ref.clone(),step_kind:self.input.step_kind.clone()}
    }
    fn response(&self)->PreparedBudget {PreparedBudget {wire_version:2,authorization:self.authorization.clone(),dispatch_token:self.dispatch_token.clone(),
        evidence_level:self.input.evidence_level.clone(),prepared_at_ms:self.prepared_at_ms,revision:self.revision}}
    fn capacity_reservation(&self)->CapacityReservation {CapacityReservation {budget_id:self.authorization.budget_id.clone(),core_key_id:self.input.core_key_id.clone(),
        account_ref:self.input.account_ref.clone(),snapshot_epoch:self.input.snapshot_epoch,eligibility:self.input.eligibility,hold:self.input.hold_microcredits}}
}
pub(super) fn sent_execution_identity(connection:&Connection,budget_id:&str)->Result<super::bridge_execution::ExecutionIdentity,String> {
    let row=required_row(connection,budget_id)?;
    if row.state!="send_intent" {return Err("budget has no durable send intent".into());}
    Ok(row.decrypt()?.execution_identity())
}
pub(super) fn budget_no_send_disposition(connection:&Connection,budget_id:&str)->Result<(PreparedAuthorization,NoSendProof),String> {
    let row=required_row(connection,budget_id)?;
    let envelope=row.decrypt()?;
    let proof=row.proof()?;
    Ok((envelope.authorization,proof))
}
pub(super) fn stored_budget_authorization(connection:&Connection,budget_id:&str)->Result<PreparedAuthorization,String> {
    Ok(required_row(connection,budget_id)?.decrypt()?.authorization)
}
struct PreparedRow {
    budget_id:String,request_id:String,core_key_id:String,account_ref:String,revision:i64,generation:String,
    authorization_json:String,ciphertext:Vec<u8>,token_hash:Vec<u8>,state:String,consume_epoch:Option<String>,disposition_ref:Option<String>,decided_at:Option<i64>,created_at:i64,
}
impl PreparedRow {
    fn from_row(r:&rusqlite::Row<'_>)->rusqlite::Result<Self> {Ok(Self {
        budget_id:r.get(0)?,request_id:r.get(1)?,core_key_id:r.get(2)?,account_ref:r.get(3)?,revision:r.get(4)?,generation:r.get(5)?,
        authorization_json:r.get(6)?,ciphertext:r.get(7)?,token_hash:r.get(8)?,state:r.get(9)?,consume_epoch:r.get(10)?,disposition_ref:r.get(11)?,decided_at:r.get(12)?,created_at:r.get(13)?,
    })}
    fn decrypt(&self)->Result<ProtectedPreparation,String> {
        let plaintext=crate::vault::unprotect_blob(&self.ciphertext)?;
        let env:ProtectedPreparation=serde_json::from_slice(&plaintext).map_err(|_|"protected preparation is invalid")?;
        let auth:PreparedAuthorization=serde_json::from_str(&self.authorization_json).map_err(|_|"stored authorization is invalid")?;
        if env.version!=1 || env.purpose!="bridge-budget-preparation" || env.authorization!=auth || env.authorization.budget_id!=self.budget_id
            || env.input.request_id!=self.request_id || env.input.core_key_id!=self.core_key_id || env.input.account_ref!=self.account_ref
            || env.revision!=self.revision || env.generation!=self.generation || env.prepared_at_ms!=self.created_at
            || !same_hash(&Sha256::digest(env.dispatch_token.as_bytes()),&self.token_hash) {
            return Err("protected budget binding mismatch".into());
        }
        Ok(env)
    }
    fn proof(&self)->Result<NoSendProof,String> {
        if self.state!="canceled" && self.state!="no_send" {return Err("durable no-send decision missing".into());}
        let auth:PreparedAuthorization=serde_json::from_str(&self.authorization_json).map_err(|_|"stored authorization is invalid")?;
        Ok(NoSendProof {budget_id:self.budget_id.clone(),request_id:self.request_id.clone(),bridge_instance_id:auth.bridge_instance_id,
            cancel_ref:self.disposition_ref.clone().ok_or("no-send reference missing")?,revision:self.revision,canceled_at_ms:self.decided_at.ok_or("no-send time missing")?})
    }
}
const ROW_COLUMNS:&str="budget_id,request_id,core_key_id,account_ref,revision,event_generation,authorization_json,protected_payload,token_hash,state,consume_epoch,disposition_ref,decided_at_ms,created_at_ms";
fn last_row(connection:&Connection,request_id:&str)->Result<Option<PreparedRow>,String> {
    connection.query_row(&format!("SELECT {ROW_COLUMNS} FROM bridge_prepared_budgets WHERE request_id=?1 ORDER BY revision DESC LIMIT 1"),[request_id],PreparedRow::from_row).optional().map_err(db_error)
}
fn required_row(connection:&Connection,budget_id:&str)->Result<PreparedRow,String> {
    connection.query_row(&format!("SELECT {ROW_COLUMNS} FROM bridge_prepared_budgets WHERE budget_id=?1"),[budget_id],PreparedRow::from_row).map_err(db_error)
}
fn same_hash(a:&[u8],b:&[u8])->bool {a.len()==b.len() && a.iter().zip(b).fold(0_u8,|v,(a,b)|v|(a^b))==0}
fn verified_budget(connection:&Connection,budget:&PreparedBudget)->Result<PreparedRow,String> {
    if budget.dispatch_token.len()>256 {return Err("invalid dispatch token".into());}
    let row=required_row(connection,&budget.authorization.budget_id)?;
    if !same_hash(&Sha256::digest(budget.dispatch_token.as_bytes()),&row.token_hash) {return Err("invalid dispatch token".into());}
    let env=row.decrypt()?;
    if budget.wire_version!=2 || budget.authorization!=env.authorization || budget.revision!=env.revision
        || budget.evidence_level!=env.input.evidence_level || budget.prepared_at_ms!=env.prepared_at_ms {
        return Err("dispatch authorization binding mismatch".into());
    } Ok(row)
}
fn require_same_ciphertext(current:&PreparedRow,verified:&PreparedRow)->Result<(),String> {
    if current.ciphertext!=verified.ciphertext || current.authorization_json!=verified.authorization_json || current.token_hash!=verified.token_hash
        || current.request_id!=verified.request_id || current.core_key_id!=verified.core_key_id || current.account_ref!=verified.account_ref
        || current.revision!=verified.revision || current.generation!=verified.generation || current.created_at!=verified.created_at {
        return Err("budget changed while acquiring execution transaction".into());
    } Ok(())
}
fn key_active(connection:&Connection,key:&str)->Result<bool,String> {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM bridge_core_api_keys WHERE key_id=?1 AND active=1 AND snapshot_version>0)",[key],|r|r.get(0)).map_err(db_error)
}
fn decide_no_send(tx:&Transaction<'_>,lease:&BridgeBudgetLease,row:&PreparedRow,state:&str,now:i64)->Result<NoSendProof,String> {
    if now<0 || (row.state!="prepared" && row.state!="consumed") {return Err("invalid no-send transition".into());}
    let reference=random_id("no-send_")?;
    bridge_budget::transition_in_transaction(tx,lease,&row.budget_id,CapacityTransition::CancelUnsent)?;
    let changed=tx.execute("UPDATE bridge_prepared_budgets SET state=?1,disposition_ref=?2,decided_at_ms=?3 WHERE budget_id=?4 AND state=?5",params![state,reference,now,row.budget_id,row.state]).map_err(db_error)?;
    if changed!=1 {return Err("no-send CAS lost".into());}
    required_row(tx,&row.budget_id)?.proof()
}
fn same_original(a:&TrustedPreparation,b:&TrustedPreparation)->bool {
    a.request_id==b.request_id && a.parent_request_id==b.parent_request_id && a.core_key_id==b.core_key_id
        && a.request_fingerprint==b.request_fingerprint && a.endpoint==b.endpoint && a.model==b.model && a.step_kind==b.step_kind && a.body==b.body
}
fn validate_input(input:&TrustedPreparation,now:i64)->Result<(),String> {
    let refs=[&input.parent_request_id,&input.request_id,&input.core_key_id,&input.request_fingerprint,&input.endpoint,&input.model,&input.account_ref,&input.policy_version,&input.pricing_profile_key];
    if refs.iter().any(|v|v.is_empty() || v.len()>256 || v.trim()!=v.as_str() || v.chars().any(char::is_control))
        || !matches!(input.step_kind.as_str(),"assist"|"chat"|"video") || !matches!(input.evidence_level.as_str(),"native_estimate"|"observed_actual"|"policy_only")
        || input.snapshot_epoch<=0 || input.hold_microcredits<=0 || now<0 || input.expires_at_ms<=now || !input.body.is_object() {
        return Err("invalid trusted budget preparation".into());
    } Ok(())
}

#[cfg(all(test,windows))]
pub(super) mod tests {
    use super::*;
    use super::super::bridge_budget::CapacitySnapshot;
    use std::{path::PathBuf,sync::{Arc,Barrier},thread};
    pub(crate) fn fixture()->(PathBuf,BridgeBillingStore,BridgeBudgetLease) {
        let dir=std::env::temp_dir().join(format!("aiwork-prepared-{:032x}",rand::random::<u128>()));
        let mut store=BridgeBillingStore::open(&dir).unwrap();
        store.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").unwrap();
        let lease=BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        store.initialize_capacity(&lease,&CapacitySnapshot {account_ref:"account".into(),snapshot_ref:"snapshot".into(),epoch:1,general:100_000_000,work:100_000_000,observed_at_ms:1}).unwrap();
        (dir,store,lease)
    }
    pub(crate) fn input()->TrustedPreparation {
        TrustedPreparation {parent_request_id:"request-parent".into(),request_id:"request-video".into(),core_key_id:"key-a".into(),request_fingerprint:"core-fingerprint".into(),endpoint:"videos".into(),model:"seedance".into(),step_kind:"video".into(),account_ref:"account".into(),snapshot_epoch:1,eligibility:CapacityEligibility::GeneralOrWork,policy_version:"policy-1".into(),pricing_profile_key:"720p-5s-test-policy".into(),evidence_level:"policy_only".into(),hold_microcredits:40_000_000,expires_at_ms:1000,body:serde_json::json!({"prompt":"test fixture only","duration":5})}
    }
    pub(crate) fn cleanup(dir:PathBuf,store:BridgeBillingStore,lease:BridgeBudgetLease) {
        drop(lease); drop(store); std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn preparation_replay_is_encrypted_bound_and_single_occupancy() {
        let (dir,mut store,lease)=fixture();
        let original=store.prepare_budget(&lease,&input(),None,10).unwrap();
        drop(store); let mut store=BridgeBillingStore::open(&dir).unwrap();
        let replay=store.prepare_budget(&lease,&input(),None,20).unwrap();
        assert_eq!(original.dispatch_token,replay.dispatch_token);
        assert_eq!(original.authorization.budget_id,replay.authorization.budget_id);
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        let ciphertext:Vec<u8>=store.connection.query_row("SELECT protected_payload FROM bridge_prepared_budgets",[],|r|r.get(0)).unwrap();
        assert!(!String::from_utf8_lossy(&ciphertext).contains("test fixture only"));
        assert!(!String::from_utf8_lossy(&ciphertext).contains(&original.dispatch_token));
        let mut wrong=input();wrong.body["duration"]=serde_json::json!(10);
        assert!(store.prepare_budget(&lease,&wrong,None,30).is_err());
        let mut token_tampered=original.clone();token_tampered.dispatch_token="wrong".into();
        assert!(store.consume_budget(&lease,&token_tampered,30).is_err());
        cleanup(dir,store,lease);
    }

    #[test]
    fn cancel_and_consume_race_has_exactly_one_winner() {
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let barrier=Arc::new(Barrier::new(2));
        let (canceled,consumed)=thread::scope(|scope| {
            let b=barrier.clone();let p=&prepared;let l=&lease;let d=&dir;
            let cancel=scope.spawn(move || {let mut s=BridgeBillingStore::open(d).unwrap(); b.wait(); s.cancel_budget(l,p,20).is_ok()});
            let b=barrier.clone();let p=&prepared;let l=&lease;let d=&dir;
            let consume=scope.spawn(move || {let mut s=BridgeBillingStore::open(d).unwrap(); b.wait(); matches!(s.consume_budget(l,p,20),Ok(ConsumeOutcome::Granted(_)))});
            (cancel.join().unwrap(),consume.join().unwrap())
        });
        assert_ne!(canceled,consumed);
        assert_eq!(store.capacity_totals("account").unwrap().pending,if canceled {0}else{40_000_000});
        cleanup(dir,store,lease);
    }

    #[test]
    fn send_intent_is_once_and_excludes_no_send_refund() {
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let ConsumeOutcome::Granted(context)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("first consume must grant")};
        let barrier=Arc::new(Barrier::new(2));
        let (sent,failed)=thread::scope(|scope| {
            let b=barrier.clone();let d=&dir;let l=&lease;let c=&context;
            let send=scope.spawn(move || {let mut s=BridgeBillingStore::open(d).unwrap();b.wait();s.mark_budget_send_intent(l,&c.budget.authorization.budget_id,&c.consume_epoch).unwrap_or(false)});
            let b=barrier.clone();let d=&dir;let l=&lease;let c=&context;
            let fail=scope.spawn(move || {let mut s=BridgeBillingStore::open(d).unwrap();b.wait();s.fail_budget_before_send(l,&c.budget.authorization.budget_id,&c.consume_epoch,30).is_ok()});
            (send.join().unwrap(),fail.join().unwrap())
        });
        assert_ne!(sent,failed);
        assert!(!store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&context.consume_epoch).unwrap_or(false));
        assert!(!matches!(store.consume_budget(&lease,&prepared,2000).unwrap(),ConsumeOutcome::Granted(_)));
        assert_eq!(store.capacity_totals("account").unwrap().pending,if sent {40_000_000}else{0});
        cleanup(dir,store,lease);
    }

    #[test]
    fn expired_unconsumed_budget_has_durable_no_send_and_replacement() {
        let (dir,mut store,lease)=fixture();
        let first=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let ConsumeOutcome::Rejected(proof)=store.consume_budget(&lease,&first,1000).unwrap() else {panic!("expired prepared must reject with proof")};
        assert_eq!(store.capacity_totals("account").unwrap().pending,0);
        drop(store); let mut store=BridgeBillingStore::open(&dir).unwrap();
        let ConsumeOutcome::Rejected(replayed)=store.consume_budget(&lease,&first,2000).unwrap() else {panic!("proof must survive reopen")};
        assert_eq!(proof,replayed);
        let mut replacement=input();replacement.expires_at_ms=3000;
        assert!(store.prepare_budget(&lease,&replacement,None,2000).is_err());
        let next=store.prepare_budget(&lease,&replacement,Some(&proof.cancel_ref),2000).unwrap();
        assert_ne!(first.authorization.budget_id,next.authorization.budget_id);
        assert_eq!(next.revision,2);
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        cleanup(dir,store,lease);
    }

    #[test]
    fn prepared_state_constraints_reject_null_send_and_fence_proofs() {
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let invalid_consume=store.connection.execute("UPDATE bridge_prepared_budgets SET state='consumed',consume_epoch=NULL WHERE budget_id=?1",[&prepared.authorization.budget_id]);
        let invalid_fence=store.connection.execute("UPDATE bridge_capacity_accounts SET rebase_state='fenced',owner_nonce=NULL",[]);
        cleanup(dir,store,lease);
        assert!(invalid_consume.is_err(),"NULL is not evidence of a consume epoch");
        assert!(invalid_fence.is_err(),"NULL is not evidence of a rebase owner");
    }

    #[test]
    fn ciphertext_transplant_and_disabled_key_cannot_dispatch() {
        let (dir,mut store,lease)=fixture();
        let first=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let mut other=input();other.request_id="request-other".into();
        let second=store.prepare_budget(&lease,&other,None,10).unwrap();
        let original:Vec<u8>=store.connection.query_row("SELECT protected_payload FROM bridge_prepared_budgets WHERE budget_id=?1",[&first.authorization.budget_id],|r|r.get(0)).unwrap();
        store.connection.execute("UPDATE bridge_prepared_budgets SET protected_payload=(SELECT protected_payload FROM bridge_prepared_budgets WHERE budget_id=?1) WHERE budget_id=?2",params![second.authorization.budget_id,first.authorization.budget_id]).unwrap();
        assert!(store.consume_budget(&lease,&first,20).is_err());
        store.connection.execute("UPDATE bridge_prepared_budgets SET protected_payload=?1 WHERE budget_id=?2",params![original,first.authorization.budget_id]).unwrap();
        store.connection.execute("UPDATE bridge_core_api_keys SET active=0",[]).unwrap();
        assert!(matches!(store.consume_budget(&lease,&first,20).unwrap(),ConsumeOutcome::Rejected(_)));
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000,"only the rejected budget is released");
        cleanup(dir,store,lease);
    }

    #[test]
    fn consumed_budget_expiry_does_not_release_or_allow_second_sender() {
        let (dir,mut store,lease)=fixture();
        let prepared=store.prepare_budget(&lease,&input(),None,10).unwrap();
        let ConsumeOutcome::Granted(context)=store.consume_budget(&lease,&prepared,20).unwrap() else {panic!("expected first consume")};
        assert!(!context.session_ref.is_empty());
        assert_eq!(context.account_ref,"account");
        assert_eq!(context.body,input().body);
        assert!(matches!(store.consume_budget(&lease,&prepared,2000).unwrap(),ConsumeOutcome::Existing(ref state) if state=="consumed"));
        assert!(store.cancel_budget(&lease,&prepared,2000).is_err());
        assert_eq!(store.capacity_totals("account").unwrap().pending,40_000_000);
        assert!(store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&context.consume_epoch).unwrap());
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert!(!store.mark_budget_send_intent(&lease,&prepared.authorization.budget_id,&context.consume_epoch).unwrap());
        assert!(store.fail_budget_before_send(&lease,&prepared.authorization.budget_id,&context.consume_epoch,3000).is_err());
        cleanup(dir,store,lease);
    }
}
