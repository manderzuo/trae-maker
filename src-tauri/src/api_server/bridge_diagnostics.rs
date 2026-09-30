//! Safe execution diagnostics are facts, not receipts or proof of zero charge.
use serde::{Deserialize,Serialize};
use serde_json::{json,Value};
use rusqlite::{params,OptionalExtension,Transaction,TransactionBehavior};
use super::{bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease,bridge_execution::ExecutionIdentity};

#[derive(Clone,Debug,Serialize,Deserialize,PartialEq,Eq)]
pub(crate) struct Diagnostic {
    pub code:String,
    pub stage:String,
    pub outcome_known:bool,
    #[serde(skip_serializing_if="Option::is_none")]
    pub upstream_error:Option<aiwork_core::UpstreamFailure>,
}
impl From<&str> for Diagnostic {
    fn from(code:&str)->Self {Self {code:code.into(),stage:"assistant_stream".into(),outcome_known:false,upstream_error:None}}
}
impl Diagnostic {
    pub fn provider(value:&Value)->Self {
        Self {code:"chat_upstream_error".into(),stage:"assistant_stream".into(),outcome_known:true,
            upstream_error:Some(aiwork_core::UpstreamFailure::from_value(value))}
    }
    pub fn http(status:u16,body:&str)->Self {
        // make_upstream_request distinguishes HTTP bodies from local transport
        // explanations. Its local transport errors are not provider HTTP 502.
        let local=body.starts_with("DNS解析失败") || body.starts_with("连接超时") || body.starts_with("上游网络请求超时")
            || body.starts_with("TLS证书验证失败") || body.starts_with("传输错误");
        if local {
            let code=if body.starts_with("连接超时") || body.starts_with("上游网络请求超时") {"chat_connection_timeout"}
                else if body.starts_with("DNS解析失败") {"chat_dns_failed"}
                else if body.starts_with("TLS证书验证失败") {"chat_tls_failed"} else {"chat_transport_failed"};
            return Self {stage:"assistant_connection".into(),..Self::from(code)};
        }
        Self {code:"chat_upstream_error".into(),stage:"assistant_http".into(),outcome_known:true,
            upstream_error:Some(aiwork_core::UpstreamFailure::from_body(body,Some(status)))}
    }
    pub fn result(&self)->Value {json!({"error":self,"upstream_error":self.upstream_error})}
}

pub(super) const SCHEMA_OBJECTS:&[(&str,&str,&str)]=&[("table","bridge_execution_diagnostics",
    "CREATE TABLE bridge_execution_diagnostics (
        budget_id TEXT PRIMARY KEY NOT NULL REFERENCES bridge_budget_executions(budget_id),
        ciphertext BLOB NOT NULL CHECK(typeof(ciphertext)='blob' AND length(ciphertext)>0),
        digest BLOB NOT NULL CHECK(typeof(digest)='blob' AND length(digest)=32)
    )")];
pub(super) fn create_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (_,_,sql) in SCHEMA_OBJECTS {tx.execute_batch(sql).map_err(|e|e.to_string())?;} Ok(())
}
pub(super) fn validate_schema(tx:&Transaction<'_>)->Result<(),String> {
    for (kind,name,sql) in SCHEMA_OBJECTS {
        let actual:Option<(String,String)>=tx.query_row("SELECT type,sql FROM sqlite_master WHERE name=?1",[name],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
        let Some((found,definition))=actual else {return Err("missing execution diagnostics schema".into())};
        if found!=*kind || super::bridge_billing::tokenize_sqlite_schema_sql(&definition)?!=super::bridge_billing::tokenize_sqlite_schema_sql(sql)? {return Err("invalid execution diagnostics schema".into());}
    } Ok(())
}
#[derive(Serialize,Deserialize)]
struct Protected {version:u8,identity:ExecutionIdentity,diagnostic:Diagnostic}
impl BridgeBillingStore {
    pub(super) fn save_budget_diagnostic(&mut self,lease:&BridgeBudgetLease,id:&str,diagnostic:&Diagnostic)->Result<(),String> {
        // No capacity, receipt or money mutation accompanies a diagnostic.
        let execution=self.budget_execution(id)?.ok_or("diagnostic execution missing")?;
        let identity=super::bridge_prepared::sent_execution_identity(&self.connection,id)?;
        let protected=Protected {version:1,identity,diagnostic:diagnostic.clone()};
        let value=serde_json::to_value(&protected).map_err(|_|"diagnostic encoding failed")?;
        let digest=aiwork_core::canonical_json_hash(&value);
        let bytes=serde_json::to_vec(&value).map_err(|_|"diagnostic encoding failed")?;
        if bytes.len()>16384 {return Err("diagnostic too large".into());}
        let cipher=crate::vault::protect_blob(&bytes)?;
        let tx=self.connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(|e|e.to_string())?;
        super::bridge_budget::require_fact_lease(&tx,lease)?;
        if execution.bridge_instance_id!=lease.instance_id() {return Err("diagnostic owner mismatch".into());}
        let state:String=tx.query_row("SELECT execution_state FROM bridge_budget_executions WHERE budget_id=?1",[id],|r|r.get(0)).map_err(|e|e.to_string())?;
        if matches!(state.as_str(),"succeeded"|"failed") {return Ok(());}
        tx.execute("INSERT INTO bridge_execution_diagnostics VALUES (?1,?2,?3) ON CONFLICT(budget_id) DO NOTHING",params![id,cipher,digest.as_slice()]).map_err(|e|e.to_string())?;
        tx.execute("UPDATE bridge_budget_executions SET execution_state='unknown' WHERE budget_id=?1 AND execution_state='running'",[id]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e|e.to_string())
    }
    pub(super) fn budget_diagnostic(&self,id:&str)->Result<Option<Diagnostic>,String> {
        let record:Option<(Vec<u8>,Vec<u8>)>=self.connection.query_row("SELECT ciphertext,digest FROM bridge_execution_diagnostics WHERE budget_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
        let Some((cipher,digest))=record else {return Ok(None)};
        let value:Value=serde_json::from_slice(&crate::vault::unprotect_blob(&cipher)?).map_err(|_|"diagnostic decoding failed")?;
        if aiwork_core::canonical_json_hash(&value).as_slice()!=digest {return Err("diagnostic digest mismatch".into());}
        let protected:Protected=serde_json::from_value(value).map_err(|_|"diagnostic decoding failed")?;
        if protected.version!=1 || protected.identity!=super::bridge_prepared::sent_execution_identity(&self.connection,id)? {return Err("diagnostic identity mismatch".into());}
        Ok(Some(protected.diagnostic))
    }
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    use super::super::{bridge_prepared::{tests::{fixture,input,cleanup},ConsumeOutcome},bridge_execution::ExecutionState};
    fn send(store:&mut BridgeBillingStore,lease:&BridgeBudgetLease,request:&str)->String {
        let mut input=input();input.request_id=request.into();input.step_kind="assist".into();input.endpoint="chat".into();
        let prepared=store.prepare_budget(lease,&input,None,10).unwrap();
        let ConsumeOutcome::Granted(ctx)=store.consume_budget(lease,&prepared,20).unwrap() else {panic!("expected consumed budget")};
        assert!(store.mark_budget_send_intent(lease,&prepared.authorization.budget_id,&ctx.consume_epoch).unwrap());
        prepared.authorization.budget_id
    }
    #[test]
    fn persisted_unknown_diagnostic_preserves_money_and_accepts_late_result() {
        let (dir,mut store,lease)=fixture();let id=send(&mut store,&lease,"request-assist");
        let before=store.capacity_totals("account").unwrap();
        let diagnostic=Diagnostic::from("chat_stream_read_timeout");
        store.save_budget_diagnostic(&lease,&id,&diagnostic).unwrap();
        assert_eq!(store.budget_execution(&id).unwrap().unwrap().state,ExecutionState::Unknown);
        assert_eq!(store.capacity_totals("account").unwrap(),before);
        assert!(store.latest_budget_receipt_event(&id).unwrap().is_none());
        assert!(store.load_budget_result(&id).unwrap().is_none());
        drop(store);let mut store=BridgeBillingStore::open(&dir).unwrap();
        assert_eq!(store.budget_diagnostic(&id).unwrap(),Some(diagnostic));
        let result=json!({"choices":[{"message":{"content":"done"},"finish_reason":"stop"}]});
        store.finish_budget_result(&lease,&id,ExecutionState::Succeeded,&result,chrono::Utc::now().timestamp_millis()).unwrap();
        assert_eq!(store.load_budget_result(&id).unwrap(),Some(result));
        assert_eq!(store.capacity_totals("account").unwrap().awaiting_receipt,40_000_000);
        cleanup(dir,store,lease);
    }
    #[test]
    fn diagnostic_ciphertext_cannot_be_transplanted_to_another_task() {
        let (dir,mut store,lease)=fixture();
        let a=send(&mut store,&lease,"request-a");let b=send(&mut store,&lease,"request-b");
        store.save_budget_diagnostic(&lease,&a,&Diagnostic::from("chat_transport_failed")).unwrap();
        store.save_budget_diagnostic(&lease,&b,&Diagnostic::from("chat_stream_read_timeout")).unwrap();
        store.connection.execute("UPDATE bridge_execution_diagnostics SET ciphertext=(SELECT ciphertext FROM bridge_execution_diagnostics WHERE budget_id=?1),digest=(SELECT digest FROM bridge_execution_diagnostics WHERE budget_id=?1) WHERE budget_id=?2",params![a,b]).unwrap();
        assert!(store.budget_diagnostic(&b).is_err());
        cleanup(dir,store,lease);
    }
    #[test]
    fn local_transport_explanation_is_not_fabricated_provider_http_error() {
        let detail=Diagnostic::http(502,"连接超时（private.example 10秒内未响应）: token=private");
        assert_eq!(detail.code,"chat_connection_timeout");assert!(!detail.outcome_known);
        assert_eq!(detail.upstream_error,None);
        let detail=Diagnostic::http(429,r#"{"error":{"code":"RATE_LIMIT","message":"Too many requests","token":"private"}}"#);
        assert!(detail.outcome_known);assert_eq!(detail.upstream_error.unwrap().http_status,Some(429));
    }
}
