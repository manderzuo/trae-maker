use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::Arc,
};

use axum::{body::{to_bytes, Body}, http::{Request, Response, StatusCode}, Router};
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::BridgeClient,
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

struct Fixture {
    app: Router,
    cookie: String,
    key_id: String,
    dir: PathBuf,
}

fn test_dir() -> PathBuf {
    PathBuf::from(format!(r"D:\gpt\starlink-video-billing-admin-{}", rand::random::<u64>()))
}

fn fixture() -> Fixture {
    let dir = test_dir();
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(
        aiwork_core::NewUser { id: "admin".into(), name: "管理员".into(), role: aiwork_core::UserRole::Admin },
        "bootstrap",
    ).unwrap();
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let issued = store.issue_api_key_for_new_user_as_admin(
        &admin,
        "验收 Key",
        BTreeSet::from(["videos:submit".into()]),
        1,
    ).unwrap();
    let config = RouterConfig::defaults(dir.clone());
    let state = StarlinkRouterState::for_test(store, BridgeClient::new("", ""), config);
    let (token, _) = state.admin_sessions.issue("admin".into(), chrono::Utc::now().timestamp_millis());
    Fixture { app: build_router(state), cookie: format!("starlink_admin_session={token}"), key_id: issued.id, dir }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

async fn json_body(response: Response<Body>) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

async fn admin_request(fixture: &Fixture, method: axum::http::Method, path: &str, body: Value) -> Response<Body> {
    fixture.app.clone().oneshot(
        Request::builder()
            .method(method)
            .uri(path)
            .header("cookie", &fixture.cookie)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    ).await.unwrap()
}

#[tokio::test]
async fn admin_can_read_paused_state_and_unverified_counts() {
    let fixture = fixture();
    let response = admin_request(&fixture, axum::http::Method::GET, "/admin/v1/video-billing", json!({})).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["mode"], "paused");
    assert!(body.get("diagnostic_key_id").is_none() || body["diagnostic_key_id"].is_null());
    assert!(body.get("plaintext").is_none());
}

#[tokio::test]
async fn diagnostic_endpoint_requires_key_id_hash_and_consumes_once() {
    let fixture = fixture();
    let input = json!({"key_id": fixture.key_id, "request_hash": "a".repeat(64), "reason": "验收"});
    let first = admin_request(&fixture, axum::http::Method::POST, "/admin/v1/video-billing/diagnostic", input.clone()).await;
    assert_eq!(first.status(), StatusCode::OK);
    let second = admin_request(&fixture, axum::http::Method::POST, "/admin/v1/video-billing/diagnostic", input).await;
    assert_eq!(second.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn video_billing_controls_require_an_admin_session() {
    let fixture = fixture();
    let response = fixture.app.clone().oneshot(
        Request::get("/admin/v1/video-billing").body(Body::empty()).unwrap(),
    ).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
