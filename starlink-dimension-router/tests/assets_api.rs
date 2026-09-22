use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use axum::{
    body::{to_bytes, Body},
    http::{Request, Response, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde_json::{json, Value};
use starlink_dimension_router::{
    bridge_client::{BridgeClient, BridgeResponse, BridgeTransport},
    config::RouterConfig,
    server::build_router,
    state::StarlinkRouterState,
};
use tower::util::ServiceExt;

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\nfixture";

struct Fixture {
    app: Router,
    owner_key: String,
    store: Arc<aiwork_core::CoreStore>,
    dir: PathBuf,
}

struct VideoFixture {
    app: Router,
    key: String,
    other_key: String,
    bridge: Arc<VideoBridge>,
    dir: PathBuf,
}

#[derive(Default)]
struct VideoBridge {
    requests: Mutex<Vec<(String, Vec<u8>)>>,
    fail_after_asset_uploads: Mutex<Option<usize>>,
}

impl VideoBridge {
    fn last_json(&self, path: &str) -> Value {
        let requests = self.requests.lock().unwrap();
        let body = requests.iter().rev().find(|(request_path, _)| request_path == path).map(|(_, body)| body).unwrap();
        serde_json::from_slice(body).unwrap()
    }

    fn paths(&self) -> Vec<String> {
        self.requests.lock().unwrap().iter().map(|(path, _)| path.clone()).collect()
    }

    fn fail_after_asset_uploads(&self, count: usize) {
        *self.fail_after_asset_uploads.lock().unwrap() = Some(count);
    }
}

impl BridgeTransport for VideoBridge {
    fn send(&self, _method: &str, url: &str, _headers: &BTreeMap<String, String>, body: &[u8]) -> Result<BridgeResponse, String> {
        let path = url.trim_start_matches("http://bridge").to_owned();
        self.requests.lock().unwrap().push((path.clone(), body.to_vec()));
        if path == "/v1/assets" {
            let uploaded = self.requests.lock().unwrap().iter().filter(|(request_path, _)| request_path == "/v1/assets").count();
            if *self.fail_after_asset_uploads.lock().unwrap() == Some(uploaded) {
                return Ok(BridgeResponse { status: 502, headers: BTreeMap::new(), body: br#"{"error":"fake bridge failure"}"#.to_vec() });
            }
        }
        let response = if path == "/v1/assets" {
            br#"{"object":"asset","id":"bridge-asset-1"}"#.to_vec()
        } else if path == "/v1/videos/generations" {
            br#"{"id":"video-1","status":"queued"}"#.to_vec()
        } else {
            br#"{}"#.to_vec()
        };
        Ok(BridgeResponse { status: 202, headers: BTreeMap::new(), body: response })
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn test_dir(label: &str) -> PathBuf {
    PathBuf::from(format!(
        r"D:\gpt\starlink-assets-api-test-{label}-{}",
        rand::random::<u64>()
    ))
}

fn fixture() -> Fixture {
    let dir = test_dir("app");
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "系统管理员".into(),
                role: aiwork_core::UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let owner = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "素材拥有者",
            BTreeSet::from(["assets:write".into()]),
            2,
        )
        .unwrap();
    let config = RouterConfig::defaults(dir.clone());
    let state = StarlinkRouterState::for_test(store.clone(), BridgeClient::new("", ""), config);
    Fixture {
        app: build_router(state),
        owner_key: owner.plaintext,
        store,
        dir,
    }
}

fn fixture_with_user_scopes(scopes: &[&str]) -> (Router, String, PathBuf) {
    let dir = test_dir("scope");
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "系统管理员".into(),
                role: aiwork_core::UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin = aiwork_core::Principal {
        user_id: "admin".into(),
        key_id: "admin_session:test".into(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    let user = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "测试用户",
            scopes.iter().map(|scope| (*scope).to_owned()).collect(),
            2,
        )
        .unwrap();
    let config = RouterConfig::defaults(dir.clone());
    let state = StarlinkRouterState::for_test(store, BridgeClient::new("", ""), config);
    (build_router(state), user.plaintext, dir)
}

fn video_fixture() -> VideoFixture {
    let dir = test_dir("video");
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            aiwork_core::NewUser { id: "admin".into(), name: "系统管理员".into(), role: aiwork_core::UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    let admin = aiwork_core::Principal { user_id: "admin".into(), key_id: "admin_session:test".into(), scopes: BTreeSet::from(["admin:*".into()]) };
    let key = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "视频用户",
            BTreeSet::from(["assets:write".into(), "videos:submit".into(), "chat:invoke".into()]),
            2,
        )
        .unwrap();
    let other = store
        .issue_api_key_for_new_user_as_admin(
            &admin,
            "其他视频用户",
            BTreeSet::from(["assets:write".into(), "videos:submit".into(), "chat:invoke".into()]),
            2,
        )
        .unwrap();
    store
        .quota_pool_grant_as_admin(&admin, aiwork_core::QuotaGrant { user_id: key.user_id.clone(), resource_kind: "credits".into(), amount: 100, actor_user_id: "admin".into(), reason: "asset relay test".into() })
        .unwrap();
    store
        .quota_pool_grant_as_admin(&admin, aiwork_core::QuotaGrant { user_id: other.user_id.clone(), resource_kind: "credits".into(), amount: 100, actor_user_id: "admin".into(), reason: "asset relay test".into() })
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(&admin, aiwork_core::KeyQuotaGrant { api_key_id: other.id.clone(), resource_kind: "credits".into(), amount: 100, actor_user_id: "admin".into(), reason: "asset relay test".into() })
        .unwrap();
    store
        .key_quota_allocate_from_pool_as_admin(&admin, aiwork_core::KeyQuotaGrant { api_key_id: key.id.clone(), resource_kind: "credits".into(), amount: 100, actor_user_id: "admin".into(), reason: "asset relay test".into() })
        .unwrap();
    store
        .set_video_billing_control(aiwork_core::VideoBillingControlInput {
            mode: aiwork_core::VideoBillingMode::Active,
            reason: "video asset route test".into(),
            diagnostic_key_id: None,
            diagnostic_request_hash: None,
        })
        .unwrap();
    let bridge = Arc::new(VideoBridge::default());
    let config = RouterConfig::defaults(dir.clone());
    let client = BridgeClient::from_transport("http://bridge", "bridge-secret", bridge.clone());
    let state = StarlinkRouterState::for_test(store, client, config);
    VideoFixture { app: build_router(state), key: key.plaintext, other_key: other.plaintext, bridge, dir }
}

async fn post_bearer(app: &Router, path: &str, key: &str, value: Value) -> Response<Body> {
    app.clone()
        .oneshot(
            Request::post(path)
                .header("authorization", format!("Bearer {key}"))
                .header("content-type", "application/json")
                .body(Body::from(value.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn get_url_path(app: &Router, url: &str) -> Response<Body> {
    let path = url
        .find("/v1/")
        .map(|index| &url[index..])
        .expect("content URL must contain a /v1/ path");
    app.clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn upload_png_id(app: &Router, key: &str) -> String {
    let response = post_bearer(
        app,
        "/v1/assets",
        key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    response_json(response).await["id"].as_str().unwrap().to_owned()
}

async fn response_json(response: Response<Body>) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn user_can_upload_owned_png_and_download_by_short_token() {
    let fixture = fixture();
    let png = STANDARD.encode(PNG_BYTES);
    let response = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.owner_key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":png}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = response_json(response).await;
    assert_eq!(body["object"], "asset");
    assert_eq!(body["mime_type"], "image/png");
    assert_eq!(body["bytes"], PNG_BYTES.len());
    let content_url = body["content_url"].as_str().unwrap().to_owned();
    let content = get_url_path(&fixture.app, &content_url).await;
    assert_eq!(content.status(), StatusCode::OK);
    assert_eq!(content.headers()["content-type"], "image/png");
    assert_eq!(to_bytes(content.into_body(), usize::MAX).await.unwrap().as_ref(), PNG_BYTES);
}

#[tokio::test]
async fn upload_requires_assets_scope_and_invalid_token_is_not_found() {
    let (app, key, dir) = fixture_with_user_scopes(&["videos:submit"]);
    let denied = post_bearer(
        &app,
        "/v1/assets",
        &key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let _ = fs::remove_dir_all(dir);

    let fixture = fixture();
    let content = get_url_path(
        &fixture.app,
        "http://router/v1/assets/asset-missing/content?token=wrong",
    )
    .await;
    assert_eq!(content.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn upload_rejects_mime_magic_mismatch_and_unsafe_filename() {
    let fixture = fixture();
    let mismatch = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.owner_key,
        json!({"filename":"ref.png","mime_type":"image/jpeg","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);
    let unsafe_name = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.owner_key,
        json!({"filename":"..\\secret.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    assert_eq!(unsafe_name.status(), StatusCode::BAD_REQUEST);
}

#[test]
fn parse_upload_rejects_unknown_and_oversized_assets() {
    let unknown = json!({
        "filename": "ref.bin",
        "mime_type": "application/octet-stream",
        "data_base64": STANDARD.encode(b"not-an-asset")
    });
    assert!(starlink_dimension_router::assets::parse_upload(unknown.to_string().as_bytes()).is_err());

    let oversized = vec![b'a'; starlink_dimension_router::assets::MAX_ASSET_BYTES + 1];
    let oversized_body = json!({
        "filename": "ref.png",
        "mime_type": "image/png",
        "data_base64": STANDARD.encode(oversized)
    });
    let error = starlink_dimension_router::assets::parse_upload(oversized_body.to_string().as_bytes()).unwrap_err();
    assert!(error.to_string().contains("32 MiB"));
}

#[tokio::test]
async fn expired_asset_and_missing_file_are_not_found() {
    let fixture = fixture();
    let response = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.owner_key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    let body = response_json(response).await;
    let asset_id = body["id"].as_str().unwrap().to_owned();
    let content_url = body["content_url"].as_str().unwrap().to_owned();
    fixture.store.expire_assets(Utc::now().timestamp_millis() + 31 * 60 * 1000).unwrap();
    assert_eq!(get_url_path(&fixture.app, &content_url).await.status(), StatusCode::NOT_FOUND);

    let response = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.owner_key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    let body = response_json(response).await;
    let content_url = body["content_url"].as_str().unwrap().to_owned();
    let principal = fixture.store.authenticate_api_key(&fixture.owner_key).unwrap();
    let record = fixture.store.asset_for_user(&principal, &asset_id).unwrap();
    assert!(record.is_some());
    let record = fixture.store.asset_for_user(&principal, body["id"].as_str().unwrap()).unwrap().unwrap();
    fs::remove_file(fixture.dir.join("data").join(record.storage_ref)).unwrap();
    assert_eq!(get_url_path(&fixture.app, &content_url).await.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn video_request_rewrites_core_asset_ids_to_bridge_asset_ids() {
    let fixture = video_fixture();
    let uploaded = post_bearer(
        &fixture.app,
        "/v1/assets",
        &fixture.key,
        json!({"filename":"ref.png","mime_type":"image/png","data_base64":STANDARD.encode(PNG_BYTES)}),
    )
    .await;
    let asset = response_json(uploaded).await;
    let asset_id = asset["id"].as_str().unwrap();
    let response = post_bearer(
        &fixture.app,
        "/v1/videos/generations",
        &fixture.key,
        json!({"model":"seedance","prompt":"让画面动起来","image_asset_ids":[asset_id]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let forwarded = fixture.bridge.last_json("/v1/videos/generations");
    assert_eq!(forwarded["image_asset_ids"][0], "bridge-asset-1");
    assert_ne!(forwarded["image_asset_ids"][0], asset_id);
    assert!(!fixture.bridge.paths().is_empty());
    let _ = fs::remove_dir_all(fixture.dir);
}

#[tokio::test]
async fn bridge_asset_failure_does_not_submit_video_or_keep_reservation() {
    let fixture = video_fixture();
    let first = upload_png_id(&fixture.app, &fixture.key).await;
    let second = upload_png_id(&fixture.app, &fixture.key).await;
    fixture.bridge.fail_after_asset_uploads(2);
    let response = post_bearer(
        &fixture.app,
        "/v1/videos/generations",
        &fixture.key,
        json!({"model":"seedance","prompt":"让画面动起来","image_asset_ids":[first, second]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    assert!(!fixture.bridge.paths().iter().any(|path| path == "/v1/videos/generations"));
}

#[tokio::test]
async fn seedance_chat_rejects_streaming_and_checks_asset_owner_before_forwarding() {
    let fixture = video_fixture();
    let asset_id = upload_png_id(&fixture.app, &fixture.key).await;
    let streamed = post_bearer(
        &fixture.app,
        "/v1/chat/completions",
        &fixture.key,
        json!({"model":"seedance","stream":true,"messages":[{"role":"user","content":"让画面动起来"}]}),
    )
    .await;
    assert_eq!(streamed.status(), StatusCode::BAD_REQUEST);
    let rejected = post_bearer(
        &fixture.app,
        "/v1/chat/completions",
        &fixture.other_key,
        json!({"model":"seedance","stream":false,"messages":[{"role":"user","content":"让画面动起来"}],"image_asset_ids":[asset_id]}),
    )
    .await;
    assert_eq!(rejected.status(), StatusCode::NOT_FOUND);
    assert!(!fixture.bridge.paths().iter().any(|path| path == "/v1/chat/completions"));
}

#[tokio::test]
async fn standard_vision_data_url_is_preserved_for_text_models() {
    let fixture = video_fixture();
    let data_url = "data:image/png;base64,iVBORw0KGgo=";
    let response = post_bearer(
        &fixture.app,
        "/v1/chat/completions",
        &fixture.key,
        json!({"model":"vision-model","stream":false,"messages":[{"role":"user","content":[{"type":"text","text":"请识别"},{"type":"image_url","image_url":{"url":data_url}}]}]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let forwarded = fixture.bridge.last_json("/v1/chat/completions");
    assert_eq!(forwarded["messages"][0]["content"][1]["image_url"]["url"], data_url);
}

#[tokio::test]
async fn text_model_asset_ids_become_owned_data_urls() {
    let fixture = video_fixture();
    let asset_id = upload_png_id(&fixture.app, &fixture.key).await;
    let response = post_bearer(
        &fixture.app,
        "/v1/chat/completions",
        &fixture.key,
        json!({"model":"vision-model","stream":false,"messages":[{"role":"user","content":"请识别"}],"image_asset_ids":[asset_id]}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let forwarded = fixture.bridge.last_json("/v1/chat/completions");
    assert_eq!(forwarded["messages"][0]["content"][1]["image_url"]["url"], format!("data:image/png;base64,{}", STANDARD.encode(PNG_BYTES)));
    assert!(forwarded.get("image_asset_ids").is_none());
}
