use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    canonical_json_hash, BeginRequest, BeginRequestInput, CoreError, CoreStore, CostPolicy,
    NewUser, RequestResult, RequestState, UserRole,
};
use rusqlite::Connection;
use serde_json::{json, Value};

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-requests-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn test_store() -> (CoreStore, String, String) {
    let dir = test_dir("store");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(user("admin-1", UserRole::Admin), "bootstrap").unwrap();
    store.create_user(user("u1", UserRole::User), "admin-1").unwrap();
    store.create_user(user("u2", UserRole::User), "admin-1").unwrap();
    let first_key = store
        .issue_api_key("u1", "test", BTreeSet::new(), "admin-1")
        .unwrap();
    let second_key = store
        .issue_api_key("u2", "test", BTreeSet::new(), "admin-1")
        .unwrap();
    (store, first_key.id, second_key.id)
}

fn input(user_id: &str, key_id: &str, endpoint: &str, idempotency_key: &str, body: Value) -> BeginRequestInput {
    BeginRequestInput {
        user_id: user_id.to_owned(),
        api_key_id: key_id.to_owned(),
        protocol: "openai".to_owned(),
        endpoint: endpoint.to_owned(),
        model: "mock-1".to_owned(),
        idempotency_key: idempotency_key.to_owned(),
        body,
    }
}

fn install_chat_policy(store: &CoreStore, endpoint: &str) {
    store
        .upsert_cost_policy(CostPolicy {
            id: format!("policy-{}", endpoint.replace('/', "_")),
            endpoint: endpoint.to_owned(),
            model_pattern: "mock-*".to_owned(),
            resource_kind: "chat_request".to_owned(),
            reserve_amount: 10,
            max_actual_amount: Some(10),
            version: 1,
            enabled: true,
        })
        .unwrap();
}

#[test]
fn canonical_hash_sorts_object_keys_but_preserves_values() {
    let first = json!({"model":"mock-1","messages":[{"role":"user","content":"hi"}]});
    let reordered = json!({"messages":[{"content":"hi","role":"user"}],"model":"mock-1"});
    let different = json!({"model":"mock-1","messages":[{"role":"user","content":"bye"}]});

    assert_eq!(canonical_json_hash(&first), canonical_json_hash(&reordered));
    assert_ne!(canonical_json_hash(&first), canonical_json_hash(&different));
}

#[test]
fn canonical_hash_sorts_nested_objects_but_preserves_array_order() {
    let first = json!({"items":[{"outer":{"b":2,"a":1},"first":"first"}]});
    let reordered_object = json!({"items":[{"first":"first","outer":{"a":1,"b":2}}]});
    let reordered_array = json!({"items":["first", {"outer":{"a":1,"b":2}}]});

    assert_eq!(canonical_json_hash(&first), canonical_json_hash(&reordered_object));
    assert_ne!(canonical_json_hash(&first), canonical_json_hash(&reordered_array));
}

#[test]
fn idempotency_is_user_and_endpoint_scoped() {
    let (store, first_key, second_key) = test_store();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);
    install_chat_policy(&store, "/v1/responses");
    let a = json!({"model":"mock-1","messages":[{"role":"user","content":"hi"}]});
    let b = json!({"messages":[{"content":"hi","role":"user"}],"model":"mock-1"});

    let first = store.begin_request(input("u1", &first_key, endpoint, "same", a)).unwrap();
    let second = store.begin_request(input("u1", &first_key, endpoint, "same", b)).unwrap();
    let other_user = store.begin_request(input("u2", &second_key, endpoint, "same", json!({"model":"mock-1"}))).unwrap();
    let other_endpoint = store.begin_request(input("u1", &first_key, "/v1/responses", "same", json!({"model":"mock-1"}))).unwrap();

    assert!(matches!(first, BeginRequest::Created(_)));
    assert!(matches!(second, BeginRequest::Existing(_)));
    assert!(matches!(other_user, BeginRequest::Created(_)));
    assert!(matches!(other_endpoint, BeginRequest::Created(_)));
}

#[test]
fn same_idempotency_key_with_different_request_hash_conflicts() {
    let (store, first_key, _) = test_store();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);

    let first = store.begin_request(input("u1", &first_key, endpoint, "same", json!({"prompt":"first"}))).unwrap();
    let second = store.begin_request(input("u1", &first_key, endpoint, "same", json!({"prompt":"second"}))).unwrap();

    assert!(matches!(first, BeginRequest::Created(_)));
    assert!(matches!(second, BeginRequest::Conflict));
}

#[test]
fn begin_request_requires_an_enabled_matching_cost_policy() {
    let (store, first_key, _) = test_store();

    let error = store
        .begin_request(input("u1", &first_key, "/v1/chat/completions", "same", json!({"model":"mock-1"})))
        .expect_err("a missing policy must not fall back to a default cost");

    assert!(matches!(error, CoreError::BudgetPolicyMissing { .. }));
}

#[test]
fn begin_request_rejects_an_api_key_owned_by_a_different_user() {
    let (store, _, second_key) = test_store();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);

    let error = store
        .begin_request(input("u1", &second_key, endpoint, "cross-user", json!({"model":"mock-1"})))
        .expect_err("a key cannot be used as another user's identity");

    assert!(matches!(error, CoreError::InvalidRequestIdentity { .. }));
}

#[test]
fn begin_request_rejects_a_revoked_api_key() {
    let (store, first_key, _) = test_store();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);
    store.revoke_api_key(&first_key, "admin-1").unwrap();

    let error = store
        .begin_request(input("u1", &first_key, endpoint, "revoked", json!({"model":"mock-1"})))
        .expect_err("a revoked key cannot create a request");

    assert!(matches!(error, CoreError::InvalidRequestIdentity { .. }));
}

#[test]
fn transitions_require_the_expected_state_and_never_move_backward() {
    let (store, first_key, _) = test_store();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);
    let request = match store
        .begin_request(input("u1", &first_key, endpoint, "same", json!({"prompt":"private prompt"})))
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a newly created request, got {other:?}"),
    };

    store
        .transition_request(&request.id, RequestState::Received, RequestState::Validating, None)
        .unwrap();
    let repeated = store.transition_request(
        &request.id,
        RequestState::Received,
        RequestState::Validating,
        None,
    );
    let backward = store.transition_request(
        &request.id,
        RequestState::Validating,
        RequestState::Received,
        None,
    );

    assert!(matches!(repeated, Err(CoreError::InvalidTransition { .. })));
    assert!(matches!(backward, Err(CoreError::InvalidTransition { .. })));

    store
        .transition_request(
            &request.id,
            RequestState::Validating,
            RequestState::Failed,
            Some(RequestResult {
                status: Some(400),
                error_code: Some("invalid_request".to_owned()),
            }),
        )
        .unwrap();
}

#[test]
fn request_records_store_metadata_but_not_the_full_prompt_or_output() {
    let dir = test_dir("privacy");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(user("admin-1", UserRole::Admin), "bootstrap").unwrap();
    store.create_user(user("u1", UserRole::User), "admin-1").unwrap();
    let key = store
        .issue_api_key("u1", "test", BTreeSet::new(), "admin-1")
        .unwrap();
    let endpoint = "/v1/chat/completions";
    install_chat_policy(&store, endpoint);
    let secret_prompt = "private prompt that must not be persisted";
    let request = match store
        .begin_request(input(
            "u1",
            &key.id,
            endpoint,
            "privacy",
            json!({"prompt": secret_prompt}),
        ))
        .unwrap()
    {
        BeginRequest::Created(request) => request,
        other => panic!("expected a newly created request, got {other:?}"),
    };
    store
        .transition_request(
            &request.id,
            RequestState::Received,
            RequestState::Validating,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &request.id,
            RequestState::Validating,
            RequestState::Failed,
            Some(RequestResult {
                status: Some(400),
                error_code: Some("invalid_request".to_owned()),
            }),
        )
        .unwrap();

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let schema: String = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'requests'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let stored_prompt: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM requests WHERE \
             instr(id, ?1) > 0 OR instr(user_id, ?1) > 0 OR instr(api_key_id, ?1) > 0 OR \
             instr(protocol, ?1) > 0 OR instr(endpoint, ?1) > 0 OR instr(model, ?1) > 0 OR \
             instr(error_code, ?1) > 0)",
            [secret_prompt],
            |row| row.get(0),
        )
        .unwrap();

    assert!(!schema.contains("prompt"));
    assert!(!schema.contains("output"));
    assert!(!stored_prompt);
}
