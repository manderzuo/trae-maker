use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::Arc,
};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, LeaseOutcome, LeaseState, NewUser,
    ObservationStatus, PreflightReserveInput, Principal, QuotaGrant, RegisterUpstreamAccount,
    RequestState, ReservationState, SchedulerLeaseRequest, SchedulerLeaseResult, ScheduleError,
    SelectionStrategy, UpstreamAccountState, UpstreamLeaseGrant, UpstreamObservation, UserRole,
    CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

const NOW_MS: i64 = 1_725_000_000_000;
const RESOURCE_KIND: &str = "chat_request";

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-streaming-{prefix}-{}",
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

fn test_store() -> (Arc<CoreStore>, String, String, PathBuf) {
    let dir = test_dir("store");
    let store = Arc::new(CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser {
                id: "admin-1".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser {
                id: "u1".into(),
                name: "Streaming user".into(),
                role: UserRole::User,
            },
            "admin-1",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("admin-1", "admin", BTreeSet::new(), "admin-1")
        .unwrap();
    let user_key = store
        .issue_api_key(
            "u1",
            "streaming",
            BTreeSet::from(["chat:invoke".to_owned()]),
            "admin-1",
        )
        .unwrap();
    store
        .upsert_cost_policy(CostPolicy {
            id: "chat-policy-v1".into(),
            endpoint: "chat".into(),
            model_pattern: "mock-*".into(),
            resource_kind: RESOURCE_KIND.into(),
            reserve_amount: 3,
            max_actual_amount: Some(3),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .grant(QuotaGrant {
            user_id: "u1".into(),
            resource_kind: RESOURCE_KIND.into(),
            amount: 30,
            actor_user_id: "admin-1".into(),
            reason: "fixed streaming test allocation".into(),
        })
        .unwrap();
    (store, user_key.id, admin_key.id, dir)
}

fn account(id: &str, provider: &str, region: &str) -> RegisterUpstreamAccount {
    let mut input = RegisterUpstreamAccount::new(
        id.into(),
        provider.into(),
        format!("vault://{id}-opaque-ref"),
    );
    input.region = Some(region.into());
    input.capabilities = BTreeSet::from(["chat".to_owned()]);
    input.max_concurrency = 1;
    input.state = UpstreamAccountState::Available;
    input
}

fn configure_account(store: &CoreStore, admin_key_id: &str) {
    let admin = Principal {
        user_id: "admin-1".into(),
        key_id: admin_key_id.into(),
        scopes: BTreeSet::new(),
    };
    store
        .upsert_upstream_account(account("trae-cn", "trae", "cn"), &admin)
        .unwrap();
    store
        .append_upstream_observation(UpstreamObservation::new(
            "obs-streaming".into(),
            "trae-cn".into(),
            RESOURCE_KIND.into(),
            Some(12),
            1,
            "reader".into(),
            ObservationStatus::Fresh,
            NOW_MS,
            NOW_MS + 60_000,
            json!({
                "available": 12,
                "value_scale": 1,
                "source": "reader",
                "status": "fresh",
                "resource_kind": RESOURCE_KIND
            }),
        ))
        .unwrap();
}

fn lease_request(key_id: &str, idempotency_key: &str) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: "u1".into(),
                api_key_id: key_id.into(),
                protocol: "openai".into(),
                endpoint: "chat".into(),
                model: "mock-1".into(),
                idempotency_key: idempotency_key.into(),
                body: json!({"model": "mock-1", "messages": []}),
            },
            resource_kind: RESOURCE_KIND.into(),
            amount: 3,
            ttl_ms: 300_000,
        },
        provider_hint: Some("trae".into()),
        required_capabilities: vec!["chat".into()],
        region: Some("cn".into()),
        predicted_units: 6,
        safety_margin_units: 2,
        observation_max_age_ms: 30_000,
        allowed_accounts: Some(vec!["trae-cn".into()]),
        dedicated_account: Some("trae-cn".into()),
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms: NOW_MS,
        lease_ttl_ms: 120_000,
        reconcile_ttl_ms: 600_000,
    }
}

fn acquired(result: Result<SchedulerLeaseResult, ScheduleError>) -> UpstreamLeaseGrant {
    match result.expect("scheduler acquire") {
        SchedulerLeaseResult::Acquired(grant) => grant,
        SchedulerLeaseResult::Replay { request, .. } => {
            panic!("expected a newly acquired lease, got replay for {}", request.id)
        }
    }
}

fn request_id_for_lease(dir: &PathBuf, lease_id: &str) -> String {
    Connection::open(dir.join("data").join(CORE_DB_FILE))
        .unwrap()
        .query_row(
            "SELECT request_id FROM upstream_leases WHERE id = ?1",
            [lease_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn release_event_count(dir: &PathBuf, request_id: &str) -> i64 {
    Connection::open(dir.join("data").join(CORE_DB_FILE))
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'release'",
            [request_id],
            |row| row.get(0),
        )
        .unwrap()
}

fn downgrade_request_tables_to_v6(dir: &PathBuf) {
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    connection.pragma_update(None, "foreign_keys", "OFF").unwrap();
    connection
        .execute_batch(
            "CREATE TABLE requests_v6 (
               id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL REFERENCES users(id),
               api_key_id TEXT NOT NULL REFERENCES api_keys(id),
               protocol TEXT NOT NULL,
               endpoint TEXT NOT NULL,
               model TEXT NOT NULL,
               request_hash BLOB NOT NULL,
               state TEXT NOT NULL CHECK(state IN ('received','validating','reserved','queued','dispatched','completing','succeeded','failed','unknown','settled')),
               result_status INTEGER,
               error_code TEXT,
               created_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL
             );
             INSERT INTO requests_v6
               (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms)
               SELECT id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms
               FROM requests;
             CREATE TABLE idempotency_keys_v6 (
               scope TEXT NOT NULL,
               client_key TEXT NOT NULL,
               request_hash BLOB NOT NULL,
               request_id TEXT NOT NULL REFERENCES requests_v6(id),
               created_at_ms INTEGER NOT NULL,
               PRIMARY KEY(scope, client_key)
             );
             INSERT INTO idempotency_keys_v6 (scope, client_key, request_hash, request_id, created_at_ms)
               SELECT scope, client_key, request_hash, request_id, created_at_ms FROM idempotency_keys;
             DROP TABLE idempotency_keys;
             DROP TABLE requests;
             ALTER TABLE requests_v6 RENAME TO requests;
             ALTER TABLE idempotency_keys_v6 RENAME TO idempotency_keys;
             DROP TABLE job_attempts;
             DROP TABLE jobs;
             DROP TABLE assets;
             UPDATE schema_meta SET value = '6' WHERE key = 'schema_version';",
        )
        .unwrap();
}

#[test]
fn v9_preserves_request_rows_and_allows_cancel_state_transitions() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_account(&store, &admin_key_id);
    assert_eq!(store.schema_version().unwrap(), 9);

    let grant = acquired(store.preflight_reserve_with_lease(
        &principal(&key_id),
        lease_request(&key_id, "streaming-state-machine"),
    ));
    let request_id = request_id_for_lease(&dir, &grant.lease_id);
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Reserved);
    drop(store);
    downgrade_request_tables_to_v6(&dir);
    let store = Arc::new(CoreStore::open(&dir).unwrap());
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 9);
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Reserved);

    store
        .transition_request(
            &request_id,
            RequestState::Reserved,
            RequestState::Queued,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &request_id,
            RequestState::Queued,
            RequestState::Dispatched,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &request_id,
            RequestState::Dispatched,
            RequestState::Completing,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &request_id,
            RequestState::Completing,
            RequestState::CancelRequested,
            None,
        )
        .unwrap();
    store
        .transition_request(
            &request_id,
            RequestState::CancelRequested,
            RequestState::Canceled,
            Some(aiwork_core::RequestResult {
                status: Some(499),
                error_code: Some("canceled".into()),
            }),
        )
        .unwrap();
    store
        .transition_request(
            &request_id,
            RequestState::Canceled,
            RequestState::Settled,
            None,
        )
        .unwrap();
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Settled);

    drop(store);
    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    assert_eq!(reopened.schema_version().unwrap(), 9);
    assert_eq!(reopened.request_state(&request_id).unwrap(), RequestState::Settled);
}

#[test]
fn cancel_requires_owner_and_keeps_reservation_held_until_confirmation() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_account(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(
        &principal(&key_id),
        lease_request(&key_id, "streaming-cancel-owner"),
    ));
    let request_id = request_id_for_lease(&dir, &grant.lease_id);
    let wrong_principal = Principal {
        user_id: "u2".into(),
        key_id: "wrong-key".into(),
        scopes: BTreeSet::from(["chat:invoke".to_owned()]),
    };

    assert!(matches!(
        store.request_upstream_cancel(&wrong_principal, &grant.lease_id, NOW_MS + 1),
        Err(ScheduleError::InvalidRequestIdentity)
    ));
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Reserved);
    assert_eq!(store.reservation_for_request(&request_id).unwrap().unwrap().state, ReservationState::Held);
    assert_eq!(store.balance("u1", RESOURCE_KIND).unwrap().held, 3);
    assert_eq!(release_event_count(&dir, &request_id), 0);

    let lease = store
        .request_upstream_cancel(&principal(&key_id), &grant.lease_id, NOW_MS + 2)
        .unwrap();
    assert_eq!(lease.state, LeaseState::Held);
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::CancelRequested);
    assert_eq!(store.reservation_for_request(&request_id).unwrap().unwrap().state, ReservationState::Held);
    assert_eq!(store.balance("u1", RESOURCE_KIND).unwrap().held, 3);
    assert_eq!(release_event_count(&dir, &request_id), 0);
}

#[test]
fn confirmed_cancel_releases_once_and_idempotent_replay_has_no_grant() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_account(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(
        &principal(&key_id),
        lease_request(&key_id, "streaming-cancel-settlement"),
    ));
    let request_id = request_id_for_lease(&dir, &grant.lease_id);
    store
        .request_upstream_cancel(&principal(&key_id), &grant.lease_id, NOW_MS + 1)
        .unwrap();

    let canceled = store
        .settle_upstream_lease_with_status(
            &principal(&key_id),
            &grant.lease_id,
            LeaseOutcome::Canceled {
                upstream_request_ref: Some("upstream-canceled-ref".into()),
                now_ms: NOW_MS + 2,
            },
        )
        .unwrap();
    assert!(canceled.applied);
    assert_eq!(canceled.lease.state, LeaseState::Failed);
    assert_eq!(store.request_state(&request_id).unwrap(), RequestState::Settled);
    assert_eq!(store.reservation_for_request(&request_id).unwrap().unwrap().state, ReservationState::Released);
    assert_eq!(store.balance("u1", RESOURCE_KIND).unwrap().available, 30);
    assert_eq!(store.balance("u1", RESOURCE_KIND).unwrap().held, 0);
    assert_eq!(release_event_count(&dir, &request_id), 1);

    let replay = store
        .settle_upstream_lease_with_status(
            &principal(&key_id),
            &grant.lease_id,
            LeaseOutcome::Success {
                actual_units: Some(1),
                upstream_request_ref: Some("ignored-replay".into()),
                now_ms: NOW_MS + 3,
            },
        )
        .unwrap();
    assert!(!replay.applied);
    assert_eq!(replay.lease.state, LeaseState::Failed);
    assert_eq!(release_event_count(&dir, &request_id), 1);

    let replay_request = store
        .preflight_reserve_with_lease(&principal(&key_id), lease_request(&key_id, "streaming-cancel-settlement"))
        .unwrap();
    assert!(matches!(
        replay_request,
        SchedulerLeaseResult::Replay { request, lease }
            if request.state == RequestState::Settled && lease.state == LeaseState::Failed
    ));
}

#[test]
fn terminal_heartbeat_and_cancel_replay_have_no_side_effect() {
    let (store, key_id, admin_key_id, dir) = test_store();
    configure_account(&store, &admin_key_id);
    let grant = acquired(store.preflight_reserve_with_lease(
        &principal(&key_id),
        lease_request(&key_id, "streaming-terminal-side-effects"),
    ));
    let request_id = request_id_for_lease(&dir, &grant.lease_id);
    store
        .request_upstream_cancel(&principal(&key_id), &grant.lease_id, NOW_MS + 1)
        .unwrap();
    let settled = store
        .settle_upstream_lease(
            &principal(&key_id),
            &grant.lease_id,
            LeaseOutcome::Canceled {
                upstream_request_ref: None,
                now_ms: NOW_MS + 2,
            },
        )
        .unwrap();
    let heartbeat = store
        .heartbeat_upstream_lease(&principal(&key_id), &grant.lease_id, NOW_MS + 3, 120_000)
        .unwrap();
    assert_eq!(heartbeat, settled);
    let cancel_replay = store
        .request_upstream_cancel(&principal(&key_id), &grant.lease_id, NOW_MS + 4)
        .unwrap();
    assert_eq!(cancel_replay, settled);
    assert_eq!(release_event_count(&dir, &request_id), 1);
}
