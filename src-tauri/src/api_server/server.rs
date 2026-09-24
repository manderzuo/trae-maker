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
}

impl ApiServerHandle {
    /// 发送 shutdown 信号并等待优雅退出（最多 3s，每 50ms 轮询一次），
    /// 超时才 abort（P2 修复9：原实现 send 后立即 abort，优雅停机被自身取消，
    /// 在途请求被硬断）
    pub fn stop(&mut self) {
        self.background_stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        if let Some(h) = self.join_handle.take() {
            if !wait_task_finished(&h, std::time::Duration::from_secs(3)) {
                h.abort();
            }
        }
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

    let app = build_router(state.clone());
    spawn_wb_health_probe(state.clone());
    let background_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    spawn_core_usage_reconciler(state.clone(), background_stop.clone());
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
    })
}

/// Poll linked Core usage sessions once per minute. The worker does nothing
/// until an authenticated Core request has produced a persisted upstream
/// account/session association, and then fetches only those accounts.
fn spawn_core_usage_reconciler(
    state: Arc<ApiSharedState>,
    stop: Arc<std::sync::atomic::AtomicBool>,
) {
    let _ = std::thread::Builder::new()
        .name("aiwork-core-usage-poll".into())
        .spawn(move || loop {
            for _ in 0..60 {
                if stop.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            if stop.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }

            let pending = match super::bridge_billing::pending_core_session_accounts_for_poll(&state.data_dir) {
                Ok(pending) => pending,
                Err(_) => {
                    crate::fs_utils::app_log(&state.data_dir, "Core 用量轮询失败：待核验请求索引不可用");
                    continue;
                }
            };
            if pending.is_empty() {
                continue;
            }
            let account_refs = pending.iter().map(|(uid, _)| uid.clone()).collect();
            let credentials = state.pool.usage_credentials_for(&account_refs);
            if credentials.is_empty() {
                continue;
            }
            if crate::commands::usage_history::refresh_pending_core_usage(
                &state.data_dir,
                &pending,
                &credentials,
            ).is_err() {
                crate::fs_utils::app_log(&state.data_dir, "Core 用量轮询失败：上游只读用量查询或本地缓存不可用");
            }
        });
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
        .route("/internal/bridge/key-registry", put(super::bridge_api::replace_core_key_registry).layer(DefaultBodyLimit::max(8 * 1024 * 1024)))
        .route("/internal/bridge/key-usage", get(super::bridge_api::core_key_usage))
        .route(
            "/internal/bridge/requests/:request_id/billing",
            get(super::bridge_api::billing),
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
                app: Some(build_router(state)),
                key: issued.plaintext,
                dir,
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
