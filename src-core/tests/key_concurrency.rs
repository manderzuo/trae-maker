use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequestInput, CoreError, CoreStore, NewUser, PreflightReserveInput,
    PreflightReserveResult, RequestResult, RequestState, Settlement, UserRole,
};
use serde_json::json;

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiwork-key-concurrency-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn request(key_id: &str, idempotency_key: &str) -> PreflightReserveInput {
    PreflightReserveInput {
        request: BeginRequestInput {
            user_id: "user-1".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "deepseek-v4-flash".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model":"deepseek-v4-flash","messages":[]} ),
        },
        resource_kind: "credits".into(),
        amount: 1,
        ttl_ms: 120_000,
    }
}

#[test]
fn api_key_concurrency_limit_is_enforced_until_request_settlement() {
    let dir = test_dir();
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin }, "bootstrap").unwrap();
    store.create_user(NewUser { id: "user-1".into(), name: "User".into(), role: UserRole::User }, "admin").unwrap();
    let admin_key = store.issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap").unwrap();
    let admin_principal = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let user_key = store.issue_api_key_as_admin_with_max_concurrency(
        "user-1", "user", BTreeSet::from(["chat:invoke".into()]), 1,
        &admin_principal,
    ).unwrap();
    store.key_quota_grant_as_admin(&admin_principal, aiwork_core::KeyQuotaGrant { api_key_id: user_key.id.clone(), resource_kind: "credits".into(), amount: 5, actor_user_id: "admin".into(), reason: "test".into() }).unwrap();

    let first = store.preflight_reserve(request(&user_key.id, "req-1")).unwrap();
    let (_first_handle, first_reservation_id) = match first { PreflightReserveResult::Created { request, reservation } => (request, reservation.id), other => panic!("expected first request, got {other:?}") };
    let second = store.preflight_reserve(request(&user_key.id, "req-2"));
    assert!(matches!(second, Err(CoreError::KeyConcurrencyExceeded { max_concurrency: 1, .. })));

    let principal = store.authenticate_api_key(&user_key.plaintext).unwrap();
    store.settle_request(&principal, &first_reservation_id, Settlement::Release, RequestState::Failed, Some(RequestResult { status: Some(502), error_code: Some("test".into()) })).unwrap();
    assert!(matches!(store.preflight_reserve(request(&user_key.id, "req-3")), Ok(PreflightReserveResult::Created { .. })));
    let _ = fs::remove_dir_all(dir);
}
