//! Authenticated v2 reads: execution delivery never waits for financial finality.
use std::sync::Arc;
use axum::{extract::{Path, Query, State}, http::StatusCode, response::{IntoResponse, Response}, Json};
use serde::Deserialize;
use serde_json::{json, Value};
use super::{ApiSharedState, bridge_billing::BridgeBillingStore, bridge_prepared::stored_budget_authorization};

#[derive(Deserialize)]
pub(crate) struct BudgetQuery { budget_id: String }

/// Authenticated readiness check: no media, budget, upstream I/O or paid work.
pub(super) async fn frame_extractor_health(State(state):State<Arc<ApiSharedState>>)->Response {
    static SLOTS:std::sync::OnceLock<Arc<tokio::sync::Semaphore>>=std::sync::OnceLock::new();
    let slots=SLOTS.get_or_init(||Arc::new(tokio::sync::Semaphore::new(4))).clone();
    let permit=match slots.try_acquire_owned() {Ok(p)=>p,Err(_)=>return frame_error("frame_extractor_busy")};
    match tokio::task::spawn_blocking(move||{let _permit=permit;super::video_frames::configured_extractor(&state.data_dir)}).await {
        Ok(Ok(_))=>(StatusCode::OK,Json(json!({"status":"ready"}))).into_response(),
        Ok(Err(code))=>frame_error(&code),
        Err(_)=>frame_error("frame_extraction_unavailable"),
    }
}
fn frame_error(code:&str)->Response {
    let mut response=if code=="frame_extractor_busy" {error(StatusCode::TOO_MANY_REQUESTS,code)} else {
        error(StatusCode::SERVICE_UNAVAILABLE,super::video_frames::public_tool_error(code).unwrap_or("frame_extraction_unavailable"))
    };
    response.headers_mut().insert("cache-control","no-store".parse().unwrap());response
}

/// Internal bridge authentication is enforced by the router middleware.
/// This non-billable route accepts bytes only; Core verifies asset ownership.
pub(super) async fn reference_last_frame(State(state):State<Arc<ApiSharedState>>,request:axum::extract::Request)->Response {
    static SLOTS:std::sync::OnceLock<Arc<tokio::sync::Semaphore>>=std::sync::OnceLock::new();
    let slots=SLOTS.get_or_init(||Arc::new(tokio::sync::Semaphore::new(4))).clone();
    let permit=match slots.try_acquire_owned() {Ok(p)=>p,Err(_)=>return error(StatusCode::TOO_MANY_REQUESTS,"frame_extractor_busy")};
    let key=request.headers().get("x-aiwork-core-key-id").and_then(|v|v.to_str().ok()).unwrap_or("").to_owned();
    if key.is_empty()||key.len()>128||!key.bytes().all(|b|b.is_ascii_alphanumeric()||matches!(b,b'-'|b'_')) {return error(StatusCode::BAD_REQUEST,"invalid_core_key_id");}
    let bytes=match axum::body::to_bytes(request.into_body(),32*1024*1024).await {Ok(b)=>b,Err(_)=>return error(StatusCode::PAYLOAD_TOO_LARGE,"frame_source_invalid")};
    if bytes.len()<12||&bytes[4..8]!=b"ftyp" {return error(StatusCode::BAD_REQUEST,"frame_source_invalid");}
    let result=tokio::task::spawn_blocking(move||->Result<_,String>{
        let _permit=permit;
        let frame=super::video_frames::extract_uploaded(&state.data_dir,&bytes)?;
        let png=super::video_frames::read_small(&frame.path,8*1024*1024)?;
        Ok((frame,png))
    }).await;
    match result {
        Ok(Ok((frame,png)))=>Response::builder().header("content-type","image/png").header("content-length",png.len()).header("cache-control","private, no-store")
            .header("x-aiwork-core-key-id",key).header("x-aiwork-source-sha256",frame.source_sha256).header("x-aiwork-frame-sha256",frame.frame_sha256)
            .header("x-aiwork-frame-width",frame.width).header("x-aiwork-frame-height",frame.height).header("x-aiwork-frame-timestamp-ms",frame.timestamp_ms)
            .body(axum::body::Body::from(png)).unwrap_or_else(|_|error(StatusCode::SERVICE_UNAVAILABLE,"frame_output_unavailable")),
        Ok(Err(code))=>frame_error(&code),
        _=>error(StatusCode::SERVICE_UNAVAILABLE,"frame_extraction_unavailable"),
    }
}

#[derive(Deserialize)]
pub(super) struct ChunkQuery {budget_id:String,#[serde(default)] after:u64}
pub(super) async fn chunks(State(state):State<Arc<ApiSharedState>>,
    axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,
    Path(request):Path<String>,Query(query):Query<ChunkQuery>)->Response {
    if !super::bridge_billing::valid_request_id(&request) || !super::bridge_billing::valid_request_id(&query.budget_id) {
        return error(StatusCode::BAD_REQUEST,"invalid_request_id");
    }
    let reply=tokio::task::spawn_blocking(move ||->Result<(StatusCode,Value),String> {
        let store=BridgeBillingStore::open(&state.data_dir)?;
        let auth=stored_budget_authorization(&store.connection,&query.budget_id)?;
        if auth.request_id!=request || auth.endpoint!="chat" {return Ok((StatusCode::CONFLICT,json!({"error":{"code":"identity_conflict"}})));}
        let mut value=json!({"wire_version":2,"request_id":request,"budget_id":auth.budget_id,
            "core_key_id":auth.core_key_id,"account_ref":auth.account_ref,"bridge_instance_id":auth.bridge_instance_id});
        drop(store);
        match runtime.chunks.page(&query.budget_id,query.after) {
            Ok(Some(page))=>{
                value["status"]=json!("available");value["chunks"]=json!(page.chunks);value["next"]=json!(page.next);value["finished"]=json!(page.finished);
            },
            Ok(None)=>{value["status"]=json!("unavailable");},
            Err(_)=>return Ok((StatusCode::CONFLICT,json!({"error":{"code":"chat_stream_cursor_unavailable"}}))),
        }
        Ok((StatusCode::OK,value))
    }).await;
    let mut response=match reply {Ok(Ok((status,value)))=>(status,Json(value)).into_response(),_=>error(StatusCode::SERVICE_UNAVAILABLE,"bridge_state_unavailable")};
    response.headers_mut().insert("cache-control","no-store".parse().unwrap());response
}

fn error(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({"error":{"code":code,"type":"bridge_error"}}))).into_response()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryRequest {
    instance_id:String,generation:String,acknowledge_retained_unknowns:bool,
}
pub(super) async fn recovery_status(axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>)->Response {
    match tokio::task::spawn_blocking(move ||runtime.recovery_status()).await {
        Ok(Ok(value))=>Json(value).into_response(),_=>error(StatusCode::SERVICE_UNAVAILABLE,"bridge_recovery_status_unavailable"),
    }
}
pub(super) async fn recover(axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,Json(request):Json<RecoveryRequest>)->Response {
    if request.instance_id.len()>128 || request.generation.len()>128 {return error(StatusCode::BAD_REQUEST,"bridge_recovery_identity_invalid");}
    match tokio::task::spawn_blocking(move ||runtime.recover_local_instance(&request.instance_id,&request.generation,request.acknowledge_retained_unknowns)).await {
        Ok(Ok(value))=>Json(value).into_response(),
        Ok(Err(reason))=>match reason.as_str() {
            "bridge_recovery_confirmation_required"|"bridge_recovery_identity_or_state_changed"|"bridge_recovery_busy"=>error(StatusCode::CONFLICT,&reason),
            _=>error(StatusCode::SERVICE_UNAVAILABLE,"bridge_recovery_failed"),
        },
        _=>error(StatusCode::SERVICE_UNAVAILABLE,"bridge_recovery_failed"),
    }
}

pub(super) async fn prepare(State(state):State<Arc<ApiSharedState>>,
    axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,
    axum::Extension(bridge_key):axum::Extension<super::usage::KeyId>,
    Json(request):Json<super::bridge_planner::PrepareRequest>)->Response {
    let result=tokio::task::spawn_blocking(move || super::bridge_planner::prepare(&runtime,&request,
        &super::bridge_planner::NativePreparationSource(&state,&bridge_key.0),chrono::Utc::now().timestamp_millis())).await;
    match result {
        Ok(Ok(prepared))=>Json(prepared).into_response(),
        Ok(Err(reason))=>{
            let code=match reason.as_str() {
                "budget_policy_unconfigured"|"budget_policy_expired"|"upstream_account_unavailable"|"upstream_capacity_insufficient"|
                "upstream_capacity_unavailable"|"native_estimate_unavailable"|"bridge_recovery_required"=>reason.as_str(),
                "reference_video_budget_metadata_required"|"reference_video_metadata_invalid"|
                "reference_video_format_unsupported"|"reference_asset_type_mismatch"|"reference_asset_unavailable"=>return error(StatusCode::BAD_REQUEST,&reason),
                "budget_preparation_busy"=>return error(StatusCode::CONFLICT,"budget_preparation_busy"),
                "prepared request identity conflict"=>return error(StatusCode::CONFLICT,"budget_identity_conflict"),
                "invalid_budget_business_request"=>return error(StatusCode::BAD_REQUEST,"invalid_budget_business_request"),
                "invalid_chat_image"=>return error(StatusCode::BAD_REQUEST,"invalid_chat_image"),
                "budget_chat_input_too_large"=>return error(StatusCode::PAYLOAD_TOO_LARGE,"budget_chat_input_too_large"),
                _=>"budget_preparation_failed",
            };error(StatusCode::SERVICE_UNAVAILABLE,code)
        },
        _=>error(StatusCode::SERVICE_UNAVAILABLE,"budget_preparation_failed"),
    }
}
pub(super) async fn cancel(axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,
    Json(prepared):Json<super::bridge_prepared::PreparedBudget>)->Response {
    let result=tokio::task::spawn_blocking(move ||runtime.with_store(|store,lease| {
        let proof=store.cancel_budget(lease,&prepared,chrono::Utc::now().timestamp_millis())?;
        store.confirm_budget_no_send(lease,&proof.budget_id)?;
        Ok::<_,String>(json!({"wire_version":2,"status":"canceled","proof":proof}))
    })).await;
    match result {Ok(Ok(value))=>Json(value).into_response(),_=>error(StatusCode::CONFLICT,"budget_cancel_not_proven")}
}

pub(super) async fn dispatch(State(state):State<Arc<ApiSharedState>>,
    axum::Extension(runtime):axum::Extension<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,
    Json(prepared):Json<super::bridge_prepared::PreparedBudget>)->Response {
    // Local consume/crypto is off the Tokio I/O worker. dispatch starts a bounded
    // independent worker; this future never owns the lifetime of paid I/O.
    match tokio::task::spawn_blocking(move ||super::bridge_dispatch::dispatch(state,runtime,prepared)).await {
        Ok(Ok(value))=>(StatusCode::ACCEPTED,Json(value)).into_response(),
        Ok(Err(reason)) if reason=="bridge_workers_busy"=>error(StatusCode::TOO_MANY_REQUESTS,"bridge_workers_busy"),
        _=>error(StatusCode::CONFLICT,"budget_dispatch_not_accepted"),
    }
}

async fn read_request(state: Arc<ApiSharedState>, request: String, budget: String, kind: &'static str) -> Response {
    if !super::bridge_billing::valid_request_id(&request)
        || !super::bridge_billing::valid_request_id(&budget) {
        return error(StatusCode::BAD_REQUEST, "invalid_request_id");
    }
    let result = tokio::task::spawn_blocking(move || -> Result<(StatusCode, Value), String> {
        let store = BridgeBillingStore::open(&state.data_dir)?;
        let exists: bool = store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM bridge_prepared_budgets WHERE budget_id=?1)", [&budget], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if !exists { return Ok((StatusCode::NOT_FOUND, json!({"error":{"code":"budget_not_found"}}))); }
        let auth = stored_budget_authorization(&store.connection, &budget)?;
        if auth.request_id != request {
            return Ok((StatusCode::CONFLICT, json!({"error":{"code":"identity_conflict"}})));
        }
        let mut value = json!({"wire_version":2,"request_id":request,"budget_id":budget,
            "core_key_id":auth.core_key_id,"account_ref":auth.account_ref,"bridge_instance_id":auth.bridge_instance_id});
        let execution = store.budget_execution(&budget)?;
        match kind {
            "execution" => {
                value["status"] = execution.as_ref().map(|e| json!(e.state)).unwrap_or(json!("not_started"));
                value["execution"] = json!(execution);
            }
            "result" => {
                let result = if execution.is_some() { store.load_budget_result(&budget)? } else { None };
                value["status"] = json!(if result.is_some() { "ready" } else { "not_ready" });
                value["result"] = json!(result);
            }
            "billing" => {
                let event = store.latest_budget_receipt_event(&budget)?;
                value["status"] = json!(event.as_ref().map(|e| e.kind.as_str()).unwrap_or("pending"));
                value["receipt"] = json!(event.as_ref().and_then(|e| e.receipt.as_ref()));
                value["event"] = json!(event);
            }
            "refresh" => {
                if execution.is_none() {
                    return Ok((StatusCode::CONFLICT, json!({"error":{"code":"execution_not_started"}})));
                }
                super::usage_refresh::request_refresh(state.clone(), &request)?;
                value["status"] = json!("accepted");
                return Ok((StatusCode::ACCEPTED, value));
            }
            _ => return Err("unsupported internal read".into()),
        }
        Ok((StatusCode::OK, value))
    }).await;
    match result {
        Ok(Ok((status, value))) => (status, Json(value)).into_response(),
        _ => error(StatusCode::SERVICE_UNAVAILABLE, "bridge_state_unavailable"),
    }
}

pub(crate) async fn execution(State(state): State<Arc<ApiSharedState>>, Path(request): Path<String>, Query(query): Query<BudgetQuery>) -> Response {
    read_request(state, request, query.budget_id, "execution").await
}
pub(crate) async fn result(State(state): State<Arc<ApiSharedState>>, Path(request): Path<String>, Query(query): Query<BudgetQuery>) -> Response {
    read_request(state, request, query.budget_id, "result").await
}
pub(super) async fn content(State(state):State<Arc<ApiSharedState>>,Path(request):Path<String>,Query(query):Query<BudgetQuery>)->Response {
    if !super::bridge_billing::valid_request_id(&request) || !super::bridge_billing::valid_request_id(&query.budget_id) {return error(StatusCode::BAD_REQUEST,"invalid_request_id");}
    let opened=tokio::task::spawn_blocking(move ||->Result<_,String> {
        let store=BridgeBillingStore::open(&state.data_dir)?;
        let auth=stored_budget_authorization(&store.connection,&query.budget_id)?;
        if auth.request_id!=request || auth.endpoint!="videos" {return Err("content binding mismatch".into());}
        let execution=store.budget_execution(&query.budget_id)?.ok_or("execution missing")?;
        if execution.budget_id!=auth.budget_id||execution.request_id!=auth.request_id||execution.core_key_id!=auth.core_key_id||execution.account_ref!=auth.account_ref||execution.bridge_instance_id!=auth.bridge_instance_id||execution.step_kind!="video" {return Err("content identity mismatch".into());}
        if execution.state!=super::bridge_execution::ExecutionState::Succeeded || !execution.result_available {return Err("result not ready".into());}
        // Decrypt and verify saved result as well: a damaged result must not
        // authorize downloading an unrelated or unproven file after restart.
        let result=store.load_budget_result(&query.budget_id)?.ok_or("result missing")?;
        let task=execution.task_ref.ok_or("task missing")?;
        if result["id"].as_str()!=Some(task.as_str()) || result["status"]!="completed" {return Err("result task mismatch".into());}
        let permit=state.limiter.acquire_request(&auth.core_key_id,&super::api_keys::KeyLimits::default()).map_err(|_|"download concurrency exceeded")?;
        // Complete identity/result checks before network I/O. File recovery
        // cannot rewrite execution/financial facts or generate another video.
        drop(store);
        let (file,length)=super::bridge_artifacts::open_completed(&state,&auth.account_ref,&task,&result)?;
        Ok((file,length,permit,auth,task))
    }).await;
    let (mut file,length,permit,auth,task)=match opened {
        Ok(Ok(data))=>data,
        Ok(Err(reason)) if matches!(reason.as_str(),"artifact_recovery_busy"|"download concurrency exceeded")=>{
            let mut response=error(StatusCode::TOO_MANY_REQUESTS,"budget_content_busy");response.headers_mut().insert("retry-after","2".parse().unwrap());return response;
        },
        _=>return error(StatusCode::SERVICE_UNAVAILABLE,"budget_content_unavailable"),
    };
    let (send,receive)=tokio::sync::mpsc::channel::<Result<axum::body::Bytes,std::io::Error>>(2);
    tokio::task::spawn_blocking(move || {
        use std::io::Read;let _permit=permit;let mut buffer=[0u8;64*1024];let mut remaining=length;
        while remaining>0 {
            let cap=usize::try_from(remaining.min(buffer.len() as u64)).unwrap();
            match file.read(&mut buffer[..cap]) {
                Ok(0)=>{let _=send.blocking_send(Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof,"video artifact truncated")));break;},
                Ok(n)=>{remaining-=n as u64;if send.blocking_send(Ok(axum::body::Bytes::copy_from_slice(&buffer[..n]))).is_err() {break;}},
                Err(e)=>{let _=send.blocking_send(Err(e));break;},
            }
        }
    });
    Response::builder().header("content-type","video/mp4").header("content-length",length).header("cache-control","private, no-store")
        .header("x-aiwork-request-id",auth.request_id).header("x-aiwork-budget-id",auth.budget_id)
        .header("x-aiwork-core-key-id",auth.core_key_id).header("x-aiwork-account-ref",auth.account_ref)
        .header("x-aiwork-bridge-instance-id",auth.bridge_instance_id).header("x-aiwork-task-ref",task)
        .header("content-disposition","attachment; filename=video.mp4").body(axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(receive)))
        .unwrap_or_else(|_|error(StatusCode::INTERNAL_SERVER_ERROR,"content_response_failed"))
}
pub(crate) async fn billing(State(state): State<Arc<ApiSharedState>>, Path(request): Path<String>, Query(query): Query<BudgetQuery>) -> Response {
    read_request(state, request, query.budget_id, "billing").await
}
pub(super) async fn last_frame(State(state):State<Arc<ApiSharedState>>,Path(request):Path<String>,Query(query):Query<BudgetQuery>)->Response {
    if !super::bridge_billing::valid_request_id(&request)||!super::bridge_billing::valid_request_id(&query.budget_id){return error(StatusCode::BAD_REQUEST,"invalid_request_id");}
    let reply=tokio::task::spawn_blocking(move||->Result<_,String>{
        let store=BridgeBillingStore::open(&state.data_dir)?;
        let auth=stored_budget_authorization(&store.connection,&query.budget_id)?;
        if auth.request_id!=request||auth.endpoint!="videos"{return Err("identity_conflict".into());}
        let execution=store.budget_execution(&query.budget_id)?.ok_or("frame_result_not_ready")?;
        if execution.budget_id!=auth.budget_id||execution.request_id!=auth.request_id||execution.core_key_id!=auth.core_key_id||execution.account_ref!=auth.account_ref||execution.bridge_instance_id!=auth.bridge_instance_id||execution.step_kind!="video"{return Err("identity_conflict".into());}
        if execution.state!=super::bridge_execution::ExecutionState::Succeeded||!execution.result_available{return Err("frame_result_not_ready".into());}
        let result=store.load_budget_result(&query.budget_id)?.ok_or("frame_result_not_ready")?;
        let task=execution.task_ref.ok_or("frame_result_not_ready")?;
        if result["id"].as_str()!=Some(task.as_str())||result["status"]!="completed"{return Err("identity_conflict".into());}
        drop(store);
        let extractor=super::video_frames::configured_extractor(&state.data_dir)?;
        let (_file,_length)=super::bridge_artifacts::open_completed(&state,&auth.account_ref,&task,&result)?;
        let path=super::video_store::artifact_path(&state.data_dir,&task)?;
        let frame=super::video_frames::extract_last(&state.data_dir,&path,&extractor)?;
        let bytes=super::video_frames::read_small(&frame.path,8*1024*1024)?;
        Ok((frame,bytes,auth))
    }).await;
    match reply {
        Ok(Ok((frame,bytes,auth)))=>Response::builder().header("content-type","image/png").header("content-length",bytes.len()).header("cache-control","private, no-store")
            .header("x-aiwork-frame-width",frame.width).header("x-aiwork-frame-height",frame.height).header("x-aiwork-frame-timestamp-ms",frame.timestamp_ms)
            .header("x-aiwork-source-sha256",frame.source_sha256).header("x-aiwork-frame-sha256",frame.frame_sha256)
            .header("x-aiwork-request-id",auth.request_id).header("x-aiwork-budget-id",auth.budget_id).header("x-aiwork-core-key-id",auth.core_key_id)
            .header("x-aiwork-account-ref",auth.account_ref).header("x-aiwork-bridge-instance-id",auth.bridge_instance_id)
            .body(axum::body::Body::from(bytes)).unwrap_or_else(|_|error(StatusCode::SERVICE_UNAVAILABLE,"frame_output_unavailable")),
        Ok(Err(code)) if matches!(code.as_str(),"identity_conflict"|"frame_result_not_ready")=>error(StatusCode::CONFLICT,&code),
        Ok(Err(code)) if matches!(code.as_str(),"frame_extractor_busy"|"artifact_recovery_busy")=>{let mut r=error(StatusCode::TOO_MANY_REQUESTS,"frame_extractor_busy");r.headers_mut().insert("retry-after","2".parse().unwrap());r},
        Ok(Err(code))=>frame_error(&code),
        _=>error(StatusCode::SERVICE_UNAVAILABLE,"frame_extraction_unavailable"),
    }
}
pub(crate) async fn refresh(State(state): State<Arc<ApiSharedState>>, Path(request): Path<String>, Query(query): Query<BudgetQuery>) -> Response {
    read_request(state, request, query.budget_id, "refresh").await
}

#[derive(Deserialize)]
pub(crate) struct EventQuery { generation: String, #[serde(default)] after: i64, limit: usize }
pub(crate) async fn events(State(state): State<Arc<ApiSharedState>>, Query(query): Query<EventQuery>) -> Response {
    if query.after < 0 || !(1..=256).contains(&query.limit) || query.generation.len() > 256 {
        return error(StatusCode::BAD_REQUEST, "invalid_event_page");
    }
    let result = tokio::task::spawn_blocking(move || -> Result<(StatusCode, Value), String> {
        let store = BridgeBillingStore::open(&state.data_dir)?;
        let (instance, generation) = store.bridge_identity()?;
        if !query.generation.is_empty() && generation != query.generation {
            return Ok((StatusCode::CONFLICT, json!({"error":{"code":"generation_changed"},
                "wire_version":2,"bridge_instance_id":instance,"generation":generation})));
        }
        let events = store.budget_receipt_events(&generation, query.after, query.limit)?;
        Ok((StatusCode::OK, json!({"wire_version":2,"bridge_instance_id":instance,"generation":generation,"events":events})))
    }).await;
    match result {
        Ok(Ok((status, value))) => (status, Json(value)).into_response(),
        _ => error(StatusCode::SERVICE_UNAVAILABLE, "bridge_state_unavailable"),
    }
}
