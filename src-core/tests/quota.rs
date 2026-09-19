use std::{
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, LeaseState, NewUser, ObservationStatus, Principal,
    PreflightReserveInput, QuotaGrant, QuotaReserve, RegisterUpstreamAccount, ReserveResult,
    SchedulerLeaseRequest, SchedulerLeaseResult, SelectionStrategy, Settlement, UpstreamAccountState,
    UpstreamObservation, UserRole, CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

const SCHEDULER_NOW_MS: i64 = 1_725_000_100_000;

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-quota-{prefix}-{}",
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

fn test_store_with_grant(amount: i64) -> (Arc<CoreStore>, PathBuf) {
    let dir = test_dir("store");
    let store = Arc::new(CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_user(user("admin-1", UserRole::Admin), "bootstrap").unwrap();
    store.create_user(user("u1", UserRole::User), "admin-1").unwrap();
    store
        .grant(QuotaGrant {
            user_id: "u1".to_owned(),
            resource_kind: "chat_request".to_owned(),
            amount,
            actor_user_id: "admin-1".to_owned(),
            reason: "initial allocation".to_owned(),
        })
        .unwrap();
    (store, dir)
}

fn reserve(request_id: &str, amount: i64, ttl_ms: i64) -> QuotaReserve {
    reserve_for("u1", request_id, "chat_request", amount, ttl_ms)
}

fn reserve_for(
    user_id: &str,
    request_id: &str,
    resource_kind: &str,
    amount: i64,
    ttl_ms: i64,
) -> QuotaReserve {
    QuotaReserve {
        user_id: user_id.to_owned(),
        request_id: request_id.to_owned(),
        resource_kind: resource_kind.to_owned(),
        amount,
        ttl_ms,
    }
}

fn principal(user_id: &str) -> Principal {
    Principal {
        user_id: user_id.to_owned(),
        key_id: "test-key".to_owned(),
        scopes: Default::default(),
    }
}

fn created(store: &CoreStore, request_id: &str, amount: i64) -> String {
    created_with_ttl(store, request_id, amount, 60_000)
}

fn created_with_ttl(store: &CoreStore, request_id: &str, amount: i64, ttl_ms: i64) -> String {
    match store.reserve(reserve(request_id, amount, ttl_ms)).unwrap() {
        ReserveResult::Created(reservation) => reservation.id,
        other => panic!("expected created reservation, got {other:?}"),
    }
}

#[test]
fn concurrent_reservations_never_overdraw() {
    let (store, _) = test_store_with_grant(100);
    let barrier = Arc::new(Barrier::new(32));
    let mut handles = Vec::new();

    for index in 0..32 {
        let store = Arc::clone(&store);
        let barrier = Arc::clone(&barrier);
        handles.push(thread::spawn(move || {
            barrier.wait();
            store.reserve(reserve(&format!("request-{index}"), 10, 60_000))
        }));
    }

    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap().unwrap()).collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, ReserveResult::Created(_)))
            .count(),
        10
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, ReserveResult::Insufficient { available: 0 }))
            .count(),
        22
    );

    let balance = store.balance("u1", "chat_request").unwrap();
    assert_eq!(balance.available, 0);
    assert_eq!(balance.held, 100);
}

#[test]
fn duplicate_request_id_returns_its_original_reservation_without_a_second_hold() {
    let (store, _) = test_store_with_grant(100);

    let first = store.reserve(reserve("same-request", 10, 60_000)).unwrap();
    let second = store.reserve(reserve("same-request", 10, 60_000)).unwrap();

    let first_id = match first {
        ReserveResult::Created(reservation) => reservation.id,
        other => panic!("expected created reservation, got {other:?}"),
    };
    match second {
        ReserveResult::Existing(reservation) => assert_eq!(reservation.id, first_id),
        other => panic!("expected existing reservation, got {other:?}"),
    }
    assert_eq!(store.balance("u1", "chat_request").unwrap().available, 90);
}

#[test]
fn duplicate_request_id_from_another_owner_or_resource_is_rejected_without_disclosure() {
    let (store, _) = test_store_with_grant(100);
    let reservation_id = created(&store, "conflicting-request", 10);

    for conflicting_input in [
        reserve_for("u2", "conflicting-request", "chat_request", 10, 60_000),
        reserve_for("u1", "conflicting-request", "image_request", 10, 60_000),
    ] {
        let error = store.reserve(conflicting_input).expect_err("conflicting request must not return a reservation");
        assert!(error.to_string().contains("quota reservation request id conflict"));
    }

    assert_eq!(store.balance("u1", "chat_request").unwrap().available, 90);
    assert_eq!(
        store.settle(&principal("u1"), &reservation_id, Settlement::Release).unwrap().available,
        100
    );
}

#[test]
fn release_is_idempotent_and_restores_the_full_hold_once() {
    let (store, _) = test_store_with_grant(100);
    let reservation_id = created(&store, "release-request", 10);

    assert_eq!(
        store.settle(&principal("u1"), &reservation_id, Settlement::Release).unwrap().available,
        100
    );
    assert_eq!(
        store.settle(&principal("u1"), &reservation_id, Settlement::Release).unwrap().available,
        100
    );
}

#[test]
fn commit_is_idempotent_and_releases_only_the_unused_hold() {
    let (store, _) = test_store_with_grant(100);
    let reservation_id = created(&store, "commit-request", 10);

    assert_eq!(
        store
            .settle(
                &principal("u1"),
                &reservation_id,
                Settlement::Commit {
                    actual_amount: Some(7),
                },
            )
            .unwrap()
            .available,
        93
    );
    assert_eq!(
        store
            .settle(
                &principal("u1"),
                &reservation_id,
                Settlement::Commit {
                    actual_amount: Some(7),
                },
            )
            .unwrap()
            .available,
        93
    );
}

#[test]
fn commit_without_actual_amount_keeps_the_full_hold_and_marks_it_unknown() {
    let (store, dir) = test_store_with_grant(100);
    let reservation_id = created(&store, "unknown-actual-request", 10);

    assert_eq!(
        store
            .settle(
                &principal("u1"),
                &reservation_id,
                Settlement::Commit {
                    actual_amount: None,
                },
            )
            .unwrap()
            .available,
        90
    );
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let reason: String = connection
        .query_row(
            "SELECT reason FROM quota_ledger WHERE request_id = 'unknown-actual-request' AND event_kind = 'commit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reason, "actual_unknown");
}

#[test]
fn unknown_settlement_keeps_the_reservation_held_even_after_its_ttl() {
    let (store, dir) = test_store_with_grant(100);
    let reservation_id = created_with_ttl(&store, "unknown-request", 10, 1);
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let expires_at_ms: i64 = connection
        .query_row(
            "SELECT expires_at_ms FROM quota_reservations WHERE id = ?1",
            [&reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    while std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
        <= expires_at_ms
    {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as i64
            > expires_at_ms
    );

    assert_eq!(
        store.settle(&principal("u1"), &reservation_id, Settlement::Unknown).unwrap().available,
        90
    );
    let balance = store.balance("u1", "chat_request").unwrap();
    assert_eq!(balance.available, 90);
    assert_eq!(balance.held, 10);
    let state: String = connection
        .query_row(
            "SELECT state FROM quota_reservations WHERE id = ?1",
            [&reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state, "unknown");
    assert_eq!(
        store.settle(&principal("u1"), &reservation_id, Settlement::Unknown).unwrap().available,
        90
    );
    let state_after_repeat: String = connection
        .query_row(
            "SELECT state FROM quota_reservations WHERE id = ?1",
            [&reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(state_after_repeat, "unknown");
}

#[test]
fn administrator_adjustment_persists_its_actor_and_reason() {
    let (store, dir) = test_store_with_grant(100);

    let balance = store
        .grant(QuotaGrant {
            user_id: "u1".to_owned(),
            resource_kind: "chat_request".to_owned(),
            amount: -5,
            actor_user_id: "admin-1".to_owned(),
            reason: "manual correction".to_owned(),
        })
        .unwrap();

    assert_eq!(balance.available, 95);
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let row = connection
        .query_row(
            "SELECT event_kind, actor_user_id, reason, delta FROM quota_ledger \
             WHERE user_id = 'u1' AND reason = 'manual correction'",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(row, ("adjust".to_owned(), "admin-1".to_owned(), "manual correction".to_owned(), -5));
}

#[test]
fn scheduler_recovery_after_request_progression_preserves_the_quota_hold() {
    let dir = test_dir("scheduler-recovery");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.create_user(user("admin-1", UserRole::Admin), "bootstrap").unwrap();
    store.create_user(user("u1", UserRole::User), "admin-1").unwrap();
    let key = store.issue_api_key(
        "u1", "scheduler", std::collections::BTreeSet::from(["chat:invoke".to_owned()]), "admin-1",
    ).unwrap();
    let admin_key = store.issue_api_key("admin-1", "scheduler-admin", Default::default(), "admin-1").unwrap();
    store.upsert_cost_policy(CostPolicy {
        id: "scheduler-policy".into(), endpoint: "chat".into(), model_pattern: "mock-*".into(),
        resource_kind: "chat_request".into(), reserve_amount: 3, max_actual_amount: Some(3), version: 1, enabled: true,
    }).unwrap();
    store.grant(QuotaGrant {
        user_id: "u1".into(), resource_kind: "chat_request".into(), amount: 3,
        actor_user_id: "admin-1".into(), reason: "fixed scheduler grant".into(),
    }).unwrap();
    let admin = Principal { user_id: "admin-1".into(), key_id: admin_key.id, scopes: Default::default() };
    let mut account = RegisterUpstreamAccount::new("scheduler-account".into(), "fixture".into(), "vault://quota-fixture".into());
    account.capabilities = std::collections::BTreeSet::from(["chat".to_owned()]);
    account.state = UpstreamAccountState::Available;
    store.upsert_upstream_account(account, &admin).unwrap();
    store.append_upstream_observation(UpstreamObservation::new(
        "scheduler-observation".into(), "scheduler-account".into(), "chat_request".into(), Some(10), 1,
        "reader".into(), ObservationStatus::Fresh, SCHEDULER_NOW_MS, SCHEDULER_NOW_MS + 60_000,
        json!({"available": 10, "value_scale": 1, "source": "reader", "status": "fresh"}),
    )).unwrap();
    let principal = Principal {
        user_id: "u1".into(), key_id: key.id, scopes: std::collections::BTreeSet::from(["chat:invoke".to_owned()]),
    };
    let acquired = store.preflight_reserve_with_lease(&principal, SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: "u1".into(), api_key_id: principal.key_id.clone(), protocol: "openai".into(), endpoint: "chat".into(),
                model: "mock-1".into(), idempotency_key: "quota-progressed-recovery".into(), body: json!({"model": "mock-1"}),
            }, resource_kind: "chat_request".into(), amount: 3, ttl_ms: 300_000,
        }, provider_hint: None, required_capabilities: vec!["chat".into()], region: None, predicted_units: 3,
        safety_margin_units: 0, observation_max_age_ms: 30_000, allowed_accounts: None, dedicated_account: None,
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms: SCHEDULER_NOW_MS, lease_ttl_ms: 1, reconcile_ttl_ms: 600_000,
    }).unwrap();
    let lease_id = match acquired { SchedulerLeaseResult::Acquired(grant) => grant.lease_id, other => panic!("expected acquisition, got {other:?}") };
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let request_id: String = connection.query_row("SELECT request_id FROM upstream_leases WHERE id = ?1", [&lease_id], |row| row.get(0)).unwrap();
    store.transition_request(&request_id, aiwork_core::RequestState::Reserved, aiwork_core::RequestState::Queued, None).unwrap();
    store.transition_request(&request_id, aiwork_core::RequestState::Queued, aiwork_core::RequestState::Dispatched, None).unwrap();
    store.recover_expired_upstream_leases(SCHEDULER_NOW_MS + 2).unwrap();
    assert_eq!(store.request_state(&request_id).unwrap(), aiwork_core::RequestState::Unknown);
    assert_eq!(store.balance("u1", "chat_request").unwrap().held, 3);
    let state: String = connection.query_row("SELECT state FROM upstream_leases WHERE id = ?1", [&lease_id], |row| row.get(0)).unwrap();
    assert_eq!(state, LeaseState::Unknown.as_str());
}

#[test]
fn user_quota_usage_projection_is_owner_scoped_bounded_and_redacted() {
    let (store, dir) = test_store_with_grant(100);
    store.create_user(user("u2", UserRole::User), "admin-1").unwrap();
    store
        .grant(QuotaGrant {
            user_id: "u2".into(),
            resource_kind: "chat_request".into(),
            amount: 999,
            actor_user_id: "admin-1".into(),
            reason: "other-user-secret-grant".into(),
        })
        .unwrap();
    let key = store
        .issue_api_key(
            "u1",
            "usage",
            std::collections::BTreeSet::from(["usage:read".to_owned()]),
            "admin-1",
        )
        .unwrap();
    let owner = Principal {
        user_id: "u1".into(),
        key_id: key.id,
        scopes: std::collections::BTreeSet::from(["usage:read".to_owned()]),
    };

    let _held_id = created(&store, "usage-held", 5);
    let unknown_id = created(&store, "usage-unknown", 7);
    let committed_id = created(&store, "usage-committed", 9);
    store
        .settle(&owner, &unknown_id, Settlement::Unknown)
        .unwrap();
    store
        .settle(
            &owner,
            &committed_id,
            Settlement::Commit {
                actual_amount: Some(4),
            },
        )
        .unwrap();

    let view = store.quota_usage_for_principal(&owner, 100).unwrap();
    assert_eq!(view.balances.len(), 1);
    assert_eq!(view.balances[0].resource_kind, "chat_request");
    assert_eq!(view.balances[0].available, 84);
    assert_eq!(view.balances[0].held, 12);
    assert_eq!(view.balances[0].settled, 4);
    assert_eq!(view.ledger.len(), 5);
    assert!(view.ledger.iter().all(|entry| entry.request_id.as_deref() != Some("u2")));
    assert!(view.ledger.iter().any(|entry| entry.request_id.as_deref() == Some("usage-committed")));

    let serialized = serde_json::to_string(&view).unwrap();
    for forbidden in ["actor_user_id", "other-user-secret-grant", "prompt", "u2"] {
        assert!(!serialized.contains(forbidden), "projection leaked {forbidden}");
    }
    assert_eq!(store.quota_usage_for_principal(&owner, 1).unwrap().ledger.len(), 1);
    assert!(matches!(
        store.quota_usage_for_principal(&owner, 0),
        Err(aiwork_core::CoreError::Validation { .. })
    ));
    assert!(matches!(
        store.quota_usage_for_principal(&owner, 101),
        Err(aiwork_core::CoreError::Validation { .. })
    ));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
