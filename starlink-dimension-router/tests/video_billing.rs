use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{body::Body, http::{Request, Response, StatusCode}, Router};
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

#[derive(Clone)]
struct VideoFixture {
    app: Router,
    state: Arc<StarlinkRouterState>,
    store: Arc<aiwork_core::CoreStore>,
    key: String,
    key_id: String,
    dir: PathBuf,
    bridge: Arc<FakeBridge>,
}

struct FakeBridge {
    status_body: Mutex<Value>,
    submit_status: Mutex<u16>,
    requests: Mutex<Vec<String>>,
}

impl FakeBridge {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            status_body: Mutex::new(json!({
                "task": {"id": "video-test", "status": "queued"}
            })),
            submit_status: Mutex::new(202),
            requests: Mutex::new(Vec::new()),
        })
    }

    fn set_status(&self, value: Value) {
        *self.status_body.lock().unwrap() = value;
    }

    fn set_submit_status(&self, status: u16) {
        *self.submit_status.lock().unwrap() = status;
    }

    fn request_count(&self, path: &str) -> usize {
        self.requests.lock().unwrap().iter().filter(|item| item.as_str() == path).count()
    }
}

impl BridgeTransport for FakeBridge {
    fn send(
        &self,
        method: &str,
        url: &str,
        _headers: &BTreeMap<String, String>,
        _body: &[u8],
    ) -> Result<BridgeResponse, String> {
        let path = url.trim_start_matches("http://bridge").to_owned();
        self.requests.lock().unwrap().push(path.clone());
        let response = match (method, path.as_str()) {
            ("POST", "/v1/videos/generations") => BridgeResponse {
                status: *self.submit_status.lock().unwrap(),
                headers: BTreeMap::new(),
                body: br#"{"task":{"id":"video-test","status":"queued"}}"#.to_vec(),
            },
            ("GET", "/v1/videos/video-test") => BridgeResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: serde_json::to_vec(&*self.status_body.lock().unwrap()).unwrap(),
            },
            _ => BridgeResponse { status: 404, headers: BTreeMap::new(), body: br#"{}"#.to_vec() },
        };
        Ok(response)
    }
}

fn test_dir(label: &str) -> PathBuf {
    PathBuf::from(format!(r"D:\gpt\starlink-video-billing-{label}-{}", rand::random::<u64>()))
}

fn fixture(label: &str) -> VideoFixture {
    let dir = test_dir(label);
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(
        aiwork_core::NewUser { id: "admin".into(), name: "管理员".into(), role: aiwork_core::UserRole::Admin },
        "bootstrap",
    ).unwrap();
    let admin = aiwork_core::Principal { user_id: "admin".into(), key_id: "admin_session:test".into(), scopes: BTreeSet::from(["admin:*".into()]) };
    let issued = store.issue_api_key_for_new_user_as_admin(
        &admin,
        "视频测试用户",
        BTreeSet::from(["videos:submit".into()]),
        2,
    ).unwrap();
    store.quota_pool_grant_as_admin(&admin, aiwork_core::QuotaGrant {
        user_id: issued.user_id.clone(), resource_kind: "credits".into(), amount: 10,
        actor_user_id: "admin".into(), reason: "video billing test".into(),
    }).unwrap();
    store.key_quota_allocate_from_pool_as_admin(&admin, aiwork_core::KeyQuotaGrant {
        api_key_id: issued.id.clone(), resource_kind: "credits".into(), amount: 10,
        actor_user_id: "admin".into(), reason: "video billing test".into(),
    }).unwrap();
    store.set_video_billing_control(aiwork_core::VideoBillingControlInput {
        mode: aiwork_core::VideoBillingMode::Active,
        reason: "测试开启".into(),
        diagnostic_key_id: None,
        diagnostic_request_hash: None,
    }).unwrap();
    let bridge = FakeBridge::new();
    let config = RouterConfig::defaults(dir.clone());
    let client = BridgeClient::from_transport("http://bridge", "bridge-secret", bridge.clone());
    let state = StarlinkRouterState::for_test(store.clone(), client, config);
    let app = build_router(state.clone());
    VideoFixture { app, state, store, key: issued.plaintext, key_id: issued.id, dir, bridge }
}

impl Drop for VideoFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

async fn post_video(fixture: &VideoFixture) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::post("/v1/videos/generations")
            .header("authorization", format!("Bearer {}", fixture.key))
            .header("content-type", "application/json")
            .header("idempotency-key", format!("video-billing-{}", rand::random::<u64>()))
            .body(Body::from(r#"{"model":"seedance","prompt":"test"}"#))
            .unwrap(),
    ).await.unwrap()
}

async fn poll_video(fixture: &VideoFixture) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::get("/v1/videos/video-test")
            .header("authorization", format!("Bearer {}", fixture.key))
            .body(Body::empty())
            .unwrap(),
    ).await.unwrap()
}

fn quota(fixture: &VideoFixture) -> aiwork_core::CoreQuotaUsageView {
    let principal = fixture.store.authenticate_api_key(&fixture.key).unwrap();
    fixture.store.key_quota_usage_for_principal(&principal, 100).unwrap()
}

#[tokio::test]
async fn accepted_video_keeps_one_reservation_held() {
    let fixture = fixture("held");
    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 1);
    assert_eq!(usage.balances[0].settled, 0);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "held");
}

#[tokio::test]
async fn verified_receipt_commits_actual_credits_once() {
    let fixture = fixture("verified");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({
        "task": {"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 1);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "settled");
    assert_eq!(fixture.bridge.request_count("/v1/videos/video-test"), 2);
}

#[tokio::test]
async fn concurrent_verified_polls_commit_only_once() {
    let fixture = fixture("concurrent");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"credits","task_ref":"video-test"}}
    }));
    let (first, second) = tokio::join!(poll_video(&fixture), poll_video(&fixture));
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(second.status(), StatusCode::OK);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 1);
    assert_eq!(fixture.state.jobs.lock().unwrap()["video-test"].billing_state, "settled");
}

#[tokio::test]
async fn completed_without_verified_receipt_is_reconciliation_required() {
    let fixture = fixture("unverified");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"completed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "reconcile_required");
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(quota(&fixture).balances[0].held, 1);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn accepted_upstream_failure_without_verified_receipt_stays_held_for_reconciliation() {
    let fixture = fixture("failed");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({"task":{"id":"video-test","status":"failed"}}));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "reconcile_required");
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(quota(&fixture).balances[0].held, 1);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn fractional_receipt_is_stored_for_reconciliation_before_fixed_point_migration() {
    let fixture = fixture("fractional");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"12.500000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.status, "reconcile_required");
    assert_eq!(job.actual_credits.as_deref(), Some("12.500000"));
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn receipt_over_the_held_upper_bound_is_never_committed() {
    let fixture = fixture("over-bound");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"2.000000","unit":"credits","task_ref":"video-test"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(job.error_code.as_deref(), Some("actual_credits_exceed_hold"));
    assert_eq!(quota(&fixture).balances[0].held, 1);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn wrong_task_reference_and_unit_are_not_accepted_as_verified_receipts() {
    let fixture = fixture("wrong-receipt");
    assert_eq!(post_video(&fixture).await.status(), StatusCode::ACCEPTED);
    fixture.bridge.set_status(json!({
        "task":{"id":"video-test","status":"completed",
        "billing":{"status":"verified","actual_credits":"1.000000","unit":"points","task_ref":"other-task"}}
    }));
    assert_eq!(poll_video(&fixture).await.status(), StatusCode::OK);
    let job = fixture.state.jobs.lock().unwrap()["video-test"].clone();
    assert_eq!(job.billing_state, "reconcile_required");
    assert_eq!(job.error_code.as_deref(), Some("billing_receipt_unverified"));
    assert_eq!(quota(&fixture).balances[0].held, 1);
    assert_eq!(quota(&fixture).balances[0].settled, 0);
}

#[tokio::test]
async fn pre_accept_rejection_releases_the_hold() {
    let fixture = fixture("rejected");
    fixture.bridge.set_submit_status(400);
    assert_eq!(post_video(&fixture).await.status(), StatusCode::BAD_REQUEST);
    let usage = quota(&fixture);
    assert_eq!(usage.balances[0].held, 0);
    assert_eq!(usage.balances[0].settled, 0);
}

#[tokio::test]
async fn diagnostic_mode_rejects_a_non_claimed_key_before_reservation() {
    let fixture = fixture("diagnostic");
    fixture.store.set_video_billing_control(aiwork_core::VideoBillingControlInput::diagnostic(
        &fixture.key_id, &"a".repeat(64), "only one diagnostic request",
    )).unwrap();
    let response = post_video(&fixture).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(quota(&fixture).balances[0].held, 0);
}
