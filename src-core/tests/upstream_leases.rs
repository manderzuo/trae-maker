use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, KeyQuotaGrant, LeaseOutcome, LeaseState, NewUser,
    ObservationStatus, PreflightReserveInput, Principal, QuotaGrant,
    RegisterUpstreamAccount, SchedulerLeaseRequest, SchedulerLeaseResult, ScheduleError,
    SelectionStrategy, UpstreamLeaseGrant,
    UpstreamAccountState, UpstreamObservation, UserRole, CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

const NOW_MS: i64 = 1_725_000_000_000;
const RESOURCE_KIND: &str = "chat_request";

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-upstream-leases-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn principal(key_id: &str) -> Principal {
    Principal {
        user_id: "u1".into(),
        key_id: key_id.into(),
        scopes: BTreeSet::from(["chat:invoke".to_owned()]),
    }
}

fn admin_principal(key_id: &str) -> Principal {
    Principal {
        user_id: "admin-1".into(),
        key_id: key_id.into(),
        scopes: BTreeSet::new(),
    }
}

fn test_store() -> (Arc<CoreStore>, String, String, PathBuf) {
    let dir = test_dir("store");
    let store = Arc::new(CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store.create_bootstrap_admin(NewUser {
        id: "admin-1".into(), name: "Admin".into(), role: UserRole::Admin,
    }, "bootstrap").unwrap();
    store.create_user(NewUser {
        id: "u1".into(), name: "Scheduler user".into(), role: UserRole::User,
    }, "admin-1").unwrap();
    let admin_key = store.issue_api_key("admin-1", "admin", BTreeSet::new(), "admin-1").unwrap();
    let user_key = store.issue_api_key(
        "u1", "scheduler", BTreeSet::from(["chat:invoke".to_owned()]), "admin-1",
    ).unwrap();
    store.upsert_cost_policy(CostPolicy {
        id: "chat-policy-v1".into(), endpoint: "chat".into(), model_pattern: "mock-*".into(),
        resource_kind: RESOURCE_KIND.into(), reserve_amount: 3, max_actual_amount: Some(3),
        version: 1, enabled: true,
    }).unwrap();
    store.grant(QuotaGrant {
        user_id: "u1".into(), resource_kind: RESOURCE_KIND.into(), amount: 30,
        actor_user_id: "admin-1".into(), reason: "fixed test allocation".into(),
    }).unwrap();
    store
        .key_quota_grant_as_admin(
            &admin_principal(&admin_key.id),
            KeyQuotaGrant {
                api_key_id: user_key.id.clone(),
                resource_kind: RESOURCE_KIND.into(),
                amount: 30,
                actor_user_id: "ignored-by-principal".into(),
                reason: "fixed key test allocation".into(),
            },
        )
        .unwrap();
    (store, user_key.id, admin_key.id, dir)
}

fn key_balance(store: &CoreStore, key_id: &str, admin_key_id: &str) -> aiwork_core::QuotaBudgetBalance {
    store
        .key_quota_balance_as_admin(&admin_principal(admin_key_id), key_id, RESOURCE_KIND)
        .unwrap()
}

fn account(id: &str, provider: &str, region: &str, max_concurrency: i64) -> RegisterUpstreamAccount {
    let mut input = RegisterUpstreamAccount::new(
        id.into(), provider.into(), format!("vault://{id}-opaque-ref"),
    );
    input.region = Some(region.into());
    input.capabilities = BTreeSet::from(["chat".to_owned(), "vision".to_owned()]);
    input.max_concurrency = max_concurrency;
    input.state = UpstreamAccountState::Available;
    input
}

fn observation(
    id: &str,
    account_ref: &str,
    source: &str,
    status: ObservationStatus,
    observed_value: Option<i64>,
    value_scale: i64,
    stale_at_ms: i64,
) -> UpstreamObservation {
    UpstreamObservation::new(
        id.into(), account_ref.into(), RESOURCE_KIND.into(), observed_value, value_scale,
        source.into(), status, NOW_MS, stale_at_ms,
        json!({"available": observed_value, "value_scale": value_scale, "source": source,
               "status": status.as_str(), "resource_kind": RESOURCE_KIND}),
    )
}

fn configure_accounts(store: &CoreStore, admin_key_id: &str) {
    let admin = admin_principal(admin_key_id);
    store.upsert_upstream_account(account("trae-cn", "trae", "cn", 1), &admin).unwrap();
    store.upsert_upstream_account(account("work-us", "workbuddy", "us", 2), &admin).unwrap();
    store.append_upstream_observation(observation(
        "obs-trae-fresh", "trae-cn", "reader", ObservationStatus::Fresh, Some(12), 1, NOW_MS + 60_000,
    )).unwrap();
    store.append_upstream_observation(observation(
        "obs-work-fresh", "work-us", "reader", ObservationStatus::Fresh, Some(1_200), 100, NOW_MS + 60_000,
    )).unwrap();
}

fn lease_request(key_id: &str, idempotency_key: &str) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: "u1".into(), api_key_id: key_id.into(), protocol: "openai".into(),
                endpoint: "chat".into(), model: "mock-1".into(),
                idempotency_key: idempotency_key.into(), body: json!({"model": "mock-1", "messages": []}),
            },
            resource_kind: RESOURCE_KIND.into(), amount: 3, ttl_ms: 300_000,
        },
        provider_hint: Some("trae".into()),
        required_capabilities: vec!["chat".into()],
        region: Some("cn".into()),
        predicted_units: 6,
        safety_margin_units: 2,
        observation_max_age_ms: 30_000,
        allowed_accounts: Some(vec!["trae-cn".into(), "work-us".into()]),
        dedicated_account: Some("trae-cn".into()),
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms: NOW_MS,
        lease_ttl_ms: 120_000,
        reconcile_ttl_ms: 600_000,
    }
}

fn assert_grant(grant: &UpstreamLeaseGrant) {
    assert_eq!(grant.account_ref, "trae-cn");
    assert_eq!(grant.observation_id, "obs-trae-fresh");
    assert_eq!(grant.predicted_units, 6);
    assert_eq!(grant.lease_expires_at_ms, NOW_MS + 120_000);
    assert_eq!(grant.credentials_ref, "vault://trae-cn-opaque-ref");
}

fn acquired(result: Result<SchedulerLeaseResult, ScheduleError>) -> UpstreamLeaseGrant {
    match result.expect("scheduler acquire") {
        SchedulerLeaseResult::Acquired(grant) => grant,
        SchedulerLeaseResult::Replay { request, .. } => panic!("expected a newly acquired lease, got replay for {}", request.id),
    }
}

fn request_id_for_lease(dir: &PathBuf, lease_id: &str) -> String {
    Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap()
        .query_row("SELECT request_id FROM upstream_leases WHERE id = ?1", [lease_id], |row| row.get(0))
        .unwrap()
}

#[test]
fn one_slot_allows_one_concurrent_lease_and_no_orphan_reservation() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let start = Arc::new(Barrier::new(2));
    let finish = Arc::new(Barrier::new(2));
    let handles: Vec<_> = ["race-a", "race-b"].into_iter().map(|idempotency_key| {
        let store = Arc::clone(&store);
        let start = Arc::clone(&start);
        let finish = Arc::clone(&finish);
        let key_id = key_id.clone();
        thread::spawn(move || {
            start.wait();
            let result = store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, idempotency_key));
            finish.wait();
            result
        })
    }).collect();
    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(results.iter().any(|result| matches!(result, Err(ScheduleError::NoUpstreamCapacity))));
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM upstream_leases", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM quota_reservations", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
}

#[test]
fn stale_or_json_cache_observation_fails_closed_without_user_hold() {
    let (store, key_id, admin_key_id, dir) = test_store();
    let admin = admin_principal(&admin_key_id);
    store.upsert_upstream_account(account("trae-cn", "trae", "cn", 1), &admin).unwrap();
    for (id, source, status, stale_at_ms) in [
        ("obs-stale", "reader", ObservationStatus::Stale, NOW_MS + 60_000),
        ("obs-json", "json_cache", ObservationStatus::Fresh, NOW_MS + 60_000),
    ] {
        store.append_upstream_observation(observation(id, "trae-cn", source, status, Some(12), 1, stale_at_ms)).unwrap();
        let request = lease_request(&key_id, id);
        assert!(matches!(
            store.preflight_reserve_with_lease(&principal(&key_id), request),
            Err(ScheduleError::NoFreshObservation)
        ));
    }
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    for table in ["requests", "idempotency_keys", "quota_reservations", "upstream_leases"] {
        assert_eq!(connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0)).unwrap(), 0, "rejection left a row in {table}");
    }
}

#[test]
fn same_request_replays_one_lease_and_hash_conflict_is_rejected() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let first = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "same-key")));
    assert_grant(&first);
    let replay = store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "same-key")).unwrap();
    assert!(matches!(replay, SchedulerLeaseResult::Replay { ref lease, .. } if lease.id == first.lease_id));
    let mut conflict = lease_request(&key_id, "same-key");
    conflict.preflight.request.body = json!({"model": "mock-1", "messages": [{"role": "user", "content": "changed"}]});
    assert!(matches!(store.preflight_reserve_with_lease(&principal(&key_id), conflict), Err(ScheduleError::IdempotencyConflict)));
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM upstream_leases", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    let audit: String = connection.query_row("SELECT metadata_json FROM audit_events WHERE action = 'upstream.lease_acquire'", [], |row| row.get(0)).unwrap();
    assert!(audit.contains("obs-trae-fresh") && audit.contains("selection_reason"));
    assert!(!audit.contains("vault://") && !audit.contains("opaque-ref"));
}

#[test]
fn timeout_unknown_survives_restart_and_is_not_ttl_released() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "timeout-key")));
    let unknown = store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::TransportUnknown { reason: "fixed-timeout".into(), upstream_request_ref: Some("upstream-fixed-ref".into()), now_ms: NOW_MS + 1 },
    ).unwrap();
    assert_eq!(unknown.state, LeaseState::Unknown);
    drop(store);
    let restarted = CoreStore::open(&dir).unwrap();
    restarted.migrate().unwrap();
    let recovered = restarted.recover_expired_upstream_leases(NOW_MS + 120_001).unwrap();
    assert!(recovered.iter().any(|lease| lease.id == grant.lease_id && lease.state == LeaseState::Unknown));
    assert_eq!(key_balance(&restarted, &key_id, &admin_key_id).held, 3);
    assert_eq!(restarted.list_recoverable_leases().unwrap()[0].state, LeaseState::Unknown);
}

#[test]
fn settlement_is_idempotent_and_expired_held_lease_becomes_unknown() {
    let (store, key_id, admin_key_id, _) = test_store();
    configure_accounts(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "success-key")));
    let succeeded = store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::Success { actual_units: Some(2), upstream_request_ref: Some("fixed-success-ref".into()), now_ms: NOW_MS + 2 },
    ).unwrap();
    assert_eq!(succeeded.state, LeaseState::Succeeded);
    let replay = store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::Rejected { status: 429, code: Some("ignored-repeat".into()), accepted: false, now_ms: NOW_MS + 3 },
    ).unwrap();
    assert_eq!(replay.state, LeaseState::Succeeded);
    assert_eq!(key_balance(&store, &key_id, &admin_key_id).available, 28);

    let held = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "expired-key")));
    let recovered = store.recover_expired_upstream_leases(NOW_MS + 120_001).unwrap();
    assert!(recovered.iter().any(|lease| lease.id == held.lease_id && lease.state == LeaseState::Unknown));
    assert_eq!(key_balance(&store, &key_id, &admin_key_id).held, 3);

    let mut rejected_request = lease_request(&key_id, "reject-key");
    rejected_request.provider_hint = Some("workbuddy".into());
    rejected_request.region = Some("us".into());
    rejected_request.allowed_accounts = Some(vec!["work-us".into()]);
    rejected_request.dedicated_account = Some("work-us".into());
    let rejected = acquired(store.preflight_reserve_with_lease(&principal(&key_id), rejected_request));
    let rejected = store.settle_upstream_lease(
        &principal(&key_id), &rejected.lease_id,
        LeaseOutcome::Rejected { status: 400, code: Some("fixed-reject".into()), accepted: false, now_ms: NOW_MS + 4 },
    ).unwrap();
    assert_eq!(rejected.state, LeaseState::Failed);
    assert_eq!(
        key_balance(&store, &key_id, &admin_key_id).held,
        3,
        "only the unknown lease remains held"
    );
}

#[test]
fn terminal_or_unknown_replay_never_returns_an_execution_grant() {
    let (store, key_id, admin_key_id, _) = test_store();
    configure_accounts(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "terminal-replay")));
    store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::Success { actual_units: Some(2), upstream_request_ref: None, now_ms: NOW_MS + 1 },
    ).unwrap();
    let terminal_replay = store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "terminal-replay")).unwrap();
    assert!(matches!(terminal_replay, SchedulerLeaseResult::Replay { ref request, ref lease }
        if request.state == aiwork_core::RequestState::Settled && lease.state == LeaseState::Succeeded));

    let mut unknown_request = lease_request(&key_id, "unknown-replay");
    unknown_request.provider_hint = Some("workbuddy".into());
    unknown_request.region = Some("us".into());
    unknown_request.allowed_accounts = Some(vec!["work-us".into()]);
    unknown_request.dedicated_account = Some("work-us".into());
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), unknown_request.clone()));
    store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::TransportUnknown { reason: "fixed-timeout".into(), upstream_request_ref: None, now_ms: NOW_MS + 1 },
    ).unwrap();
    let replay = store.preflight_reserve_with_lease(&principal(&key_id), unknown_request).unwrap();
    assert!(matches!(replay, SchedulerLeaseResult::Replay { ref request, ref lease }
        if request.state == aiwork_core::RequestState::Unknown && lease.state == LeaseState::Unknown));
}

#[test]
fn future_observation_fails_closed_without_a_hold() {
    let (store, key_id, admin_key_id, dir) = test_store();
    let admin = admin_principal(&admin_key_id);
    store.upsert_upstream_account(account("trae-cn", "trae", "cn", 1), &admin).unwrap();
    let mut future = observation("obs-future", "trae-cn", "reader", ObservationStatus::Fresh, Some(12), 1, NOW_MS + 60_000);
    future.observed_at_ms = NOW_MS + 1;
    store.append_upstream_observation(future).unwrap();
    assert!(matches!(
        store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "future-observation")),
        Err(ScheduleError::NoFreshObservation)
    ));
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM quota_reservations", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn sensitive_upstream_diagnostics_are_reduced_to_safe_categories() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "sensitive-diagnostics")));
    let secret_like = "Bearer eyJhbGciOiJIUzI1NiJ9.prompt-body-cookie=abc";
    store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::TransportUnknown { reason: secret_like.into(), upstream_request_ref: Some(secret_like.into()), now_ms: NOW_MS + 1 },
    ).unwrap();
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    for table_and_column in [
        ("requests", "error_code"),
        ("upstream_leases", "error_kind"),
        ("upstream_leases", "upstream_request_ref"),
        ("audit_events", "metadata_json"),
    ] {
        let value: Option<String> = connection.query_row(
            &format!("SELECT {column} FROM {table} ORDER BY rowid DESC LIMIT 1", column = table_and_column.1, table = table_and_column.0),
            [], |row| row.get(0),
        ).unwrap();
        assert!(!value.unwrap_or_default().contains(secret_like), "sensitive diagnostic leaked into {}.{}", table_and_column.0, table_and_column.1);
    }
}

#[test]
fn selection_strategy_changes_ranked_candidate_with_account_ref_tiebreak() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let admin = admin_principal(&admin_key_id);
    store.upsert_upstream_account(account("trae-cn", "trae", "cn", 2), &admin).unwrap();
    let mut first = lease_request(&key_id, "strategy-first");
    first.allowed_accounts = None;
    first.dedicated_account = None;
    first.provider_hint = None;
    first.region = None;
    let first = acquired(store.preflight_reserve_with_lease(&principal(&key_id), first));
    assert_eq!(first.account_ref, "trae-cn");

    let mut least_loaded = lease_request(&key_id, "strategy-least-loaded");
    least_loaded.allowed_accounts = None;
    least_loaded.dedicated_account = None;
    least_loaded.provider_hint = None;
    least_loaded.region = None;
    least_loaded.selection_strategy = SelectionStrategy::LeastActiveSlots;
    let least_loaded = acquired(store.preflight_reserve_with_lease(&principal(&key_id), least_loaded));
    assert_eq!(least_loaded.account_ref, "work-us");
    let audit: String = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap().query_row(
        "SELECT metadata_json FROM audit_events WHERE target_id = ?1", [&least_loaded.lease_id], |row| row.get(0),
    ).unwrap();
    assert!(audit.contains("least_active_slots"));
}

#[test]
fn settlement_after_request_progression_uses_remaining_valid_transitions() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_accounts(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "progressed-settlement")));
    let request_id = request_id_for_lease(&dir, &grant.lease_id);
    store.transition_request(&request_id, aiwork_core::RequestState::Reserved, aiwork_core::RequestState::Queued, None).unwrap();
    store.transition_request(&request_id, aiwork_core::RequestState::Queued, aiwork_core::RequestState::Dispatched, None).unwrap();
    store.transition_request(&request_id, aiwork_core::RequestState::Dispatched, aiwork_core::RequestState::Completing, None).unwrap();
    let lease = store.settle_upstream_lease(
        &principal(&key_id), &grant.lease_id,
        LeaseOutcome::Success { actual_units: Some(2), upstream_request_ref: None, now_ms: NOW_MS + 2 },
    ).unwrap();
    assert_eq!(lease.state, LeaseState::Succeeded);
    assert_eq!(store.request_state(&request_id).unwrap(), aiwork_core::RequestState::Settled);
}
