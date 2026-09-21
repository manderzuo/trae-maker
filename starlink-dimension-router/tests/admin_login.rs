use std::{fs, path::PathBuf, sync::Arc};

use axum::{body::{to_bytes, Body}, http::{Request, Response, StatusCode}, Router};
use serde_json::{json, Value};
use starlink_dimension_router::{admin_session::hash_password, bridge_client::BridgeClient, config::RouterConfig, server::build_router, state::StarlinkRouterState};
use tower::util::ServiceExt;

fn test_dir(label: &str) -> PathBuf {
    let dir = PathBuf::from(format!(r"D:\gpt\starlink-admin-login-test-{label}-{}", rand::random::<u64>()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn test_app_with_admin_password(password: &str) -> Router {
    let dir = test_dir("app");
    let store = Arc::new(aiwork_core::CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(aiwork_core::NewUser { id: "admin".into(), name: "系统管理员".into(), role: aiwork_core::UserRole::Admin }, "bootstrap").unwrap();
    let hash = hash_password(password).unwrap();
    store.upsert_admin_credential(aiwork_core::NewAdminCredential { user_id: "admin".into(), username: "admin".into(), password_hash: hash.hash, salt: hash.salt, iterations: hash.iterations, must_change_password: false }).unwrap();
    let config = RouterConfig::defaults(dir);
    let state = StarlinkRouterState::for_test(store, BridgeClient::new("", ""), config);
    build_router(state)
}

async fn post_json(app: &Router, path: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(Request::post(path).header("content-type", "application/json").body(Body::from(value.to_string())).unwrap()).await.unwrap()
}

async fn post_with_cookie(app: &Router, path: &str, cookie: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(Request::post(path).header("content-type", "application/json").header("cookie", cookie).body(Body::from(value.to_string())).unwrap()).await.unwrap()
}

async fn patch_with_cookie(app: &Router, path: &str, cookie: &str, value: Value) -> Response<Body> {
    app.clone().oneshot(Request::patch(path).header("content-type", "application/json").header("cookie", cookie).body(Body::from(value.to_string())).unwrap()).await.unwrap()
}

async fn get(app: &Router, path: &str) -> Response<Body> {
    app.clone().oneshot(Request::get(path).body(Body::empty()).unwrap()).await.unwrap()
}

async fn get_with_cookie(app: &Router, path: &str, cookie: &str) -> Response<Body> {
    app.clone().oneshot(Request::get(path).header("cookie", cookie).body(Body::empty()).unwrap()).await.unwrap()
}

async fn response_error_type(response: Response<Body>) -> String {
    let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["type"].as_str().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn admin_login_sets_cookie_and_session_reads_summary() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    assert_eq!(login.status(), StatusCode::OK);
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    assert!(cookie.contains("HttpOnly"));
    assert!(cookie.contains("SameSite=Strict"));
    let session = get_with_cookie(&app, "/admin/v1/session", &cookie).await;
    assert_eq!(session.status(), StatusCode::OK);
    let summary = get_with_cookie(&app, "/admin/v1/summary", &cookie).await;
    assert_eq!(summary.status(), StatusCode::OK);
    let summary_body = to_bytes(summary.into_body(), usize::MAX).await.unwrap();
    let summary_json: Value = serde_json::from_slice(&summary_body).unwrap();
    assert_eq!(summary_json["upstream"]["upstream_credits"]["fresh"], false);
    assert_eq!(summary_json["upstream"]["upstream_credits"]["error_code"], "bridge_summary_unavailable");
    let trend = get_with_cookie(&app, "/admin/v1/usage-trend?window=24h", &cookie).await;
    assert_eq!(trend.status(), StatusCode::OK);
    let trend_body = to_bytes(trend.into_body(), usize::MAX).await.unwrap();
    let trend_json: Value = serde_json::from_slice(&trend_body).unwrap();
    assert_eq!(trend_json["window"], "24h");
    assert_eq!(trend_json["points"].as_array().map(Vec::len), Some(24));
}

#[tokio::test]
async fn wrong_password_and_missing_session_are_unauthorized() {
    let app = test_app_with_admin_password("test-password");
    let wrong_password = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"wrong"})).await;
    let wrong_username = post_json(&app, "/admin/v1/login", json!({"username":"missing","password":"wrong"})).await;
    let missing_cookie = get(&app, "/admin/v1/summary").await;
    assert_eq!(wrong_password.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_username.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(missing_cookie.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(response_error_type(wrong_password).await, response_error_type(wrong_username).await);
}

#[tokio::test]
async fn logout_and_password_change_invalidate_old_sessions() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    assert_eq!(post_with_cookie(&app, "/admin/v1/logout", &cookie, json!({})).await.status(), StatusCode::NO_CONTENT);
    assert_eq!(get_with_cookie(&app, "/admin/v1/summary", &cookie).await.status(), StatusCode::UNAUTHORIZED);
    let login2 = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie2 = login2.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    assert_eq!(post_with_cookie(&app, "/admin/v1/password", &cookie2, json!({"current_password":"test-password","new_password":"new-test-password"})).await.status(), StatusCode::OK);
    assert_eq!(post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(get_with_cookie(&app, "/admin/v1/summary", &cookie2).await.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn admin_can_create_and_manage_key_without_precreating_user() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();

    let created = post_with_cookie(&app, "/admin/v1/api-keys", &cookie, json!({
        "display_name": "视频工作室",
        "scopes": ["chat:invoke", "videos:submit", "assets:write"],
        "max_concurrency": 3
    })).await;
    assert_eq!(created.status(), StatusCode::OK);
    let created_body: Value = serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap()).unwrap();
    let user_id = created_body["user_id"].as_str().unwrap().to_owned();
    let key_id = created_body["id"].as_str().unwrap().to_owned();
    assert!(!user_id.is_empty());
    assert!(created_body["plaintext"].as_str().unwrap().len() > 10);
    assert!(created_body["scopes"].as_array().unwrap().iter().any(|scope| scope == "assets:write"));

    let users = get_with_cookie(&app, "/admin/v1/users", &cookie).await;
    let users_json: Value = serde_json::from_slice(&to_bytes(users.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert!(users_json.as_array().unwrap().iter().any(|user| user["id"] == user_id && user["name"] == "视频工作室"));

    let keys = get_with_cookie(&app, "/admin/v1/api-keys", &cookie).await;
    assert_eq!(keys.status(), StatusCode::OK);
    let keys_json: Value = serde_json::from_slice(&to_bytes(keys.into_body(), usize::MAX).await.unwrap()).unwrap();
    let key_view = keys_json.as_array().unwrap().iter().find(|key| key["id"] == key_id).unwrap();
    assert_eq!(key_view["user_name"], "视频工作室");
    assert_eq!(key_view["max_concurrency"], 3);
    assert_eq!(key_view["status"], "active");

    let quota = post_with_cookie(&app, "/admin/v1/quota/grant", &cookie, json!({
        "user_id": user_id,
        "resource_kind": "credits",
        "amount": 100,
        "reason": "test grant"
    })).await;
    assert_eq!(quota.status(), StatusCode::OK);

    let updated = patch_with_cookie(&app, &format!("/admin/v1/api-keys/{key_id}"), &cookie, json!({"enabled": false, "max_concurrency": 5})).await;
    assert_eq!(updated.status(), StatusCode::OK);
    let updated_body: Value = serde_json::from_slice(&to_bytes(updated.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_eq!(updated_body["status"], "revoked");
    assert_eq!(updated_body["max_concurrency"], 5);

    let rotated = post_with_cookie(&app, &format!("/admin/v1/api-keys/{key_id}/rotate"), &cookie, json!({})).await;
    assert_eq!(rotated.status(), StatusCode::OK);
    let rotated_body: Value = serde_json::from_slice(&to_bytes(rotated.into_body(), usize::MAX).await.unwrap()).unwrap();
    assert_ne!(rotated_body["id"], key_id);
    assert!(rotated_body["plaintext"].as_str().unwrap().len() > 10);
}

#[tokio::test]
async fn admin_allocates_from_pool_and_cannot_overallocate_a_key() {
    let app = test_app_with_admin_password("test-password");
    let login = post_json(&app, "/admin/v1/login", json!({"username":"admin","password":"test-password"})).await;
    let cookie = login.headers().get("set-cookie").unwrap().to_str().unwrap().to_owned();
    let created = post_with_cookie(&app, "/admin/v1/api-keys", &cookie, json!({
        "display_name": "池分配测试",
        "scopes": ["chat:invoke", "videos:submit"],
        "max_concurrency": 2
    })).await;
    assert_eq!(created.status(), StatusCode::OK);
    let created_body: Value = serde_json::from_slice(&to_bytes(created.into_body(), usize::MAX).await.unwrap()).unwrap();
    let user_id = created_body["user_id"].as_str().unwrap();
    let key_id = created_body["id"].as_str().unwrap();

    let pool = post_with_cookie(&app, "/admin/v1/quota/pool", &cookie, json!({
        "user_id": user_id,
        "resource_kind": "credits",
        "amount": 100,
        "reason": "pool top up"
    })).await;
    assert_eq!(pool.status(), StatusCode::OK);
    let allocation = post_with_cookie(&app, &format!("/admin/v1/api-keys/{key_id}/quota"), &cookie, json!({
        "resource_kind": "credits",
        "amount": 70,
        "reason": "key allocation"
    })).await;
    assert_eq!(allocation.status(), StatusCode::OK);

    let too_much = post_with_cookie(&app, &format!("/admin/v1/api-keys/{key_id}/quota"), &cookie, json!({
        "resource_kind": "credits",
        "amount": 31,
        "reason": "must be rejected"
    })).await;
    assert_eq!(too_much.status(), StatusCode::CONFLICT);
    assert_eq!(response_error_type(too_much).await, "quota_pool_insufficient");

    let keys = get_with_cookie(&app, "/admin/v1/api-keys", &cookie).await;
    let keys_json: Value = serde_json::from_slice(&to_bytes(keys.into_body(), usize::MAX).await.unwrap()).unwrap();
    let key_view = keys_json.as_array().unwrap().iter().find(|key| key["id"] == key_id).unwrap();
    assert_eq!(key_view["key_quota"][0]["available"], 70);
    assert_eq!(key_view["pool_allocatable"][0]["available"], 30);
}
