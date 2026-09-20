use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, KeyQuotaGrant, LeaseOutcome, LeaseState, NewUser,
    ObservationStatus, PreflightReserveInput, Principal, QuotaGrant, RegisterUpstreamAccount,
    RequestState, ReservationState, SchedulerLeaseRequest, SchedulerLeaseResult, ScheduleError,
    SelectionStrategy, UpstreamAccountState, UpstreamObservation, UserRole, CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

const NOW_MS: i64 = 1_800_000_000_000;
const RESOURCE_KIND: &str = "chat_request";
const USER_ID: &str = "phase3-streaming-user";
const ACCOUNT_REF: &str = "phase3-trae-cn";

struct TempDir(PathBuf);

impl TempDir {
    fn new(prefix: &str) -> Self {
        let root = PathBuf::from(r"D:\gpt");
        fs::create_dir_all(&root).expect("create D drive test root");
        let path = root.join(format!(
            "aiwork-core-full-phase3-{prefix}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).expect("create isolated phase3 directory");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    dir: TempDir,
    store: Arc<CoreStore>,
    admin: Principal,
    principal: Principal,
}

fn scopes(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn account() -> RegisterUpstreamAccount {
    let mut account = RegisterUpstreamAccount::new(
        ACCOUNT_REF.to_owned(),
        "trae".to_owned(),
        "vault://phase3/trae-cn".to_owned(),
    );
    account.region = Some("cn".to_owned());
    account.capabilities = BTreeSet::from(["chat".to_owned()]);
    account.enabled = true;
    account.max_concurrency = 1;
    account.state = UpstreamAccountState::Available;
    account
}

fn fixture(prefix: &str) -> Fixture {
    let dir = TempDir::new(prefix);
    let store = Arc::new(CoreStore::open(dir.path()).expect("open phase3 CoreStore"));
    store.migrate().expect("migrate phase3 CoreStore");

    let admin_user = store
        .create_bootstrap_admin(
            NewUser {
                id: "phase3-streaming-admin".to_owned(),
                name: "Phase 3 Streaming Admin".to_owned(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .expect("create bootstrap admin");
    let admin_key = store
        .issue_api_key(
            &admin_user.id,
            "phase3 admin key",
            scopes(&["admin"]),
            "bootstrap",
        )
        .expect("issue admin key");
    let admin = store
        .authenticate_api_key(&admin_key.plaintext)
        .expect("authenticate admin key");

    store
        .create_user(
            NewUser {
                id: USER_ID.to_owned(),
                name: "Phase 3 Streaming User".to_owned(),
                role: UserRole::User,
            },
            &admin.user_id,
        )
        .expect("create phase3 user");
    let user_key = store
        .issue_api_key(
            USER_ID,
            "phase3 user key",
            scopes(&["chat:invoke"]),
            &admin.user_id,
        )
        .expect("issue user key");
    let principal = store
        .authenticate_api_key(&user_key.plaintext)
        .expect("authenticate user key");

    store
        .upsert_cost_policy(CostPolicy {
            id: "phase3-streaming-policy".to_owned(),
            endpoint: "chat".to_owned(),
            model_pattern: "mock-*".to_owned(),
            resource_kind: RESOURCE_KIND.to_owned(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .expect("install phase3 cost policy");
    store
        .grant(QuotaGrant {
            user_id: USER_ID.to_owned(),
            resource_kind: RESOURCE_KIND.to_owned(),
            amount: 8,
            actor_user_id: admin.user_id.clone(),
            reason: "phase3 fixed mock streaming grant".to_owned(),
        })
        .expect("grant phase3 user quota");
    store
        .key_quota_grant_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: principal.key_id.clone(),
                resource_kind: RESOURCE_KIND.to_owned(),
                amount: 8,
                actor_user_id: "ignored-by-principal".to_owned(),
                reason: "phase3 key quota".to_owned(),
            },
        )
        .expect("grant phase3 key quota");

    store
        .upsert_upstream_account(account(), &admin)
        .expect("register phase3 account");
    store
        .append_upstream_observation(UpstreamObservation::new(
            "phase3-trae-fresh".to_owned(),
            ACCOUNT_REF.to_owned(),
            RESOURCE_KIND.to_owned(),
            Some(100),
            1,
            "reader".to_owned(),
            ObservationStatus::Fresh,
            NOW_MS,
            NOW_MS + 600_000,
            json!({
                "available": 100,
                "value_scale": 1,
                "source": "reader",
                "status": "fresh",
                "resource_kind": RESOURCE_KIND
            }),
        ))
        .expect("record fresh phase3 observation");

    Fixture {
        dir,
        store,
        admin,
        principal,
    }
}

fn request(fixture: &Fixture, idempotency_key: &str, now_ms: i64) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: fixture.principal.user_id.clone(),
                api_key_id: fixture.principal.key_id.clone(),
                protocol: "openai".to_owned(),
                endpoint: "chat".to_owned(),
                model: "mock-1".to_owned(),
                idempotency_key: idempotency_key.to_owned(),
                body: json!({"model": "mock-1", "messages": []}),
            },
            resource_kind: RESOURCE_KIND.to_owned(),
            amount: 1,
            ttl_ms: 60_000,
        },
        provider_hint: Some("trae".to_owned()),
        required_capabilities: vec!["chat".to_owned()],
        region: Some("cn".to_owned()),
        predicted_units: 1,
        safety_margin_units: 0,
        observation_max_age_ms: 60_000,
        allowed_accounts: Some(vec![ACCOUNT_REF.to_owned()]),
        dedicated_account: Some(ACCOUNT_REF.to_owned()),
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms,
        lease_ttl_ms: 60_000,
        reconcile_ttl_ms: 600_000,
    }
}

fn acquired(result: Result<SchedulerLeaseResult, ScheduleError>) -> aiwork_core::UpstreamLeaseGrant {
    match result.expect("phase3 scheduler request should succeed") {
        SchedulerLeaseResult::Acquired(grant) => grant,
        SchedulerLeaseResult::Replay { .. } => panic!("expected a new phase3 lease"),
    }
}

fn request_id_for_lease(dir: &Path, lease_id: &str) -> String {
    Connection::open(dir.join("data").join(CORE_DB_FILE))
        .expect("open phase3 database")
        .query_row(
            "SELECT request_id FROM upstream_leases WHERE id = ?1",
            [lease_id],
            |row| row.get(0),
        )
        .expect("find request for phase3 lease")
}

fn release_event_count(dir: &Path, request_id: &str) -> i64 {
    Connection::open(dir.join("data").join(CORE_DB_FILE))
        .expect("open phase3 ledger")
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'release'",
            [request_id],
            |row| row.get(0),
        )
        .expect("count phase3 release events")
}

#[test]
fn phase3_streaming_lifecycle_is_atomic_replay_safe_and_recoverable() {
    let main_fixture = fixture("lifecycle");

    // Success commits one user reservation and selects exactly the dedicated account.
    let success = acquired(main_fixture.store.preflight_reserve_with_lease(
        &main_fixture.principal,
        request(&main_fixture, "phase3-success", NOW_MS),
    ));
    assert_eq!(success.account_ref, ACCOUNT_REF);
    assert_eq!(success.credentials_ref, "vault://phase3/trae-cn");
    let success_request_id = request_id_for_lease(main_fixture.dir.path(), &success.lease_id);
    let settled = main_fixture
        .store
        .settle_upstream_lease_with_status(
            &main_fixture.principal,
            &success.lease_id,
            LeaseOutcome::Success {
                actual_units: Some(1),
                upstream_request_ref: Some("mock-stream-success".to_owned()),
                now_ms: NOW_MS + 1,
            },
        )
        .expect("settle mock stream success");
    assert!(settled.applied);
    assert_eq!(settled.lease.state, LeaseState::Succeeded);
    assert_eq!(main_fixture.store.request_state(&success_request_id).unwrap(), RequestState::Settled);
    assert_eq!(main_fixture.store.reservation_for_request(&success_request_id).unwrap().unwrap().state, ReservationState::Committed);
    assert_eq!(main_fixture.store.balance(USER_ID, RESOURCE_KIND).unwrap().held, 0);

    // Reusing the same key returns the completed lease and cannot create a second hold.
    match main_fixture
        .store
        .preflight_reserve_with_lease(&main_fixture.principal, request(&main_fixture, "phase3-success", NOW_MS + 2))
        .expect("read success replay")
    {
        SchedulerLeaseResult::Replay { request, lease } => {
            assert_eq!(request.id, success_request_id);
            assert_eq!(request.state, RequestState::Settled);
            assert_eq!(lease.state, LeaseState::Succeeded);
            assert_eq!(lease.account_ref, ACCOUNT_REF);
        }
        SchedulerLeaseResult::Acquired(_) => panic!("same stream idempotency key acquired twice"),
    }

    // An explicit non-accepted response releases the hold exactly once.
    let rejected = acquired(main_fixture.store.preflight_reserve_with_lease(
        &main_fixture.principal,
        request(&main_fixture, "phase3-rejected", NOW_MS + 3),
    ));
    let rejected_request_id = request_id_for_lease(main_fixture.dir.path(), &rejected.lease_id);
    let rejected_settlement = main_fixture
        .store
        .settle_upstream_lease_with_status(
            &main_fixture.principal,
            &rejected.lease_id,
            LeaseOutcome::Rejected {
                status: 429,
                code: Some("rate_limited".to_owned()),
                accepted: false,
                now_ms: NOW_MS + 4,
            },
        )
        .expect("settle explicit rejection");
    assert!(rejected_settlement.applied);
    assert_eq!(rejected_settlement.lease.state, LeaseState::Failed);
    assert_eq!(main_fixture.store.reservation_for_request(&rejected_request_id).unwrap().unwrap().state, ReservationState::Released);
    assert_eq!(release_event_count(main_fixture.dir.path(), &rejected_request_id), 1);

    // A confirmed cancellation releases the reservation and is idempotent on replay.
    let canceled = acquired(main_fixture.store.preflight_reserve_with_lease(
        &main_fixture.principal,
        request(&main_fixture, "phase3-canceled", NOW_MS + 5),
    ));
    let canceled_request_id = request_id_for_lease(main_fixture.dir.path(), &canceled.lease_id);
    main_fixture
        .store
        .request_upstream_cancel(&main_fixture.principal, &canceled.lease_id, NOW_MS + 6)
        .expect("record cancellation intent");
    assert_eq!(main_fixture.store.request_state(&canceled_request_id).unwrap(), RequestState::CancelRequested);
    assert_eq!(main_fixture.store.reservation_for_request(&canceled_request_id).unwrap().unwrap().state, ReservationState::Held);
    let canceled_settlement = main_fixture
        .store
        .settle_upstream_lease_with_status(
            &main_fixture.principal,
            &canceled.lease_id,
            LeaseOutcome::Canceled {
                upstream_request_ref: Some("mock-stream-canceled".to_owned()),
                now_ms: NOW_MS + 7,
            },
        )
        .expect("settle confirmed cancellation");
    assert!(canceled_settlement.applied);
    assert_eq!(canceled_settlement.lease.state, LeaseState::Failed);
    assert_eq!(main_fixture.store.reservation_for_request(&canceled_request_id).unwrap().unwrap().state, ReservationState::Released);
    assert_eq!(release_event_count(main_fixture.dir.path(), &canceled_request_id), 1);
    let replay = main_fixture
        .store
        .settle_upstream_lease_with_status(
            &main_fixture.principal,
            &canceled.lease_id,
            LeaseOutcome::Success {
                actual_units: Some(1),
                upstream_request_ref: Some("must-not-apply".to_owned()),
                now_ms: NOW_MS + 8,
            },
        )
        .expect("read terminal cancellation replay");
    assert!(!replay.applied);
    assert_eq!(release_event_count(main_fixture.dir.path(), &canceled_request_id), 1);

    // Heartbeat extends an active lease. Unsupported cancellation is then kept
    // unknown: the quota remains held and no release ledger event is emitted.
    let unknown = acquired(main_fixture.store.preflight_reserve_with_lease(
        &main_fixture.principal,
        request(&main_fixture, "phase3-unsupported-cancel", NOW_MS + 9),
    ));
    let initial_expiry = unknown.lease_expires_at_ms;
    let heartbeated = main_fixture
        .store
        .heartbeat_upstream_lease(
            &main_fixture.principal,
            &unknown.lease_id,
            NOW_MS + 10,
            120_000,
        )
        .expect("heartbeat active stream lease");
    assert_eq!(heartbeated.state, LeaseState::Active);
    assert!(heartbeated.lease_expires_at_ms > initial_expiry);
    let unknown_request_id = request_id_for_lease(main_fixture.dir.path(), &unknown.lease_id);
    main_fixture
        .store
        .request_upstream_cancel(&main_fixture.principal, &unknown.lease_id, NOW_MS + 11)
        .expect("record unsupported cancellation intent");
    let secret_like = "Bearer eyJhbGciOiJIUzI1NiJ9.prompt-body-cookie=abc";
    let unknown_settlement = main_fixture
        .store
        .settle_upstream_lease_with_status(
            &main_fixture.principal,
            &unknown.lease_id,
            LeaseOutcome::TransportUnknown {
                reason: secret_like.to_owned(),
                upstream_request_ref: Some(secret_like.to_owned()),
                now_ms: NOW_MS + 12,
            },
        )
        .expect("settle unsupported cancellation as unknown");
    assert!(unknown_settlement.applied);
    assert_eq!(unknown_settlement.lease.state, LeaseState::Unknown);
    assert_eq!(main_fixture.store.request_state(&unknown_request_id).unwrap(), RequestState::Unknown);
    assert_eq!(main_fixture.store.reservation_for_request(&unknown_request_id).unwrap().unwrap().state, ReservationState::Unknown);
    assert_eq!(release_event_count(main_fixture.dir.path(), &unknown_request_id), 0);
    let balance = main_fixture
        .store
        .key_quota_balance_as_admin(
            &main_fixture.admin,
            &main_fixture.principal.key_id,
            RESOURCE_KIND,
        )
        .unwrap();
    assert_eq!(balance.held, 1);
    assert!(balance.available >= 0);

    let connection = Connection::open(main_fixture.dir.path().join("data").join(CORE_DB_FILE))
        .expect("open phase3 audit database");
    let audit_text: String = connection
        .query_row(
            "SELECT group_concat(metadata_json, ' ') FROM audit_events",
            [],
            |row| row.get(0),
        )
        .expect("read phase3 audit metadata");
    assert!(!audit_text.contains(secret_like));
    assert!(!audit_text.contains("prompt-body"));
    assert!(!audit_text.contains("cookie"));
    drop(connection);

    // A separate fixed Mock fixture proves restart recovery from a still-held
    // lease. Recovery marks it unknown and preserves the hold, rather than
    // releasing quota or retrying upstream work.
    let recovery = fixture("recovery");
    let recovery_grant = acquired(recovery.store.preflight_reserve_with_lease(
        &recovery.principal,
        request(&recovery, "phase3-restart-recovery", NOW_MS),
    ));
    let recovery_request_id = request_id_for_lease(recovery.dir.path(), &recovery_grant.lease_id);
    let recovery_dir = recovery.dir.path().to_path_buf();
    let recovery_principal = recovery.principal.clone();
    let recovery_admin = recovery.admin.clone();
    drop(recovery.store);
    let reopened = CoreStore::open(&recovery_dir).expect("reopen phase3 CoreStore");
    reopened.migrate().expect("re-migrate phase3 CoreStore");
    let recovered = reopened
        .recover_expired_upstream_leases(NOW_MS + 120_000)
        .expect("recover expired phase3 lease");
    assert!(recovered
        .iter()
        .any(|lease| lease.id == recovery_grant.lease_id && lease.state == LeaseState::Unknown));
    assert_eq!(reopened.request_state(&recovery_request_id).unwrap(), RequestState::Unknown);
    assert_eq!(reopened.reservation_for_request(&recovery_request_id).unwrap().unwrap().state, ReservationState::Unknown);
    let recovered_balance = reopened
        .key_quota_balance_as_admin(
            &recovery_admin,
            &recovery_principal.key_id,
            RESOURCE_KIND,
        )
        .unwrap();
    assert_eq!(recovered_balance.held, 1);
    assert_eq!(release_event_count(&recovery_dir, &recovery_request_id), 0);
    let replay = reopened
        .request_upstream_cancel(&recovery_principal, &recovery_grant.lease_id, NOW_MS + 120_001)
        .expect("terminal recovery cancel replay");
    assert_eq!(replay.state, LeaseState::Unknown);
}
