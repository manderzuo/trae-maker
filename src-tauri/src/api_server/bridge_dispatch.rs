//! Single-send worker boundary; a client connection is only an observer.
use std::sync::Arc;
use serde_json::{json,Value};
use super::{bridge_runtime::BridgeBudgetRuntime,bridge_prepared::{ConsumedBudget,PreparedBudget,ConsumeOutcome},bridge_execution::ExecutionState};

pub(super) enum WorkerResult {Terminal(ExecutionState,Value),Unknown}
pub(super) fn run_consumed(runtime:&BridgeBudgetRuntime,ctx:&ConsumedBudget,send:impl FnOnce(&Value)->WorkerResult)->Result<(),String> {
    let id=&ctx.budget.authorization.budget_id;
    struct UnsentGuard<'a> {runtime:&'a BridgeBudgetRuntime,ctx:&'a ConsumedBudget}
    impl Drop for UnsentGuard<'_> {
        fn drop(&mut self) {
            // Only a successful durable CAS can prove no send. After send intent
            // this fails without releasing anything, including during unwinding.
            let _=self.runtime.with_store(|s,l| {
                let id=&self.ctx.budget.authorization.budget_id;
                s.fail_budget_before_send(l,id,&self.ctx.consume_epoch,chrono::Utc::now().timestamp_millis())?;
                s.confirm_budget_no_send(l,id).map(|_|())
            });
        }
    }
    let _unsent=UnsentGuard {runtime,ctx};
    let mut body=ctx.body.get("upstream").filter(|v|v.is_object()).cloned().ok_or("prepared upstream payload missing")?;
    super::payload::bind_upstream_session_id_value(&mut body,&ctx.session_ref)?;
    if !runtime.with_store(|s,l|s.mark_budget_send_intent(l,id,&ctx.consume_epoch))? {return Ok(());}
    struct SentGuard<'a> {runtime:&'a BridgeBudgetRuntime,id:&'a str}
    impl Drop for SentGuard<'_> {
        fn drop(&mut self) {let _=self.runtime.with_store(|s,l|s.mark_budget_execution_unknown(l,self.id));}
    }
    let _sent=SentGuard {runtime,id};
    match send(&body) {
        WorkerResult::Terminal(status,result)=>runtime.with_store(|s,l|s.finish_budget_result(l,id,status,&result,chrono::Utc::now().timestamp_millis())),
        WorkerResult::Unknown=>runtime.with_store(|s,l|s.mark_budget_execution_unknown(l,id)),
    }
}

pub(super) fn dispatch(state:Arc<super::ApiSharedState>,runtime:Arc<BridgeBudgetRuntime>,prepared:PreparedBudget)->Result<Value,String> {
    let video=prepared.authorization.endpoint=="videos";
    let key=&prepared.authorization.core_key_id;
    let policy=super::api_keys::KeyLimits::default(); // Core enforces its own per-Key admission; this additionally bounds the upstream worker pool.
    let permit=if video {state.limiter.acquire_video_job(key,&policy)} else {state.limiter.acquire_request(key,&policy)}.map_err(|_|"bridge_workers_busy")?;
    let outcome=runtime.with_store(|s,l|s.consume_budget(l,&prepared,chrono::Utc::now().timestamp_millis()))?;
    let ctx=match outcome {
        ConsumeOutcome::Existing(status)=>return Ok(json!({"wire_version":2,"status":status,"budget_id":prepared.authorization.budget_id,"request_id":prepared.authorization.request_id})),
        ConsumeOutcome::Rejected(proof)=>{
            runtime.with_store(|s,l|s.confirm_budget_no_send(l,&proof.budget_id))?;
            return Ok(json!({"wire_version":2,"status":"failed_no_charge","proof":proof}));
        },
        ConsumeOutcome::Granted(ctx)=>ctx,
    };
    let id=ctx.budget.authorization.budget_id.clone();let request_id=ctx.budget.authorization.request_id.clone();
    let account=state.pool.pick_bound_for(&ctx.account_ref,if video {super::pool::ResourceKind::Work} else {super::pool::ResourceKind::General});
    let Some(account)=account else {
        let proof=runtime.with_store(|s,l|s.fail_budget_before_send(l,&id,&ctx.consume_epoch,chrono::Utc::now().timestamp_millis()))?;
        runtime.with_store(|s,l|s.confirm_budget_no_send(l,&id))?;
        return Ok(json!({"wire_version":2,"status":"failed_no_charge","proof":proof}));
    };
    // spawn_blocking survives a dropped HTTP observer. The permit and runtime
    // Arc are held inside the worker until saving its outcome has completed.
    tokio::task::spawn_blocking(move || {
        let worker_state=state.clone();
        let result=run_consumed(&runtime,&ctx,|body| {
            if video {
                match super::video::run_budget_task(worker_state,&runtime,&ctx,account,body.clone(),permit) {
                    Ok(task) if task.status=="completed"=>WorkerResult::Terminal(ExecutionState::Succeeded,serde_json::to_value(task).unwrap_or(Value::Null)),
                    Ok(task) if task.status=="failed"=>WorkerResult::Terminal(ExecutionState::Failed,serde_json::to_value(task).unwrap_or(Value::Null)),
                    _=>WorkerResult::Unknown,
                }
            } else {
                let _permit=permit;
                let bytes=match serde_json::to_vec(body) {Ok(b)=>b,Err(_)=>return WorkerResult::Unknown};
                match super::routes::make_upstream_request(&account.jwt,&account.uid,&account.device_id,&account.machine_id,&bytes) {
                    Ok(reader)=>match super::sse::aggregate(reader,&format!("chatcmpl-{}",ctx.budget.authorization.request_id)) {
                        (Some(result),None)=>WorkerResult::Terminal(ExecutionState::Succeeded,result),
                        _=>WorkerResult::Unknown,
                    },
                    Err(_)=>WorkerResult::Unknown,
                }
            }
        });
        if result.is_err() {eprintln!("[bridge-v2] execution outcome persistence requires recovery");}
        let _=super::usage_refresh::request_refresh(state,&ctx.budget.authorization.request_id);
    });
    Ok(json!({"wire_version":2,"status":"accepted","budget_id":id,"request_id":request_id}))
}

#[cfg(all(test,windows))]
mod tests {
    use super::*;
    #[test]
    fn worker_saves_output_before_observer_and_replay_never_sends_twice() {
        let dir=std::env::temp_dir().join(format!("aiwork-dispatch-{:032x}",rand::random::<u128>()));
        let runtime=BridgeBudgetRuntime::start(&dir).unwrap();
        let ctx=runtime.with_store(|s,l| {
            s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").unwrap();
            s.initialize_capacity(l,&super::super::bridge_budget::CapacitySnapshot {account_ref:"account".into(),snapshot_ref:"fixture".into(),epoch:1,general:100_000_000,work:0,observed_at_ms:1})?;
            let mut input=super::super::bridge_prepared::tests::input();input.step_kind="assist".into();input.endpoint="chat".into();input.model="deepseek-v4-flash".into();
            input.body=json!({"upstream":{"messages":[{"role":"user","content":"hello"}]}});
            let p=s.prepare_budget(l,&input,None,10)?;
            match s.consume_budget(l,&p,20)? {ConsumeOutcome::Granted(ctx)=>Ok(ctx),_=>Err("not consumed".into())}
        }).unwrap();
        let calls=std::sync::atomic::AtomicUsize::new(0);
        run_consumed(&runtime,&ctx,|body| {
            calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
            assert_eq!(body["session_id"],ctx.session_ref);
            assert_eq!(body["messages"][0]["content"],"hello");
            WorkerResult::Terminal(ExecutionState::Succeeded,json!({"choices":[{"message":{"content":"hello"}}]}))
        }).unwrap();
        run_consumed(&runtime,&ctx,|_| {calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);WorkerResult::Unknown}).unwrap();
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst),1);
        runtime.with_store(|s,_| {
            assert_eq!(s.load_budget_result(&ctx.budget.authorization.budget_id)?.unwrap()["choices"][0]["message"]["content"],"hello");
            let totals=s.capacity_totals("account")?;assert_eq!(totals.pending,0);assert_eq!(totals.awaiting_receipt,40_000_000);
            assert!(s.latest_budget_receipt_event(&ctx.budget.authorization.budget_id)?.is_none());Ok(())
        }).unwrap();
        drop(runtime);std::fs::remove_dir_all(dir).unwrap();
    }
}
