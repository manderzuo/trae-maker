use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    canonical_json_hash, BeginRequest, BeginRequestInput, CoreError, CoreStore, CostPolicy,
    KeyQuotaGrant, NewUser, PreflightReserveInput, Principal, RequestResult, RequestState,
    Settlement, UserRole,
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

fn dual_store() -> (Arc<CoreStore>, PathBuf, String, String, Principal) {
    let dir = test_dir("dual-layer");
    let store = Arc::new(CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_user(user("admin-1", UserRole::Admin), "bootstrap").unwrap();
    store.create_user(user("u1", UserRole::User), "admin-1").unwrap();
    let key_a = store
        .issue_api_key("u1", "key-a", BTreeSet::new(), "admin-1")
        .unwrap();
    let key_b = store
        .issue_api_key("u1", "key-b", BTreeSet::new(), "admin-1")
        .unwrap();
    let admin_key = store
        .issue_api_key("admin-1", "admin", BTreeSet::new(), "admin-1")
        .unwrap();
    let admin = Principal {
        user_id: "admin-1".into(),
        key_id: admin_key.id,
        scopes: BTreeSet::new(),
    };
    for key in [&key_a, &key_b] {
        store
            .key_quota_grant_as_admin(
                &admin,
                KeyQuotaGrant {
                    api_key_id: key.id.clone(),
                    resource_kind: "chat_request".into(),
                    amount: 5,
                    actor_user_id: "admin-1".into(),
                    reason: "dual-layer fixture".into(),
                },
            )
            .unwrap();
    }
    (store, dir, key_a.id, key_b.id, admin)
}

fn insert_user_cap(dir: &PathBuf, amount: i64) {
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version,
              migration_state, created_at_ms, updated_at_ms)
             VALUES ('user-cap-1', 'user_cap', 'u1', NULL, 'chat_request', 1, 1, 'ready', 1, 1)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO quota_ledger
             (entry_id, user_id, resource_kind, event_kind, amount, delta, actor_user_id,
              reason, created_at_ms, budget_account_id, event_group_id, budget_version)
             VALUES ('user-cap-grant', 'u1', 'chat_request', 'adjust', ?1, ?1, 'admin-1',
                     'dual-layer fixture', 1, 'user-cap-1', 'user-cap-grant', 1)",
            [amount],
        )
        .unwrap();
}

fn dual_input(key_id: &str, idempotency_key: &str) -> PreflightReserveInput {
    PreflightReserveInput {
        request: input(
            "u1",
            key_id,
            "chat",
            idempotency_key,
            json!({"model": "mock-1", "messages": []}),
        ),
        resource_kind: "chat_request".into(),
        amount: 4,
        ttl_ms: 60_000,
    }
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

#[test]
fn dual_layer_reservation_enforces_user_cap_across_keys() {
    let (store, dir, key_a, key_b, _admin) = dual_store();
    insert_user_cap(&dir, 6);
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (key_id, idempotency_key) in [(&key_a, "dual-a"), (&key_b, "dual-b")] {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        let key_id = key_id.clone();
        let idempotency_key = idempotency_key.to_owned();
        handles.push(thread::spawn(move || {
            barrier.wait();
            store.preflight_reserve(dual_input(&key_id, &idempotency_key))
        }));
    }
    barrier.wait();

    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, aiwork_core::PreflightReserveResult::Created { .. }))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, aiwork_core::PreflightReserveResult::Insufficient { .. }))
            .count(),
        1
    );
    let created = results
        .iter()
        .find_map(|result| match result {
            aiwork_core::PreflightReserveResult::Created { reservation, .. } => Some(reservation),
            _ => None,
        })
        .expect("one request must reserve both budget layers");
    assert!(created.event_group_id.is_some());
    assert!(created.key_budget_account_id.is_some());
    assert_eq!(created.user_cap_account_id.as_deref(), Some("user-cap-1"));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (requests, reservations, user_cap_held, key_held): (i64, i64, i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM requests),
                (SELECT COUNT(*) FROM quota_reservations),
                (SELECT COALESCE(SUM(amount), 0) FROM quota_reservations
                 WHERE user_cap_account_id = 'user-cap-1' AND state = 'held'),
                (SELECT COALESCE(SUM(amount), 0) FROM quota_reservations
                 WHERE key_budget_account_id IS NOT NULL AND state = 'held')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(requests, 1);
    assert_eq!(reservations, 1);
    assert_eq!(user_cap_held, 4);
    assert_eq!(key_held, 4);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dual_layer_without_user_cap_keeps_key_balances_independent() {
    let (store, dir, key_a, key_b, admin) = dual_store();
    let first = store
        .preflight_reserve(dual_input(&key_a, "key-only-a"))
        .unwrap();
    let second = store
        .preflight_reserve(dual_input(&key_b, "key-only-b"))
        .unwrap();
    let reservation_a = match first {
        aiwork_core::PreflightReserveResult::Created { reservation, .. } => reservation,
        other => panic!("expected key A reservation, got {other:?}"),
    };
    let reservation_b = match second {
        aiwork_core::PreflightReserveResult::Created { reservation, .. } => reservation,
        other => panic!("expected key B reservation, got {other:?}"),
    };
    assert!(reservation_a.user_cap_account_id.is_none());
    assert!(reservation_b.user_cap_account_id.is_none());

    let owner_a = Principal {
        user_id: "u1".into(),
        key_id: key_a.clone(),
        scopes: BTreeSet::new(),
    };
    let owner_b = Principal {
        user_id: "u1".into(),
        key_id: key_b.clone(),
        scopes: BTreeSet::new(),
    };
    store
        .settle(&owner_a, &reservation_a.id, Settlement::Release)
        .unwrap();
    store
        .settle(&owner_a, &reservation_a.id, Settlement::Release)
        .unwrap();
    store
        .settle(
            &owner_b,
            &reservation_b.id,
            Settlement::Commit {
                actual_amount: Some(2),
            },
        )
        .unwrap();
    store
        .settle(
            &owner_b,
            &reservation_b.id,
            Settlement::Commit {
                actual_amount: Some(2),
            },
        )
        .unwrap();

    let unknown = match store
        .preflight_reserve(dual_input(&key_a, "key-only-unknown"))
        .unwrap()
    {
        aiwork_core::PreflightReserveResult::Created { reservation, .. } => reservation,
        other => panic!("expected unknown reservation, got {other:?}"),
    };
    store
        .settle(&owner_a, &unknown.id, Settlement::Unknown)
        .unwrap();
    store
        .settle(&owner_a, &unknown.id, Settlement::Unknown)
        .unwrap();

    let key_a_balance = store
        .key_quota_balance_as_admin(&admin, &key_a, "chat_request")
        .unwrap();
    let key_b_balance = store
        .key_quota_balance_as_admin(&admin, &key_b, "chat_request")
        .unwrap();
    assert_eq!(key_a_balance.available, 1);
    assert_eq!(key_a_balance.held, 4);
    assert_eq!(key_a_balance.settled, 0);
    assert_eq!(key_b_balance.available, 3);
    assert_eq!(key_b_balance.held, 0);
    assert_eq!(key_b_balance.settled, 2);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (release_events, commit_events): (i64, i64) = connection
        .query_row(
            "SELECT
                (SELECT COUNT(*) FROM quota_ledger WHERE event_group_id = ?1),
                (SELECT COUNT(*) FROM quota_ledger WHERE event_group_id = ?2)",
            [&reservation_a.event_group_id.clone().unwrap(), &reservation_b.event_group_id.clone().unwrap()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(release_events, 2);
    assert_eq!(commit_events, 2);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dual_layer_idempotency_replays_without_a_second_reservation() {
    let (store, dir, key_a, _key_b, _admin) = dual_store();
    let first = store
        .preflight_reserve(dual_input(&key_a, "same-dual-request"))
        .unwrap();
    let first_id = match first {
        aiwork_core::PreflightReserveResult::Created { request, reservation } => {
            (request.id, reservation.id)
        }
        other => panic!("expected created request, got {other:?}"),
    };
    let replay = store
        .preflight_reserve(dual_input(&key_a, "same-dual-request"))
        .unwrap();
    match replay {
        aiwork_core::PreflightReserveResult::Existing { request, reservation } => {
            assert_eq!(request.id, first_id.0);
            assert_eq!(reservation.unwrap().id, first_id.1);
        }
        other => panic!("expected replay, got {other:?}"),
    }
    let mut conflict = dual_input(&key_a, "same-dual-request");
    conflict.request.body = json!({"model": "mock-1", "messages": [{"role": "user", "content": "different"}]});
    assert!(matches!(
        store.preflight_reserve(conflict),
        Ok(aiwork_core::PreflightReserveResult::Conflict)
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (request_count, reservation_count, ledger_count): (i64, i64, i64) = connection
        .query_row(
            "SELECT (SELECT COUNT(*) FROM requests),
                    (SELECT COUNT(*) FROM quota_reservations),
                    (SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1)",
            [&first_id.0],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(request_count, 1);
    assert_eq!(reservation_count, 1);
    assert_eq!(ledger_count, 1);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
