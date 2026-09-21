use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::Arc,
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
    bridge_client::BridgeClient,
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
