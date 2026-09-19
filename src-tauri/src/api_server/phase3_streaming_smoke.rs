//! Phase 3A integration smoke.
//!
//! The fixture is deliberately Mock-only. It exercises the public route,
//! Core lease bridge, bounded SSE projection, and fail-closed adapter gate;
//! it never reads credentials, legacy pools, or network configuration.

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

    use axum::body::{to_bytes, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::Response;
    use axum::Extension;
    use serde_json::{json, Value};

    use aiwork_core::{
        CoreStore, CostPolicy, NewUser, Principal, QuotaGrant, RegisterUpstreamAccount,
        UpstreamAccountState, UpstreamObservation, ObservationStatus, UserRole,
    };

    use super::super::core_bridge::core_executor::{
        MockStreamAdapter, MockUpstreamExecutor, StreamTerminalOutcome,
    };
    use super::super::core_bridge::{
        CoreUpstreamExecutor, LeaseStreamAdapter, StreamSink,
    };
    use super::super::routes::chat_completions;
    use super::super::usage::KeyId;
    use super::super::{ApiLogger, ApiSharedState, CoreBridge, CoreMode};

    struct SmokeFixture {
        dir: PathBuf,
        state: Arc<ApiSharedState>,
        principal: Principal,
    }

    impl Drop for SmokeFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn core_fixture(register_stream: bool) -> (SmokeFixture, Arc<MockStreamAdapter>) {
        core_fixture_with_stream_adapter(register_stream, None)
    }

    fn core_fixture_with_stream_adapter(
        register_stream: bool,
        custom_stream: Option<Arc<dyn LeaseStreamAdapter>>,
    ) -> (SmokeFixture, Arc<MockStreamAdapter>) {
        let root = PathBuf::from(r"D:\gpt");
        fs::create_dir_all(&root).expect("create D drive smoke root");
        let dir = root.join(format!(
            "aiwork-tauri-phase3-streaming-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).expect("create isolated Tauri smoke directory");

        let store = Arc::new(CoreStore::open(&dir).expect("open Tauri smoke CoreStore"));
        store.migrate().expect("migrate Tauri smoke CoreStore");
        store
            .create_bootstrap_admin(
                NewUser {
                    id: "phase3-smoke-admin".into(),
                    name: "Phase 3 Smoke Admin".into(),
                    role: UserRole::Admin,
                },
                "bootstrap",
            )
            .expect("create smoke admin");
        let admin_key = store
            .issue_api_key("phase3-smoke-admin", "admin", BTreeSet::new(), "bootstrap")
            .expect("issue smoke admin key");
        let admin = Principal {
            user_id: "phase3-smoke-admin".into(),
            key_id: admin_key.id,
            scopes: BTreeSet::new(),
        };
        store
            .create_user(
                NewUser {
                    id: "phase3-smoke-user".into(),
                    name: "Phase 3 Smoke User".into(),
                    role: UserRole::User,
                },
                "phase3-smoke-admin",
            )
            .expect("create smoke user");
        let user_key = store
            .issue_api_key(
                "phase3-smoke-user",
                "user",
                BTreeSet::from(["chat:invoke".to_owned()]),
                "phase3-smoke-admin",
            )
            .expect("issue smoke user key");
        let principal = Principal {
            user_id: "phase3-smoke-user".into(),
            key_id: user_key.id.clone(),
            scopes: BTreeSet::from(["chat:invoke".to_owned()]),
        };
        store
            .upsert_cost_policy(CostPolicy {
                id: "phase3-smoke-policy".into(),
                endpoint: "chat".into(),
                model_pattern: "mock-*".into(),
                resource_kind: "chat_request".into(),
                reserve_amount: 1,
                max_actual_amount: Some(1),
                version: 1,
                enabled: true,
            })
            .expect("install smoke policy");
        store
            .grant(QuotaGrant {
                user_id: "phase3-smoke-user".into(),
                resource_kind: "chat_request".into(),
                amount: 2,
                actor_user_id: "phase3-smoke-admin".into(),
                reason: "phase3 smoke grant".into(),
            })
            .expect("grant smoke quota");

        let mut account = RegisterUpstreamAccount::new(
            "mock-account".into(),
            "mock".into(),
            "vault://phase3/mock-account".into(),
        );
        account.capabilities.insert("chat".into());
        account.region = Some("mock".into());
        account.enabled = true;
        account.max_concurrency = 1;
        account.state = UpstreamAccountState::Available;
        store
            .upsert_upstream_account(account, &admin)
            .expect("register smoke account");
        let now = chrono::Utc::now().timestamp_millis();
        store
            .append_upstream_observation(UpstreamObservation::new(
                "phase3-smoke-observation".into(),
                "mock-account".into(),
                "chat_request".into(),
                Some(100),
                1,
                "reader".into(),
                ObservationStatus::Fresh,
                now,
                now + 600_000,
                json!({"source": "reader", "status": "fresh"}),
            ))
            .expect("record smoke observation");
        let runtime = super::super::scheduler::SchedulerRuntime::new(
            store.clone(),
            dir.clone(),
            super::super::scheduler::SchedulerMode::Enforce,
            Default::default(),
            Default::default(),
            now,
        )
        .expect("create smoke scheduler runtime");

        let stream_mock = Arc::new(MockStreamAdapter::with_outcome(
            StreamTerminalOutcome::Success {
                actual_units: Some(1),
                upstream_request_ref: Some("mock-stream-ref".into()),
            },
        ));
        let nonstream_mock = Arc::new(MockUpstreamExecutor::ok());
        let mut executor = CoreUpstreamExecutor::new()
            .with_provider("mock", nonstream_mock)
            .for_account("mock-account", "mock", "vault://phase3/mock-account");
        if register_stream {
            let stream_adapter: Arc<dyn LeaseStreamAdapter> =
                custom_stream.unwrap_or_else(|| stream_mock.clone());
            executor = executor.with_stream_provider("mock", stream_adapter);
        }
        let bridge = CoreBridge::new(store, CoreMode::Enforce)
            .with_upstream_executor(executor)
            .with_scheduler(Arc::new(runtime))
            .expect("attach smoke scheduler");
        let state = Arc::new(ApiSharedState {
            core: Some(Arc::new(bridge)),
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
            default_model: "mock-1".into(),
            data_dir: dir.clone(),
            video_payloads: super::super::video_payload::VideoPayloadStore::new(&dir),
            cors_origins: String::new(),
            total_requests: std::sync::atomic::AtomicU64::new(0),
            inflight: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            limiter: super::super::limits::RateLimiter::from_env(),
            active_uid: std::sync::Mutex::new(None),
            last_error: std::sync::Mutex::new(None),
            logger: ApiLogger::new(dir.join("logs")),
            debug_enabled: std::sync::atomic::AtomicBool::new(false),
            usage: std::sync::Mutex::new(super::super::usage::UsageFile::default()),
            wb_probe_ts_ms: std::sync::atomic::AtomicI64::new(-1),
            wb_probe_ok: std::sync::atomic::AtomicI64::new(-1),
        });
        (
            SmokeFixture {
                dir,
                state,
                principal,
            },
            stream_mock,
        )
    }

    fn chat_body(stream: bool) -> Bytes {
        Bytes::from(
            json!({
                "model": "mock-1",
                "messages": [{"role": "user", "content": "hello"}],
                "stream": stream,
            })
            .to_string(),
        )
    }

    async fn body_text(response: Response) -> String {
        String::from_utf8(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("read smoke response body")
                .to_vec(),
        )
        .expect("smoke body is UTF-8")
    }

    #[tokio::test]
    async fn phase3_streaming_smoke_is_bounded_mock_sse_and_idempotent() {
        let (fixture, mock) = core_fixture(true);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-smoke-success".parse().unwrap());

        let first = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers.clone(),
            chat_body(true),
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        assert_eq!(
            first
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let first_body = body_text(first).await;
        assert!(first_body.contains("hello"));
        assert!(first_body.contains("data: [DONE]"));
        assert_eq!(first_body.matches("data:").count(), 2, "Mock stream must stay bounded");

        let replay = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        let replay_body: Value = serde_json::from_str(&body_text(replay).await).expect("replay JSON");
        assert_eq!(replay_body["idempotent_replay"], true);
        assert_eq!(mock.calls().len(), 1, "same idempotency key must not execute twice");
        assert_eq!(mock.calls()[0].account_ref, "mock-account");

        let replay_lookup = fixture
            .state
            .core
            .as_ref()
            .unwrap()
            .lookup_chat_replay(
                &fixture.principal,
                &fixture.principal.key_id,
                "phase3-smoke-success",
                &serde_json::from_slice(&chat_body(true)).unwrap(),
            )
            .expect("read-only replay lookup");
        assert_eq!(
            replay_lookup.as_ref().map(|replay| replay.state),
            Some(aiwork_core::RequestState::Settled)
        );
        assert_eq!(
            replay_lookup
                .as_ref()
                .and_then(|replay| replay.replay_lease.as_ref())
                .map(|lease| lease.state),
            Some(aiwork_core::LeaseState::Succeeded)
        );

        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("requests").unwrap(), 1);
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 1);
        assert_eq!(store.balance("phase3-smoke-user", "chat_request").unwrap().held, 0);
        let db = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        let state: String = db
            .query_row("SELECT state FROM upstream_leases", [], |row| row.get(0))
            .unwrap();
        assert_eq!(state, "succeeded");
    }

    #[tokio::test]
    async fn phase3_missing_stream_adapter_returns_501_before_any_hold() {
        let (fixture, mock) = core_fixture(false);
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-smoke-no-adapter".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let payload: Value = serde_json::from_str(&body_text(response).await).expect("501 JSON");
        assert_eq!(payload["error"]["code"], "scheduler_endpoint_not_enabled");
        let store = &fixture.state.core.as_ref().unwrap().store;
        assert_eq!(store.count_rows("requests").unwrap(), 0);
        assert_eq!(store.count_rows("upstream_leases").unwrap(), 0);
        assert_eq!(store.balance("phase3-smoke-user", "chat_request").unwrap().held, 0);
        assert!(mock.calls().is_empty());
    }

    struct BlockingStreamAdapter;

    impl LeaseStreamAdapter for BlockingStreamAdapter {
        fn execute_stream(
            &self,
            _lease: &aiwork_core::UpstreamLeaseGrant,
            _request: aiwork_core::ChatExecutionRequest,
            sink: &mut dyn StreamSink,
        ) -> StreamTerminalOutcome {
            while !sink.cancel_requested() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            StreamTerminalOutcome::TransportUnknown {
                reason: "blocking_test_worker_observed_cancel".into(),
                upstream_request_ref: None,
            }
        }
    }

    #[tokio::test]
    async fn phase3_client_disconnect_settles_unknown_while_worker_is_blocked() {
        let (fixture, _mock) = core_fixture_with_stream_adapter(
            true,
            Some(Arc::new(BlockingStreamAdapter)),
        );
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "phase3-smoke-disconnect".parse().unwrap());
        let response = chat_completions(
            State(fixture.state.clone()),
            Some(Extension(KeyId(fixture.principal.key_id.clone()))),
            Some(Extension(fixture.principal.clone())),
            headers,
            chat_body(true),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        drop(response);

        let body: Value = serde_json::from_slice(&chat_body(true)).unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let replay = fixture
                .state
                .core
                .as_ref()
                .unwrap()
                .lookup_chat_replay(
                    &fixture.principal,
                    &fixture.principal.key_id,
                    "phase3-smoke-disconnect",
                    &body,
                )
                .unwrap()
                .expect("disconnect request row");
            if replay
                .replay_lease
                .as_ref()
                .is_some_and(|lease| lease.state == aiwork_core::LeaseState::Unknown)
            {
                assert_eq!(
                    fixture
                        .state
                        .core
                        .as_ref()
                        .unwrap()
                        .balance("phase3-smoke-user", "chat_request")
                        .unwrap()
                        .held,
                    1
                );
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "disconnect was not settled");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
}
