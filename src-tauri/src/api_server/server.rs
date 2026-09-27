use std::sync::Arc;

use axum::middleware::from_fn_with_state;
use axum::routing::{get, post, put};
use axum::extract::DefaultBodyLimit;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::auth;
#[path = "core_account.rs"]
mod core_account;
use super::routes;
use super::wb_catalog;
use super::ApiSharedState;

/// API 服务器句柄：用于优雅停止
pub struct ApiServerHandle {
    shutdown_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<JoinHandle<()>>,
    background_stop: Arc<std::sync::atomic::AtomicBool>,
    budget_runtime: Option<Arc<super::bridge_runtime::BridgeBudgetRuntime>>,
}

impl ApiServerHandle {
    /// 发送 shutdown 信号并等待优雅退出（最多 3s，每 50ms 轮询一次），
    /// 超时才 abort（P2 修复9：原实现 send 后立即 abort，优雅停机被自身取消，
    /// 在途请求被硬断）
    pub fn stop(&mut self) {
        if let Some(runtime) = &self.budget_runtime { runtime.begin_close(); }
        self.background_stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            if !wait_task_finished(&h, std::time::Duration::from_secs(3)) {
                h.abort();
            }
        }
        self.budget_runtime.take();
    }
}

/// 轮询任务是否已结束（每 50ms 一次，最多 max）；P2 修复9 优雅停机用
fn wait_task_finished(h: &JoinHandle<()>, max: std::time::Duration) -> bool {
    let deadline = std::time::Instant::now() + max;
    loop {
        if h.is_finished() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

impl Drop for ApiServerHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动 axum HTTP 服务器
///
/// 使用 Tauri 内置 tokio runtime，不新建 runtime。
pub async fn start_api_server(
    listen_host: &str,
    port: u16,
    state: Arc<ApiSharedState>,
) -> Result<ApiServerHandle, String> {
    let addr = format!("{}:{}", listen_host.trim(), port);
    let listener = TcpListener::bind(&addr)
        .await
        .map_err(|e| format!("端口 {} 绑定失败: {}", port, e))?;

    let runtime_dir = state.data_dir.clone();
    let budget_runtime = tokio::task::spawn_blocking(move || super::bridge_runtime::BridgeBudgetRuntime::start(&runtime_dir))
        .await.map_err(|_| "bridge runtime initialization failed".to_string())??;
    let app = build_router(state.clone()).layer(axum::Extension(budget_runtime.clone()));
    spawn_wb_health_probe(state.clone());
    let background_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    super::bridge_runtime::BridgeBudgetRuntime::start_maintenance(&budget_runtime,background_stop.clone());
    super::usage_refresh::start_with_runtime(state.clone(), background_stop.clone(), Some(budget_runtime.clone()));
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = shutdown_rx.await;
    });

    let join_handle = tokio::spawn(async move {
        if let Err(e) = server.await {
            eprintln!("API server error: {}", e);
        }
    });

    Ok(ApiServerHandle {
        shutdown_tx: Some(shutdown_tx),
        join_handle: Some(join_handle),
        background_stop,
        budget_runtime: Some(budget_runtime),
    })
}

fn build_router(state: Arc<ApiSharedState>) -> Router {
    Router::new()
        .route("/health", get(routes::health))
        .route("/healthz", get(routes::healthz))
        .route("/status", get(routes::status))
        .route("/v1/models", get(routes::models))
        .route("/v1/usage", get(core_account::usage))
        .route("/v1/chat/completions", post(routes::chat_completions_with_attribution))
        .route("/v1/completions", post(routes::completions))
        .route("/v1/embeddings", post(routes::embeddings))
        .route("/v1/messages", post(routes::messages))
        .route("/v1/responses", post(routes::responses_api))
        // T5.4/F-63 生图双端点投影
        .route("/v1/images/generations", post(routes::images_generations))
        .route("/v1/images/edits", post(routes::images_edits))
        // 参考图/参考视频素材暂存；Base64 编码后约膨胀 4/3，单请求限制
        // 略高于模块 32 MiB 素材上限。
        .route(
            "/v1/assets",
            post(routes::assets_upload).layer(DefaultBodyLimit::max(46 * 1024 * 1024)),
        )
        .route("/v1/assets/:asset_id/content", get(routes::assets_content))
        .route("/internal/bridge/status", get(super::bridge_api::status))
        .route("/internal/bridge/models", get(super::bridge_api::models))
        .route("/internal/bridge/summary", get(super::bridge_api::summary))
        .route("/internal/bridge/quotes", post(super::bridge_api::quote))
        .route("/internal/bridge/v2/requests/:request_id/execution", get(super::bridge_v2_api::execution))
        .route("/internal/bridge/v2/requests/:request_id/result", get(super::bridge_v2_api::result))
        .route("/internal/bridge/v2/requests/:request_id/content", get(super::bridge_v2_api::content))
        .route("/internal/bridge/v2/requests/:request_id/billing", get(super::bridge_v2_api::billing))
        .route("/internal/bridge/v2/requests/:request_id/refresh", post(super::bridge_v2_api::refresh))
        .route("/internal/bridge/v2/receipt-events", get(super::bridge_v2_api::events))
        .route("/internal/bridge/v2/recovery", get(super::bridge_v2_api::recovery_status).post(super::bridge_v2_api::recover))
        .route("/internal/bridge/v2/budgets/prepare", post(super::bridge_v2_api::prepare))
        .route("/internal/bridge/v2/budgets/cancel", post(super::bridge_v2_api::cancel))
        .route("/internal/bridge/v2/budgets/dispatch", post(super::bridge_v2_api::dispatch))
        .route("/internal/bridge/key-registry", put(super::bridge_api::replace_core_key_registry).layer(DefaultBodyLimit::max(8 * 1024 * 1024)))
        .route("/internal/bridge/key-usage", get(super::bridge_api::core_key_usage))
        .route(
            "/internal/bridge/requests/:request_id/billing",
            get(super::bridge_api::billing),
        )
        .route(
            "/internal/bridge/requests/:request_id/video-task",
            get(super::bridge_api::controlled_video_task),
        )
        .route(
            "/internal/bridge/requests/:request_id/billing/finalize",
            post(super::bridge_api::finalize_video_billing),
        )
        .route(
            "/internal/bridge/requests/:request_id/billing/finalize-chat",
            post(super::bridge_api::finalize_chat_billing),
        )
        // W-02 Seedance 视频接口契约：异步 Work 额度任务桥，原生插件仍可降级。
        .route("/v1/videos/generations", post(routes::videos_generations_with_attribution))
        .route("/v1/videos/:task_id", get(routes::video_task))
        .route("/v1/videos/:task_id/cancel", post(routes::video_cancel))
        .route("/v1/videos/:task_id/content", get(routes::video_content))
        .layer(from_fn_with_state(state.clone(), super::cors::headers))
        .layer(from_fn_with_state(state.clone(), auth::bearer_auth))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use axum::{body::Body, http::{Request, StatusCode}};
    use tower::ServiceExt;

    use super::*;

    struct BridgeFixture {
        app: Option<Router>,
        key: String,
        dir: PathBuf,
        state: Arc<ApiSharedState>,
    }

    impl BridgeFixture {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aiwork-bridge-routes-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let core_store = aiwork_core::CoreStore::open(&dir).unwrap();
            core_store.migrate().unwrap();
            drop(core_store);
            let issued = super::super::api_keys::issue_bridge_key(&dir, "route test").unwrap();

            let state = Arc::new(ApiSharedState {
                core: None,
                pool: super::super::pool::ApiPool::new(),
                wb_pool: super::super::pool::ApiPool::new(),
                wb_enabled: std::sync::atomic::AtomicBool::new(false),
                wb_sanitize: std::sync::atomic::AtomicBool::new(true),
                wb_default_thinking: std::sync::atomic::AtomicBool::new(false),
                wb_tool_exec: std::sync::atomic::AtomicBool::new(false),
                wb_bg_downgrade: std::sync::atomic::AtomicBool::new(false),
                wb_sticky: super::super::wb_sticky::StickyStore::default(),
                pool_sticky: std::sync::Mutex::new(std::collections::HashMap::new()),
                model_cooldowns: std::sync::Mutex::new(std::collections::HashMap::new()),
                default_model: "test-model".into(),
                data_dir: dir.clone(),
                video_payloads: super::super::video_payload::VideoPayloadStore::new(&dir),
                cors_origins: String::new(),
                total_requests: std::sync::atomic::AtomicU64::new(0),
                inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
                limiter: super::super::limits::RateLimiter::with_config(
                    super::super::limits::LimitConfig {
                        max_inflight: 4,
                        max_video_jobs: 2,
                        asset_uploads_per_minute: 10,
                        asset_bytes_per_hour: 1024,
                        video_submissions_per_minute: 10,
                    },
                ),
                active_uid: std::sync::Mutex::new(None),
                last_error: std::sync::Mutex::new(None),
                logger: super::super::ApiLogger::new(dir.join("logs")),
                debug_enabled: std::sync::atomic::AtomicBool::new(false),
                usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
                wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
                wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
            });
            Self {
                app: Some(build_router(state.clone())),
                key: issued.plaintext,
                dir,
                state,
            }
        }

        fn take_app(&mut self) -> Router {
            self.app.take().unwrap()
        }
    }

    impl Drop for BridgeFixture {
        fn drop(&mut self) {
            self.app.take();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // ==================== P2 修复9：优雅停机轮询 ====================

    #[cfg(windows)]
    #[tokio::test]
    async fn v2_read_routes_deliver_result_before_receipt_without_exposing_dispatch_token() {
        use super::super::{bridge_billing::BridgeBillingStore, bridge_budget::CapacitySnapshot,
            bridge_budget_lease::BridgeBudgetLease, bridge_prepared::{tests::input, ConsumeOutcome},
            bridge_execution::ExecutionState};
        let mut fixture = BridgeFixture::new();
        let mut store = BridgeBillingStore::open(&fixture.dir).unwrap();
        store.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").unwrap();
        let lease = BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();
        store.initialize_capacity(&lease, &CapacitySnapshot { account_ref: "account".into(),
            snapshot_ref: "test-only".into(), epoch: 1, general: 100_000_000,
            work: 100_000_000, observed_at_ms: 1 }).unwrap();
        let prepared = store.prepare_budget(&lease, &input(), None, 10).unwrap();
        let ConsumeOutcome::Granted(ctx) = store.consume_budget(&lease, &prepared, 20).unwrap() else { panic!("consume") };
        let budget_id = &prepared.authorization.budget_id;
        store.mark_budget_send_intent(&lease, budget_id, &ctx.consume_epoch).unwrap();
        store.bind_budget_task(&lease, budget_id, "video-test").unwrap();
        let app = fixture.take_app();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (sent, received) = std::sync::mpsc::channel();
        let scheduler = super::super::usage_refresh::start_with(fixture.state.clone(), stop.clone(),
            || Ok(Vec::new()), move |_| { sent.send(()).unwrap(); true });
        super::super::usage_refresh::request_refresh(fixture.state.clone(), "request-video").unwrap();
        received.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        store.finish_budget_result(&lease, budget_id, ExecutionState::Succeeded,
            &serde_json::json!({"id":"video-test","status":"completed"}), chrono::Utc::now().timestamp_millis()).unwrap();
        let refresh_path = format!("/internal/bridge/v2/requests/request-video/refresh?budget_id={budget_id}");
        let response = app.clone().oneshot(Request::builder().method("POST").uri(&refresh_path)
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        let refresh_status = response.status();
        let terminal_woke = received.recv_timeout(std::time::Duration::from_secs(1)).is_ok();
        let repeated = app.clone().oneshot(Request::builder().method("POST").uri(&refresh_path)
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        let duplicate_woke = received.recv_timeout(std::time::Duration::from_millis(100)).is_ok();
        stop.store(true, std::sync::atomic::Ordering::Release);
        scheduler.join().unwrap();
        assert_eq!(refresh_status, StatusCode::ACCEPTED);
        assert_eq!(repeated.status(), StatusCode::ACCEPTED);
        assert!(terminal_woke, "durable execution completion must reset the pre-completion backoff immediately");
        assert!(!duplicate_woke, "repeated observers must not restart the backoff");
        for (route, expected) in [("execution", "succeeded"), ("billing", "pending"), ("result", "ready")] {
            let path = format!("/internal/bridge/v2/requests/request-video/{route}?budget_id={budget_id}");
            let response = app.clone().oneshot(Request::builder().uri(&path)
                .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            let bytes = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["status"], expected);
            assert_eq!(value["core_key_id"], "key-a");
            assert_eq!(value["budget_id"], budget_id.as_str());
            assert!(!String::from_utf8_lossy(&bytes).contains(&prepared.dispatch_token));
            if route == "billing" { assert!(value["receipt"].is_null()); }
            let rejected = app.clone().oneshot(Request::builder().uri(&path).body(Body::empty()).unwrap()).await.unwrap();
            assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        }
        let wrong = format!("/internal/bridge/v2/requests/request-other/billing?budget_id={budget_id}");
        let response = app.clone().oneshot(Request::builder().uri(wrong)
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        super::super::bridge_receipts::tests::cache(&fixture.dir, &store, &[(budget_id, "12.345678")], 2000);
        store.confirm_budget_usage(&lease, budget_id).unwrap();
        let response = app.clone().oneshot(Request::builder()
            .uri(format!("/internal/bridge/v2/requests/request-video/billing?budget_id={budget_id}"))
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), 65536).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["status"], "final");
        assert_eq!(value["receipt"]["actual_credits"], "12.345678");
        assert_eq!(value["event"]["receipt"], value["receipt"]);
        let response = app.clone().oneshot(Request::builder()
            .uri("/internal/bridge/v2/receipt-events?generation=old&after=0&limit=10")
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        // A broken result must not be converted into an empty success or resubmitted.
        let discovery=app.clone().oneshot(Request::builder()
            .uri("/internal/bridge/v2/receipt-events?generation=&after=0&limit=10")
            .header("authorization",format!("Bearer {}",fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(discovery.status(),StatusCode::OK,"Core must discover the current event generation before recovering its durable cursor");
        let value:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(discovery.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(value["events"][0]["receipt"]["actual_credits"],"12.345678");
        let artifact=super::super::video_store::artifact_path(&fixture.dir,"video-test").unwrap();
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();std::fs::write(&artifact,b"fixture-mp4").unwrap();
        let download=app.clone().oneshot(Request::builder().uri(format!("/internal/bridge/v2/requests/request-video/content?budget_id={budget_id}"))
            .header("authorization",format!("Bearer {}",fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(download.status(),StatusCode::OK);
        assert_eq!(&axum::body::to_bytes(download.into_body(),65536).await.unwrap()[..],b"fixture-mp4");
        store.connection.execute("UPDATE bridge_budget_executions SET result_ciphertext=x'00' WHERE budget_id=?1", [budget_id]).unwrap();
        let response = app.oneshot(Request::builder()
            .uri(format!("/internal/bridge/v2/requests/request-video/result?budget_id={budget_id}"))
            .header("authorization", format!("Bearer {}", fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        drop(lease);
        drop(store);
    }

    #[test]
    fn wait_task_finished_detects_completion_and_timeout() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        // 已完成任务：立即判定结束
        let done = rt.spawn(async {});
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(wait_task_finished(&done, std::time::Duration::from_secs(1)));
        // 长任务：达到 max 轮询上限判定未结束（不再立即 abort）
        let slow = rt.spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        });
        let started = std::time::Instant::now();
        assert!(!wait_task_finished(&slow, std::time::Duration::from_millis(150)));
        assert!(started.elapsed() >= std::time::Duration::from_millis(150));
        slow.abort();
    }

    #[test]
    fn router_registers_the_authenticated_user_usage_route() {
        let source = include_str!("server.rs");
        assert!(source.contains(".route(\"/v1/usage\", get(core_account::usage))"));
    }

    #[tokio::test]
    async fn quote_endpoint_returns_unavailable_instead_of_an_estimated_ceiling() {
        let mut fixture = BridgeFixture::new();
        let request = Request::builder()
            .method("POST")
            .uri("/internal/bridge/quotes")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"request_id":"request-quote-1","endpoint":"/v1/chat/completions","model":"seedance","request_fingerprint":"sha256:abc"}"#,
            ))
            .unwrap();
        let response = fixture.take_app().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["request_id"], "request-quote-1");
        assert_eq!(body["status"], "unavailable");
        assert_eq!(body["error_code"], "quote_unavailable");
        assert!(body.get("max_credits").is_none());
    }

    #[tokio::test]
    async fn absent_billing_receipt_is_unknown_not_zero() {
        let mut fixture = BridgeFixture::new();
        let request = Request::builder()
            .uri("/internal/bridge/requests/request-unknown/billing")
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();
        let response = fixture.take_app().oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["request_id"], "request-unknown");
        assert_eq!(body["status"], "unknown");
        assert!(body["actual_credits"].is_null());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn v2_prepare_replays_before_io_and_cancel_proves_no_send() {
        use super::super::{bridge_runtime::BridgeBudgetRuntime,bridge_budget::CapacitySnapshot,bridge_prepared::tests::input};
        let mut fixture=BridgeFixture::new();
        let runtime=BridgeBudgetRuntime::start(&fixture.dir).unwrap();
        let claim=serde_json::json!({"wire_version":2,"parent_request_id":"request-video","request_id":"request-video","core_key_id":"key-a",
            "request_fingerprint":"core-fingerprint","endpoint":"videos","model":"seedance","step_kind":"video","body":{"prompt":"cat"}});
        let prepared=runtime.with_store(|s,l| {
            s.connection.execute_batch("INSERT INTO bridge_core_api_keys VALUES ('key-a','A',1,1)").unwrap();
            s.initialize_capacity(l,&CapacitySnapshot {account_ref:"account".into(),snapshot_ref:"fixture".into(),epoch:1,general:100_000_000,work:100_000_000,observed_at_ms:1})?;
            let mut i=input();i.parent_request_id="request-video".into();i.body=serde_json::json!({"business_claim":claim,"upstream":{"prompt":"cat"}});
            s.prepare_budget(l,&i,None,10)
        }).unwrap();
        let app=fixture.take_app().layer(axum::Extension(runtime.clone()));
        let response=app.clone().oneshot(Request::builder().method("POST").uri("/internal/bridge/v2/budgets/prepare")
            .header("authorization",format!("Bearer {}",fixture.key)).header("content-type","application/json")
            .body(Body::from(claim.to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::OK,"persisted replay must work even with an empty live account pool");
        let value:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(response.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(value["dispatch_token"],prepared.dispatch_token);
        let mut forged=claim.clone();forged["hold_microcredits"]=serde_json::json!(1);
        let rejected=app.clone().oneshot(Request::builder().method("POST").uri("/internal/bridge/v2/budgets/prepare")
            .header("authorization",format!("Bearer {}",fixture.key)).header("content-type","application/json").body(Body::from(forged.to_string())).unwrap()).await.unwrap();
        assert_eq!(rejected.status(),StatusCode::UNPROCESSABLE_ENTITY);
        let response=app.clone().oneshot(Request::builder().method("POST").uri("/internal/bridge/v2/budgets/cancel")
            .header("authorization",format!("Bearer {}",fixture.key)).header("content-type","application/json")
            .body(Body::from(value.to_string())).unwrap()).await.unwrap();
        assert_eq!(response.status(),StatusCode::OK);
        runtime.with_store(|s,_| {
            assert_eq!(s.capacity_totals("account")?.pending,0);
            assert_eq!(s.latest_budget_receipt_event(&prepared.authorization.budget_id)?.unwrap().kind,"failed_no_charge");Ok(())
        }).unwrap();
        let denied=app.clone().oneshot(Request::builder().method("POST").uri("/internal/bridge/v2/budgets/prepare")
            .header("content-type","application/json").body(Body::from(claim.to_string())).unwrap()).await.unwrap();
        assert_eq!(denied.status(),StatusCode::FORBIDDEN);
        drop(app);drop(runtime);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn v2_recovery_requires_bridge_auth_identity_and_explicit_confirmation() {
        use super::super::{bridge_runtime::BridgeBudgetRuntime,bridge_billing::BridgeBillingStore,bridge_budget_lease::BridgeBudgetLease};
        let mut fixture=BridgeFixture::new();
        let store=BridgeBillingStore::open(&fixture.dir).unwrap();
        let lease=BridgeBudgetLease::try_acquire(&store).unwrap().unwrap();drop(lease);drop(store);
        let runtime=BridgeBudgetRuntime::start(&fixture.dir).unwrap();
        let app=fixture.take_app().layer(axum::Extension(runtime.clone()));
        let path="/internal/bridge/v2/recovery";
        let denied=app.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(denied.status(),StatusCode::FORBIDDEN);
        let status=app.clone().oneshot(Request::builder().uri(path).header("authorization",format!("Bearer {}",fixture.key)).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(status.status(),StatusCode::OK);
        let value:serde_json::Value=serde_json::from_slice(&axum::body::to_bytes(status.into_body(),65536).await.unwrap()).unwrap();
        assert_eq!(value["recovery_required"],true);
        let mut body=serde_json::json!({"instance_id":value["instance_id"],"generation":value["generation"],"acknowledge_retained_unknowns":false});
        let request=|value:&serde_json::Value,key:&str| Request::builder().method("POST").uri(path).header("authorization",format!("Bearer {key}"))
            .header("content-type","application/json").body(Body::from(value.to_string())).unwrap();
        let denied=app.clone().oneshot(request(&body,&fixture.key)).await.unwrap();assert_eq!(denied.status(),StatusCode::CONFLICT);
        body["acknowledge_retained_unknowns"]=serde_json::json!(true);
        let user_store=aiwork_core::CoreStore::open(&fixture.dir).unwrap();
        user_store.create_user(aiwork_core::NewUser {id:"regular-recovery-user".into(),name:"Regular".into(),role:aiwork_core::UserRole::User},"bootstrap").unwrap();
        let regular=user_store.issue_api_key("regular-recovery-user","regular",std::collections::BTreeSet::from(["models:read".into()]),"bootstrap").unwrap();
        let denied=app.clone().oneshot(request(&body,&regular.plaintext)).await.unwrap();assert_eq!(denied.status(),StatusCode::FORBIDDEN);
        let activated=app.clone().oneshot(request(&body,&fixture.key)).await.unwrap();assert_eq!(activated.status(),StatusCode::OK);
        assert!(runtime.recovery_status().unwrap().charge_ready);
        let stale=app.clone().oneshot(request(&body,&fixture.key)).await.unwrap();assert_eq!(stale.status(),StatusCode::CONFLICT);
        drop(user_store);drop(app);drop(runtime);
    }

    #[tokio::test]
    async fn controlled_video_recovery_endpoint_requires_bridge_auth_and_registered_request() {
        let mut fixture = BridgeFixture::new();
        let mut store = super::super::bridge_billing::BridgeBillingStore::open(&fixture.dir).unwrap();
        store.record_core_request_with_billing_mode("req-recovery", "key-user", super::super::bridge_billing::CoreBillingMode::ControlledUnquoted, Some("op-recovery")).unwrap();
        drop(store);
        let path = "/internal/bridge/requests/req-recovery/video-task";
        let unauthenticated = fixture.app.as_ref().unwrap().clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(unauthenticated.status(), StatusCode::FORBIDDEN);
        let response = fixture.take_app().oneshot(Request::builder().uri(path)
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["request_id"], "req-recovery");
        assert!(value["task"].is_null());
    }

    #[tokio::test]
    async fn regular_chat_finalization_records_only_its_exact_session_credits() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-chat-regular";
        let account_ref = "uid-chat";
        let session_id = "session-chat-regular";
        super::super::bridge_billing::persist_core_request_attribution(
            &fixture.dir, request_id, "key_chat",
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, session_id,
        ).unwrap();
        let now = chrono::Utc::now().timestamp();
        let usage_cache = serde_json::json!({
            "fetched_at": now,
            "accounts": {
                account_ref: {
                    "name":"测试账号", "last_fetch_end_ts":now, "daily":{},
                    "session_usage": {
                        session_id: {
                            "session_id":session_id, "usage_time":now, "date":"2026-09-25",
                            "model_name":"DeepSeek", "credits_float":"0.050400",
                            "ambiguous":false, "core_request_id":request_id,
                            "core_key_id":"key_chat", "core_attribution_ambiguous":false
                        }
                    }
                }
            }
        });
        let cache_path = fixture.dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&usage_cache).unwrap()).unwrap();
        let app = fixture.take_app();
        let finalize = || Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize-chat"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();
        for _ in 0..2 {
            let response = app.clone().oneshot(finalize()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(receipt["request_id"], request_id);
            assert_eq!(receipt["status"], "final");
            assert_eq!(receipt["actual_credits"], "0.050400");
            assert_eq!(receipt["source_ref"], "trae-usage-session:session-chat-regular");
            assert!(receipt["task_ref"].is_null());
        }
    }

    #[tokio::test]
    async fn regular_video_finalization_records_only_its_exact_session_credits() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-video-regular";
        let account_ref = "uid-video";
        let session_id = "session-video-regular";
        super::super::bridge_billing::persist_core_request_attribution(
            &fixture.dir, request_id, "key_video",
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, session_id,
        ).unwrap();
        let now = chrono::Utc::now().timestamp();
        let usage_cache = serde_json::json!({
            "fetched_at": now,
            "accounts": {
                account_ref: {
                    "name":"测试账号", "last_fetch_end_ts":now, "daily":{},
                    "session_usage": {
                        session_id: {
                            "session_id":session_id, "usage_time":now, "date":"2026-09-25",
                            "model_name":"Seedance", "credits_float":"3.250000",
                            "ambiguous":false, "core_request_id":request_id,
                            "core_key_id":"key_video", "core_attribution_ambiguous":false
                        }
                    }
                }
            }
        });
        let cache_path = fixture.dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&usage_cache).unwrap()).unwrap();
        let app = fixture.take_app();
        let finalize = || Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_ref":"video-task-regular"}"#))
            .unwrap();
        for _ in 0..2 {
            let response = app.clone().oneshot(finalize()).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(receipt["request_id"], request_id);
            assert_eq!(receipt["status"], "final");
            assert_eq!(receipt["actual_credits"], "3.250000");
            assert_eq!(receipt["source_ref"], "trae-usage-session:session-video-regular");
            assert_eq!(receipt["task_ref"], "video-task-regular");
        }
    }

    #[tokio::test]
    async fn regular_video_finalization_keeps_ambiguous_session_usage_unknown() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-video-ambiguous";
        let account_ref = "uid-video";
        let session_id = "session-first";
        super::super::bridge_billing::persist_core_request_attribution(
            &fixture.dir, request_id, "key_video",
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, session_id,
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, "session-second",
        ).unwrap();
        let now = chrono::Utc::now().timestamp();
        let usage_cache = serde_json::json!({
            "fetched_at": now,
            "accounts": {
                account_ref: {
                    "name":"测试账号", "last_fetch_end_ts":now, "daily":{},
                    "session_usage": {
                        session_id: {
                            "session_id":session_id, "usage_time":now, "date":"2026-09-25",
                            "model_name":"Seedance", "credits_float":"3.250000",
                            "ambiguous":false, "core_request_id":request_id,
                            "core_key_id":"key_video", "core_attribution_ambiguous":false
                        }
                    }
                }
            }
        });
        let cache_path = fixture.dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&usage_cache).unwrap()).unwrap();
        let request = Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_ref":"video-task-ambiguous"}"#))
            .unwrap();
        let response = fixture.take_app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt["status"], "unknown");
        assert!(receipt["actual_credits"].is_null());
    }

    #[tokio::test]
    async fn one_shot_video_finalization_waits_for_then_records_exact_session_credits() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-one-shot-test";
        let account_ref = "uid-a";
        let session_id = "session-a";
        super::super::bridge_billing::persist_core_request_attribution_with_mode(
            &fixture.dir, request_id, "key_test_a", true,
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, session_id,
        ).unwrap();
        let app = fixture.take_app();
        let finalize = || Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_ref":"video-task-a"}"#))
            .unwrap();

        let response = app.clone().oneshot(finalize()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(unknown["status"], "unknown");
        assert!(unknown["actual_credits"].is_null());

        let now = chrono::Utc::now().timestamp();
        let usage_cache = serde_json::json!({
            "fetched_at": now,
            "accounts": {
                account_ref: {
                    "name":"测试账号",
                    "last_fetch_end_ts":now,
                    "daily":{},
                    "session_usage": {
                        session_id: {
                            "session_id":session_id,
                            "usage_time":now,
                            "date":"2026-09-24",
                            "model_name":"Seedance",
                            "credits_float":"3.250000",
                            "ambiguous":false,
                            "core_request_id":request_id,
                            "core_key_id":"key_test_a",
                            "core_attribution_ambiguous":false
                        }
                    }
                }
            }
        });
        let cache_path = fixture.dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&usage_cache).unwrap()).unwrap();

        let response = app.oneshot(finalize()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt["request_id"], request_id);
        assert_eq!(receipt["status"], "final");
        assert_eq!(receipt["actual_credits"], "3.250000");
        assert_eq!(receipt["unit"], "credits");
        assert_eq!(receipt["source_ref"], "trae-usage-session:session-a");
        assert_eq!(receipt["task_ref"], "video-task-a");
    }

    #[tokio::test]
    async fn one_shot_chat_finalization_settles_only_its_unique_session_usage() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-chat-one-shot";
        let account_ref = "uid-chat";
        let session_id = "session-chat";
        super::super::bridge_billing::persist_core_request_attribution_with_mode(
            &fixture.dir, request_id, "key_chat", true,
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, account_ref, session_id,
        ).unwrap();
        let app = fixture.take_app();
        let finalize = || Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize-chat"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();

        let response = app.clone().oneshot(finalize()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let unknown: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(unknown["status"], "unknown");
        assert!(unknown["actual_credits"].is_null());

        let now = chrono::Utc::now().timestamp();
        let usage_cache = serde_json::json!({
            "fetched_at": now,
            "accounts": {
                account_ref: {
                    "name":"测试账号", "last_fetch_end_ts":now, "daily":{},
                    "session_usage": {
                        session_id: {
                            "session_id":session_id, "usage_time":now, "date":"2026-09-24",
                            "model_name":"DeepSeek", "credits_float":"0.050400",
                            "ambiguous":false, "core_request_id":request_id,
                            "core_key_id":"key_chat", "core_attribution_ambiguous":false
                        }
                    }
                }
            }
        });
        let cache_path = fixture.dir.join("data").join("usage_history.json");
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, serde_json::to_vec(&usage_cache).unwrap()).unwrap();

        let response = app.clone().oneshot(finalize()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt["request_id"], request_id);
        assert_eq!(receipt["status"], "final");
        assert_eq!(receipt["actual_credits"], "0.050400");
        assert_eq!(receipt["source_ref"], "trae-usage-session:session-chat");
        assert!(receipt["task_ref"].is_null());
    }

    #[tokio::test]
    async fn chat_finalization_returns_unknown_while_the_background_refresh_is_blocked() {
        let mut fixture = BridgeFixture::new();
        let request_id = "request-chat-blocked-refresh";
        super::super::bridge_billing::persist_core_request_attribution(
            &fixture.dir, request_id, "key_chat",
        ).unwrap();
        super::super::bridge_billing::persist_core_session_attempt(
            &fixture.dir, request_id, "uid-chat", "session-chat-blocked",
        ).unwrap();

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (first_started_tx, first_started_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let release_first_rx = Arc::new(std::sync::Mutex::new(release_first_rx));
        let (follow_up_tx, follow_up_rx) = std::sync::mpsc::channel();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worker_calls = calls.clone();
        let worker_release_rx = release_first_rx.clone();
        let state = fixture.state.clone();
        let data_dir = fixture.dir.clone();
        let worker = super::super::usage_refresh::start_with(
            state,
            stop.clone(),
            move || super::super::bridge_billing::pending_core_session_accounts_for_poll(&data_dir),
            move |_| {
                let call = worker_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    first_started_tx.send(()).unwrap();
                    worker_release_rx.lock().unwrap().recv().unwrap();
                } else {
                    follow_up_tx.send(()).unwrap();
                }
                false
            },
        );
        first_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();

        let request = Request::builder()
            .method("POST")
            .uri(format!("/internal/bridge/requests/{request_id}/billing/finalize-chat"))
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();
        let response = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            fixture.take_app().oneshot(request),
        ).await.expect("finalize must not wait for the blocked upstream fetch").unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let receipt: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(receipt["status"], "unknown");
        assert!(receipt["actual_credits"].is_null());

        release_first_tx.send(()).unwrap();
        follow_up_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        stop.store(true, std::sync::atomic::Ordering::Release);
        worker.join().unwrap();
    }

    #[test]
    fn restart_during_stopping_fetch_hands_pending_work_to_one_bounded_successor() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = BridgeFixture::new();
        let old_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let new_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (old_started_tx, old_started_rx) = std::sync::mpsc::channel();
        let (release_old_tx, release_old_rx) = std::sync::mpsc::channel();
        let release_old_rx = Arc::new(std::sync::Mutex::new(release_old_rx));
        let old_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let all_active = Arc::new(AtomicUsize::new(0));
        let max_all_active = Arc::new(AtomicUsize::new(0));
        let per_account = Arc::new(std::sync::Mutex::new(std::collections::HashMap::<String, usize>::new()));
        let max_per_account = Arc::new(AtomicUsize::new(0));

        let old_state = fixture.state.clone();
        let old_release = release_old_rx.clone();
        let old_active = all_active.clone();
        let old_max_active = max_all_active.clone();
        let old_per_account = per_account.clone();
        let old_max_per_account = max_per_account.clone();
        let old_started_flag = old_started.clone();
        let old_worker = super::super::usage_refresh::start_with_result(
            old_state,
            old_stop.clone(),
            || Ok(vec![("uid-a".to_string(), 1000)]),
            move |item| {
                let active = old_active.fetch_add(1, Ordering::SeqCst) + 1;
                old_max_active.fetch_max(active, Ordering::SeqCst);
                let account_active = {
                    let mut counts = old_per_account.lock().unwrap();
                    let count = counts.entry(item.account_ref.clone()).or_default();
                    *count += 1;
                    *count
                };
                old_max_per_account.fetch_max(account_active, Ordering::SeqCst);
                old_started_flag.store(true, Ordering::SeqCst);
                old_started_tx.send(()).unwrap();
                old_release.lock().unwrap().recv().unwrap();
                *old_per_account.lock().unwrap().get_mut(&item.account_ref).unwrap() -= 1;
                old_active.fetch_sub(1, Ordering::SeqCst);
                true
            },
        ).unwrap().expect("first scheduler starts");
        old_started_rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();

        old_stop.store(true, Ordering::Release);
        let (new_started_tx, new_started_rx) = std::sync::mpsc::channel();
        let gate = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
        let new_gate = gate.clone();
        let new_active = all_active.clone();
        let new_max_active = max_all_active.clone();
        let new_per_account = per_account.clone();
        let new_max_per_account = max_per_account.clone();
        let state = fixture.state.clone();
        let pending = (b'a'..=b'h')
            .map(|suffix| (format!("uid-{}", suffix as char), 2000))
            .collect::<Vec<_>>();
        let deferred = super::super::usage_refresh::start_with_result(
            state,
            new_stop.clone(),
            move || Ok(pending.clone()),
            move |item| {
                let active = new_active.fetch_add(1, Ordering::SeqCst) + 1;
                new_max_active.fetch_max(active, Ordering::SeqCst);
                let account_active = {
                    let mut counts = new_per_account.lock().unwrap();
                    let count = counts.entry(item.account_ref.clone()).or_default();
                    *count += 1;
                    *count
                };
                new_max_per_account.fetch_max(account_active, Ordering::SeqCst);
                new_started_tx.send(item.account_ref.clone()).unwrap();
                let (released, changed) = &*new_gate;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = changed.wait(released).unwrap();
                }
                *new_per_account.lock().unwrap().get_mut(&item.account_ref).unwrap() -= 1;
                new_active.fetch_sub(1, Ordering::SeqCst);
                false
            },
        ).unwrap();

        release_old_tx.send(()).unwrap();
        let mut started = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        for _ in 0..4 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() { break; }
            match new_started_rx.recv_timeout(remaining) {
                Ok(account_ref) => started.push(account_ref),
                Err(_) => break,
            }
        }
        let fifth_started_while_four_blocked = new_started_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_ok();
        {
            let (released, changed) = &*gate;
            *released.lock().unwrap() = true;
            changed.notify_all();
        }
        while started.len() < 8 {
            match new_started_rx.recv_timeout(std::time::Duration::from_secs(2)) {
                Ok(account_ref) => started.push(account_ref),
                Err(_) => break,
            }
        }
        new_stop.store(true, Ordering::Release);
        old_worker.join().unwrap();

        assert_eq!(started.len(), 8, "successor must discover and process every persisted pending account");
        assert!(!fifth_started_while_four_blocked, "successor must not exceed four in-flight reads");
        assert!(old_started.load(Ordering::SeqCst));
        assert!(max_all_active.load(Ordering::SeqCst) <= 4);
        assert_eq!(max_per_account.load(Ordering::SeqCst), 1, "old and successor reads for uid-a must not overlap");
        assert!(deferred.is_none(), "restart during draining must be explicitly deferred to the existing supervisor");
    }

    #[test]
    fn zero_worker_startup_reports_failure_and_releases_its_registry_entry() {
        let fixture = BridgeFixture::new();
        let failed = super::super::usage_refresh::start_with_no_workers(
            fixture.state.clone(),
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            || Ok(Vec::new()),
            |_| false,
        );
        let error = failed.expect_err("a scheduler with no workers must not report successful startup");
        assert!(error.contains("no background usage worker"));

        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker = super::super::usage_refresh::start_with_result(
            fixture.state.clone(),
            stop.clone(),
            || Ok(Vec::new()),
            |_| false,
        ).unwrap().expect("failed startup must leave the registry available for a retry");
        stop.store(true, std::sync::atomic::Ordering::Release);
        worker.join().unwrap();
    }

    #[test]
    fn scheduler_retries_a_failed_initial_pending_loader_without_restarting_workers() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let fixture = BridgeFixture::new();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fake_now = Arc::new(std::sync::Mutex::new(std::time::Instant::now()));
        let clock_now = fake_now.clone();
        let clock = move || *clock_now.lock().unwrap();

        let (wait_requested_tx, wait_requested_rx) = std::sync::mpsc::channel();
        let (advance_tx, advance_rx) = std::sync::mpsc::channel::<std::time::Duration>();
        let advance_rx = Arc::new(std::sync::Mutex::new(advance_rx));
        let waiter_now = fake_now.clone();
        let waiter = move |requested: std::time::Duration| {
            wait_requested_tx.send(requested).unwrap();
            let advance = advance_rx.lock().unwrap().recv().unwrap();
            let mut now = waiter_now.lock().unwrap();
            *now = *now + advance;
        };

        let (first_failure_tx, first_failure_rx) = std::sync::mpsc::channel();
        let (recovered_tx, recovered_rx) = std::sync::mpsc::channel();
        let loader_calls = Arc::new(AtomicUsize::new(0));
        let loader_call_count = loader_calls.clone();
        let pending_loader = move || {
            if loader_call_count.fetch_add(1, Ordering::SeqCst) == 0 {
                first_failure_tx.send(()).unwrap();
                Err("injected temporary pending-index failure".into())
            } else {
                recovered_tx.send(()).unwrap();
                Ok(vec![("uid-recovered".to_string(), 42000)])
            }
        };
        let (processed_tx, processed_rx) = std::sync::mpsc::channel();
        let scheduler = super::super::usage_refresh::start_with_timing(
            fixture.state.clone(),
            stop.clone(),
            pending_loader,
            move |item| {
                processed_tx.send(item.account_ref.clone()).unwrap();
                false
            },
            clock,
            waiter,
        )
        .unwrap()
        .expect("four workers and their supervisor should start");

        first_failure_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        for _ in 0..8 {
            let requested = wait_requested_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            assert!(requested <= std::time::Duration::from_millis(250));
            advance_tx.send(requested).unwrap();
        }
        recovered_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert_eq!(
            processed_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            "uid-recovered"
        );
        assert_eq!(loader_calls.load(Ordering::SeqCst), 2);

        stop.store(true, Ordering::Release);
        let _ = advance_tx.send(std::time::Duration::ZERO);
        scheduler.join().unwrap();
    }

    #[tokio::test]
    async fn authenticated_bridge_can_sync_and_read_redacted_core_key_usage() {
        let mut fixture = BridgeFixture::new();
        let app = fixture.take_app();
        let snapshot = serde_json::json!({
            "version": 7,
            "keys": [
                {"id":"key_opaque_a","display_name":"周的电脑","active":true},
                {"id":"key_opaque_b","display_name":"工作站","active":false}
            ]
        });
        let sync = Request::builder()
            .method("PUT")
            .uri("/internal/bridge/key-registry")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .body(Body::from(snapshot.to_string()))
            .unwrap();
        let response = app.clone().oneshot(sync).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let usage = Request::builder()
            .uri("/internal/bridge/key-usage")
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(usage).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["keys"].as_array().unwrap().len(), 2);
        assert_eq!(body["keys"][0]["key_id"], "key_opaque_a");
        assert_eq!(body["keys"][0]["verified_credits"], "0.000000");
        assert_eq!(body["keys"][1]["active"], false);
        assert!(body.get("plaintext").is_none());
        assert!(!body.to_string().contains("aw_live_"));
    }

    #[tokio::test]
    async fn key_registry_and_usage_routes_reject_non_bridge_credentials() {
        let mut fixture = BridgeFixture::new();
        let request = Request::builder()
            .uri("/internal/bridge/key-usage")
            .body(Body::empty())
            .unwrap();
        let response = fixture.take_app().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn authenticated_core_request_headers_are_persisted_before_handler_dispatch() {
        let mut fixture = BridgeFixture::new();
        let app = fixture.take_app();
        let request = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("x-core-request-id", "core-test-req-1")
            .header("x-core-key-id", "key_opaque_a")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"test-model","messages":[{"role":"user","content":"hello"}]}"#))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_ne!(response.status(), StatusCode::CONFLICT);

        let usage = Request::builder()
            .uri("/internal/bridge/key-usage")
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(usage).await.unwrap();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let key = body["keys"].as_array().unwrap().iter()
            .find(|value| value["key_id"] == "key_opaque_a").unwrap();
        assert_eq!(key["pending_requests"], 1);
    }
}

/// WB 上游健康检测线程（F-34 ④/§2.2 频控维度）：每 5min + 0-60s 抖动对 CN 主域名
/// 发一次轻量 GET（模型目录路径）。无凭证探测——任何 HTTP 响应（含 401）都证明
/// 服务在线，仅连接失败/超时判不可达；结果写 state.wb_probe_* 供 /status 透出。
/// 频控红线：单次单请求、不重试、不批量，探活流量可忽略。
fn spawn_wb_health_probe(state: Arc<ApiSharedState>) {
    use std::sync::atomic::Ordering;
    std::thread::spawn(move || {
        // T5.1/F-37：启动即做一次上游目录动态替换（best effort，失败不影响启动——
        // 本地静态兜底目录保持不动）；取任一健康 WB 账号的凭证拉取
        {
            let picked = state.wb_pool.pick_excluding_constrained(
                &std::collections::HashSet::new(),
                None,
                None,
            );
            if let Some(p) = picked {
                match wb_catalog::fetch_and_replace(
                    &state.data_dir,
                    &p.uid,
                    &p.jwt,
                    &p.domain,
                    &p.enterprise_id,
                    p.global_region,
                ) {
                    Ok(n) => {
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("WB 模型目录动态替换成功: {} 个模型", n),
                        );
                    }
                    Err(e) => {
                        crate::fs_utils::app_log(
                            &state.data_dir,
                            &format!("WB 模型目录动态替换失败（保持静态兜底）: {}", e),
                        );
                    }
                }
            }
        }
        // 探测目标：WB 上游对话主域名（CN）的轻量 GET 路径
        const PROBE_URL: &str =
            concat!("https://copilot.tencent.com", "/console/enterprises/personal/models");
        loop {
            // 5min 基础间隔 + 0-60s 抖动（多实例同时启动时错峰；零新增依赖，纳秒派生）
            let jitter = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos() as u64 % 60)
                .unwrap_or(0);
            std::thread::sleep(std::time::Duration::from_secs(300 + jitter));
            let agent = ureq::AgentBuilder::new()
                .timeout(std::time::Duration::from_secs(10))
                .build();
            let ok = match agent
                .get(PROBE_URL)
                .set("User-Agent", super::wb_upstream::WB_UA)
                .call()
            {
                Ok(_) => true,
                Err(ureq::Error::Status(_, _)) => true, // 有 HTTP 响应 = 服务在线
                Err(_) => false,                        // 网络/超时 = 不可达
            };
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            state.wb_probe_ts_ms.store(now, Ordering::Relaxed);
            state.wb_probe_ok.store(if ok { 1 } else { 0 }, Ordering::Relaxed);
            if !ok {
                // 失败明示（§2.2 接口稳定性）：记日志不静默
                eprintln!("[wb-probe] 上游健康检测失败: {PROBE_URL}");
            }
        }
    });
}
