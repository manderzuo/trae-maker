use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequest, BeginRequestInput, ChatExecutionRequest, ChatExecutionResult, ChatExecutor,
    CoreStore, CostPolicy, MockChatExecutor, NewUser, PreflightReserveInput,
    PreflightReserveResult, Principal, QuotaGrant, QuotaReserve, ReservationState,
    ReserveResult, Settlement, UserRole, CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-flow-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_store(grant: i64) -> (CoreStore, String) {
    let (store, key_id, _) = test_store_with_dir(grant);
    (store, key_id)
}

fn test_store_with_dir(grant: i64) -> (CoreStore, String, PathBuf) {
    let dir = test_dir("store");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "u1".into(),
                name: "Test user".into(),
                role: UserRole::User,
            },
            "bootstrap",
        )
        .unwrap();
    let key = store
        .issue_api_key(
            "u1",
            "test",
            BTreeSet::from(["chat:invoke".to_owned()]),
            "bootstrap",
        )
        .unwrap();
    store
        .upsert_cost_policy(CostPolicy {
            id: "chat-policy-v1".into(),
            endpoint: "chat".into(),
            model_pattern: "mock-*".into(),
            resource_kind: "chat_request".into(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .unwrap();
    if grant > 0 {
        store
            .grant(QuotaGrant {
                user_id: "u1".into(),
                resource_kind: "chat_request".into(),
                amount: grant,
                actor_user_id: "u1".into(),
                reason: "test grant".into(),
            })
            .unwrap();
    }
    (store, key.id, dir)
}

fn begin(store: &CoreStore, key_id: &str, idempotency_key: &str) -> BeginRequest {
    store
        .begin_request(BeginRequestInput {
            user_id: "u1".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "mock-1".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model": "mock-1", "messages": []}),
        })
        .unwrap()
}

fn reserve(store: &CoreStore, request_id: &str) -> ReserveResult {
    reserve_amount(store, request_id, 1)
}

fn reserve_amount(store: &CoreStore, request_id: &str, amount: i64) -> ReserveResult {
    store
        .reserve(QuotaReserve {
            user_id: "u1".into(),
            request_id: request_id.into(),
            resource_kind: "chat_request".into(),
            amount,
            ttl_ms: 60_000,
        })
        .unwrap()
}

fn preflight_input(key_id: &str, idempotency_key: &str) -> PreflightReserveInput {
    PreflightReserveInput {
        request: BeginRequestInput {
            user_id: "u1".into(),
            api_key_id: key_id.into(),
            protocol: "openai".into(),
            endpoint: "chat".into(),
            model: "mock-1".into(),
            idempotency_key: idempotency_key.into(),
            body: json!({"model": "mock-1", "messages": []}),
        },
        resource_kind: "chat_request".into(),
        amount: 1,
        ttl_ms: 60_000,
    }
}

fn principal(key_id: &str) -> Principal {
    Principal {
        user_id: "u1".into(),
        key_id: key_id.into(),
        scopes: BTreeSet::from(["chat:invoke".to_owned()]),
    }
}

#[test]
fn mock_chat_flow_reserves_before_execution_and_settles_once() {
    let (store, key_id) = test_store(1);
    let request = match begin(&store, &key_id, "idem-1") {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new request, got {other:?}"),
    };
    let reservation = match reserve(&store, &request.id) {
        ReserveResult::Created(reservation) => reservation,
        other => panic!("expected a new reservation, got {other:?}"),
    };

    let executor = MockChatExecutor::ok();
    let result = executor
        .execute(ChatExecutionRequest {
            request_id: request.id,
            endpoint: request.endpoint,
            model: request.model,
            body: json!({"model": "mock-1", "messages": []}),
        })
        .unwrap();
    assert_eq!(result, ChatExecutionResult::ok());
    store
        .settle(
            &principal(&key_id),
            &reservation.id,
            Settlement::Commit {
                actual_amount: result.actual_amount,
            },
        )
        .unwrap();

    assert_eq!(executor.calls().len(), 1);
    assert_eq!(store.balance("u1", "chat_request").unwrap().available, 0);
    assert_eq!(store.balance("u1", "chat_request").unwrap().held, 0);
}

#[test]
fn insufficient_budget_is_rejected_before_executor_call() {
    let (store, key_id) = test_store(1);
    let request = match begin(&store, &key_id, "idem-insufficient") {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new request, got {other:?}"),
    };
    assert!(matches!(
        reserve_amount(&store, &request.id, 2),
        ReserveResult::Insufficient { available: 1 }
    ));
    let executor = MockChatExecutor::ok();
    assert!(executor.calls().is_empty());
}

#[test]
fn same_idempotency_key_reuses_request_and_reservation() {
    let (store, key_id) = test_store(2);
    let first = match begin(&store, &key_id, "idem-repeat") {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new request, got {other:?}"),
    };
    let first_reservation = match reserve(&store, &first.id) {
        ReserveResult::Created(reservation) => reservation,
        other => panic!("expected a new reservation, got {other:?}"),
    };
    let second = match begin(&store, &key_id, "idem-repeat") {
        BeginRequest::Existing(request) => request,
        other => panic!("expected the existing request, got {other:?}"),
    };
    let second_reservation = match reserve(&store, &second.id) {
        ReserveResult::Existing(reservation) => reservation,
        other => panic!("expected the existing reservation, got {other:?}"),
    };

    assert_eq!(first.id, second.id);
    assert_eq!(first_reservation.id, second_reservation.id);
    assert_eq!(store.balance("u1", "chat_request").unwrap().held, 1);
}

#[test]
fn uncertain_upstream_keeps_reservation_unknown() {
    let (store, key_id) = test_store(1);
    let request = match begin(&store, &key_id, "idem-unknown") {
        BeginRequest::Created(request) => request,
        other => panic!("expected a new request, got {other:?}"),
    };
    let reservation = match reserve(&store, &request.id) {
        ReserveResult::Created(reservation) => reservation,
        other => panic!("expected a new reservation, got {other:?}"),
    };
    store
        .settle(&principal(&key_id), &reservation.id, Settlement::Unknown)
        .unwrap();

    assert_eq!(store.balance("u1", "chat_request").unwrap().held, 1);
    assert_eq!(reservation.state, ReservationState::Held);
}

#[test]
fn atomic_preflight_insufficient_quota_leaves_no_orphan_and_can_retry() {
    let (store, key_id, dir) = test_store_with_dir(0);
    let input = preflight_input(&key_id, "idem-atomic-insufficient");

    assert!(matches!(
        store.preflight_reserve(input),
        Ok(PreflightReserveResult::Insufficient {
            available: 0,
            required: 1,
        })
    ));

    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    for table in ["requests", "idempotency_keys", "quota_reservations"] {
        let count: i64 = connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0, "atomic preflight left rows in {table}");
    }

    store
        .grant(QuotaGrant {
            user_id: "u1".into(),
            resource_kind: "chat_request".into(),
            amount: 1,
            actor_user_id: "u1".into(),
            reason: "retry grant".into(),
        })
        .unwrap();
    let retry = store.preflight_reserve(preflight_input(&key_id, "idem-atomic-insufficient"));
    assert!(matches!(retry, Ok(PreflightReserveResult::Created { .. })));
}
