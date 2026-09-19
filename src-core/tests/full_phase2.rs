use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    BeginRequestInput, ChatExecutionRequest, ChatExecutor, CoreStore, CostPolicy, LeaseOutcome,
    LeaseState, MockChatExecutor, NewUser, ObservationStatus, PreflightReserveInput, Principal,
    QuotaGrant, RegisterUpstreamAccount, RequestState, ReservationState, ScheduleError,
    SchedulerLeaseRequest, SchedulerLeaseResult, SelectionStrategy, UpstreamAccountState,
    UpstreamObservation, UserRole, CORE_DB_FILE,
};
use rusqlite::Connection;
use serde_json::json;

const NOW_MS: i64 = 1_800_000_000_000;
const RESOURCE_KIND: &str = "chat_request";
const USER_ID: &str = "phase2-smoke-user";

struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let root = PathBuf::from(r"D:\gpt");
        fs::create_dir_all(&root).expect("create D drive test root");
        let path = root.join(format!(
            "aiwork-core-full-phase2-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).expect("create isolated phase2 directory");
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
    user: Principal,
}

fn scopes(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn account(
    id: &str,
    provider: &str,
    region: &str,
    credentials_ref: &str,
) -> RegisterUpstreamAccount {
    let mut account = RegisterUpstreamAccount::new(
        id.to_owned(),
        provider.to_owned(),
        credentials_ref.to_owned(),
    );
    account.region = Some(region.to_owned());
    account.capabilities = BTreeSet::from(["chat".to_owned()]);
    account.enabled = true;
    account.max_concurrency = 1;
    account.state = UpstreamAccountState::Available;
    account
}

fn observation(
    id: &str,
    account_ref: &str,
    status: ObservationStatus,
    observed_at_ms: i64,
    stale_at_ms: i64,
) -> UpstreamObservation {
    UpstreamObservation::new(
        id.to_owned(),
        account_ref.to_owned(),
        RESOURCE_KIND.to_owned(),
        Some(100),
        1,
        "reader".to_owned(),
        status,
        observed_at_ms,
        stale_at_ms,
        json!({
            "available": 100,
            "value_scale": 1,
            "source": "reader",
            "status": status.as_str(),
            "resource_kind": RESOURCE_KIND
        }),
    )
}

fn request(
    fixture: &Fixture,
    idempotency_key: &str,
    account_ref: &str,
    provider: &str,
    region: &str,
    now_ms: i64,
) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: fixture.user.user_id.clone(),
                api_key_id: fixture.user.key_id.clone(),
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
        provider_hint: Some(provider.to_owned()),
        required_capabilities: vec!["chat".to_owned()],
        region: Some(region.to_owned()),
        predicted_units: 1,
        safety_margin_units: 0,
        observation_max_age_ms: 60_000,
        allowed_accounts: Some(vec![account_ref.to_owned()]),
        dedicated_account: Some(account_ref.to_owned()),
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms,
        lease_ttl_ms: 60_000,
        reconcile_ttl_ms: 600_000,
    }
}

fn acquired(result: Result<SchedulerLeaseResult, ScheduleError>) -> aiwork_core::UpstreamLeaseGrant {
    match result.expect("scheduler request should succeed") {
        SchedulerLeaseResult::Acquired(grant) => grant,
        SchedulerLeaseResult::Replay { .. } => panic!("expected a new lease, got replay"),
    }
}

fn fixture() -> Fixture {
    let dir = TempDir::new();
    let store = Arc::new(CoreStore::open(dir.path()).expect("open phase2 CoreStore"));
    store.migrate().expect("migrate phase2 CoreStore");

    let admin_user = store
        .create_bootstrap_admin(user("phase2-smoke-admin", UserRole::Admin), "bootstrap")
        .expect("create bootstrap admin");
    let admin_key = store
        .issue_api_key(
            &admin_user.id,
            "phase2 admin key",
            scopes(&["admin"]),
            "bootstrap",
        )
        .expect("issue admin key");
    let admin = store
        .authenticate_api_key(&admin_key.plaintext)
        .expect("authenticate admin key");

    store
        .create_user_as_admin(user(USER_ID, UserRole::User), &admin)
        .expect("create phase2 user");
    let user_key = store
        .issue_api_key_as_admin(
            USER_ID,
            "phase2 user key",
            scopes(&["chat:invoke"]),
            &admin,
        )
        .expect("issue user key");
    let user = store
        .authenticate_api_key(&user_key.plaintext)
        .expect("authenticate user key");

    store
        .upsert_cost_policy(CostPolicy {
            id: "phase2-chat-policy".to_owned(),
            endpoint: "chat".to_owned(),
            model_pattern: "mock-*".to_owned(),
            resource_kind: RESOURCE_KIND.to_owned(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .expect("install phase2 cost policy");
    store
        .grant_as_admin(
            QuotaGrant {
                user_id: USER_ID.to_owned(),
                resource_kind: RESOURCE_KIND.to_owned(),
                amount: 3,
                actor_user_id: "ignored-by-principal".to_owned(),
                reason: "phase2 fixed smoke grant".to_owned(),
            },
            &admin,
        )
        .expect("grant fixed user quota");

    store
        .upsert_upstream_account(
            account(
                "phase2-trae-cn",
                "trae",
                "cn",
                "vault://phase2/trae-cn",
            ),
            &admin,
        )
        .expect("register cn account");
    store
        .upsert_upstream_account(
            account(
                "phase2-work-us",
                "workbuddy",
                "us",
                "vault://phase2/work-us",
            ),
            &admin,
        )
        .expect("register us account");
    store
        .append_upstream_observation(observation(
            "phase2-trae-fresh",
            "phase2-trae-cn",
            ObservationStatus::Fresh,
            NOW_MS,
            NOW_MS + 60_000,
        ))
        .expect("record cn fresh observation");
    store
        .append_upstream_observation(observation(
            "phase2-work-initial",
            "phase2-work-us",
            ObservationStatus::Fresh,
            NOW_MS,
            NOW_MS + 60_000,
        ))
        .expect("record us fresh observation");

    Fixture {
        dir,
        store,
        admin,
        user,
    }
}

#[test]
fn phase2_full_mock_flow_is_atomic_restartable_and_budget_safe() {
    let fixture = fixture();

    let start = Arc::new(Barrier::new(2));
    let racers = ["phase2-concurrent-a", "phase2-concurrent-b"]
        .into_iter()
        .map(|idempotency_key| {
            let store = Arc::clone(&fixture.store);
            let principal = fixture.user.clone();
            let start = Arc::clone(&start);
            let idempotency_key = idempotency_key.to_owned();
            thread::spawn(move || {
                start.wait();
                let result = store.preflight_reserve_with_lease(
                    &principal,
                    request_for_thread(
                        &principal,
                        &idempotency_key,
                        "phase2-trae-cn",
                        "trae",
                        "cn",
                        NOW_MS,
                    ),
                );
                (idempotency_key, result)
            })
        })
        .collect::<Vec<_>>();
    let results = racers
        .into_iter()
        .map(|handle| handle.join().expect("concurrent scheduler thread"))
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(result, Ok(SchedulerLeaseResult::Acquired(_))))
            .count(),
        1,
        "max_concurrency=1 must allow one concurrent lease"
    );
    assert_eq!(
        results
            .iter()
            .filter(|(_, result)| matches!(result, Err(ScheduleError::NoUpstreamCapacity)))
            .count(),
        1,
        "the second concurrent request must fail deterministically with no capacity"
    );
    let (winning_key, concurrent_grant) = results
        .into_iter()
        .find_map(|(key, result)| match result {
            Ok(SchedulerLeaseResult::Acquired(grant)) => Some((key, grant)),
            _ => None,
        })
        .expect("one concurrent lease grant");

    let concurrent_request = match fixture
        .store
        .preflight_reserve_with_lease(
            &fixture.user,
            request(
                &fixture,
                &winning_key,
                "phase2-trae-cn",
                "trae",
                "cn",
                NOW_MS,
            ),
        )
        .expect("read concurrent request replay")
    {
        SchedulerLeaseResult::Replay { request, lease } => {
            assert_eq!(lease.id, concurrent_grant.lease_id);
            request
        }
        SchedulerLeaseResult::Acquired(_) => panic!("concurrent request must replay"),
    };
    let executor = MockChatExecutor::ok();
    let response = executor
        .execute(ChatExecutionRequest {
            request_id: concurrent_request.id.clone(),
            endpoint: concurrent_request.endpoint.clone(),
            model: concurrent_request.model.clone(),
            body: json!({"model": "mock-1", "messages": []}),
        })
        .expect("fixed Mock chat success");
    fixture
        .store
        .settle_upstream_lease(
            &fixture.user,
            &concurrent_grant.lease_id,
            LeaseOutcome::Success {
                actual_units: response.actual_amount,
                upstream_request_ref: Some("mock-success-ref".to_owned()),
                now_ms: NOW_MS + 1,
            },
        )
        .expect("settle successful lease");
    assert_eq!(executor.calls().len(), 1);

    match fixture
        .store
        .preflight_reserve_with_lease(
            &fixture.user,
            request(
                &fixture,
                &winning_key,
                "phase2-trae-cn",
                "trae",
                "cn",
                NOW_MS + 2,
            ),
        )
        .expect("successful request replay")
    {
        SchedulerLeaseResult::Replay { request, lease } => {
            assert_eq!(request.id, concurrent_request.id);
            assert_eq!(request.state, RequestState::Settled);
            assert_eq!(lease.state, LeaseState::Succeeded);
        }
        SchedulerLeaseResult::Acquired(_) => panic!("same idempotency key acquired twice"),
    }
    assert_eq!(executor.calls().len(), 1, "replay must not call Mock twice");

    let rejected_grant = acquired(fixture.store.preflight_reserve_with_lease(
        &fixture.user,
        request(
            &fixture,
            "phase2-rejected",
            "phase2-trae-cn",
            "trae",
            "cn",
            NOW_MS + 3,
        ),
    ));
    let rejected = fixture
        .store
        .settle_upstream_lease_with_status(
            &fixture.user,
            &rejected_grant.lease_id,
            LeaseOutcome::Rejected {
                status: 429,
                code: Some("rate_limited".to_owned()),
                accepted: false,
                now_ms: NOW_MS + 4,
            },
        )
        .expect("settle explicit upstream rejection");
    assert!(rejected.applied);
    assert_eq!(rejected.lease.state, LeaseState::Failed);
    let rejected_request = fixture
        .store
        .preflight_reserve(request(
            &fixture,
            "phase2-rejected",
            "phase2-trae-cn",
            "trae",
            "cn",
            NOW_MS + 5,
        )
        .preflight)
        .expect("read rejected request");
    match rejected_request {
        aiwork_core::PreflightReserveResult::Existing {
            reservation: Some(reservation), ..
        } => assert_eq!(reservation.state, ReservationState::Released),
        other => panic!("expected released rejection reservation, got {other:?}"),
    }

    fixture
        .store
        .append_upstream_observation(observation(
            "phase2-work-stale",
            "phase2-work-us",
            ObservationStatus::Stale,
            NOW_MS + 1,
            NOW_MS + 60_000,
        ))
        .expect("record stale observation");
    let balance_before_stale = fixture
        .store
        .balance(USER_ID, RESOURCE_KIND)
        .expect("read balance before stale rejection");
    assert!(matches!(
        fixture.store.preflight_reserve_with_lease(
            &fixture.user,
            request(
                &fixture,
                "phase2-stale",
                "phase2-work-us",
                "workbuddy",
                "us",
                NOW_MS + 2,
            ),
        ),
        Err(ScheduleError::NoFreshObservation)
    ));
    assert_eq!(
        fixture.store.balance(USER_ID, RESOURCE_KIND).unwrap(),
        balance_before_stale,
        "stale rejection must not reserve user quota"
    );

    fixture
        .store
        .append_upstream_observation(observation(
            "phase2-work-z-restored",
            "phase2-work-us",
            ObservationStatus::Fresh,
            NOW_MS + 2,
            NOW_MS + 60_000,
        ))
        .expect("restore a fresh observation for unknown flow");
    let unknown_grant = acquired(fixture.store.preflight_reserve_with_lease(
        &fixture.user,
        request(
            &fixture,
            "phase2-unknown",
            "phase2-work-us",
            "workbuddy",
            "us",
            NOW_MS + 3,
        ),
    ));
    let timeout_executor = MockChatExecutor::timeout();
    assert!(timeout_executor
        .execute(ChatExecutionRequest {
            request_id: "phase2-unknown-request".to_owned(),
            endpoint: "chat".to_owned(),
            model: "mock-1".to_owned(),
            body: json!({"model": "mock-1", "messages": []}),
        })
        .is_err());
    let secret_like = "Bearer eyJhbGciOiJIUzI1NiJ9.prompt-body-cookie=abc";
    let unknown = fixture
        .store
        .settle_upstream_lease_with_status(
            &fixture.user,
            &unknown_grant.lease_id,
            LeaseOutcome::TransportUnknown {
                reason: secret_like.to_owned(),
                upstream_request_ref: Some(secret_like.to_owned()),
                now_ms: NOW_MS + 4,
            },
        )
        .expect("settle timeout as unknown");
    assert!(unknown.applied);
    assert_eq!(unknown.lease.state, LeaseState::Unknown);

    let dir = fixture.dir.path().to_path_buf();
    let before_restart = fixture
        .store
        .balance(USER_ID, RESOURCE_KIND)
        .expect("read balance before restart");
    assert_eq!(before_restart.available, 1);
    assert_eq!(before_restart.held, 1);
    drop(fixture.store.clone());
    let reopened = CoreStore::open(&dir).expect("reopen CoreStore after unknown");
    reopened.migrate().expect("re-migrate reopened CoreStore");
    let recovered = reopened
        .recover_expired_upstream_leases(NOW_MS + 120_000)
        .expect("recover leases after restart");
    assert!(recovered
        .iter()
        .any(|lease| lease.id == unknown_grant.lease_id && lease.state == LeaseState::Unknown));
    assert_eq!(reopened.balance(USER_ID, RESOURCE_KIND).unwrap().held, 1);

    let status = reopened
        .scheduler_status_for_admin(&fixture.admin, NOW_MS + 5)
        .expect("read redacted admin scheduler status");
    assert_eq!(status["accounts"], 2);
    assert_eq!(status["unknown_leases"], 1);
    assert_eq!(status["slot_saturated"], 1);
    let status_text = status.to_string();
    assert!(!status_text.contains("vault://phase2"));
    assert!(!status_text.contains(USER_ID));
    assert!(!status_text.contains("credentials_ref"));

    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE))
        .expect("open phase2 audit database");
    let audit_text: String = connection
        .query_row(
            "SELECT group_concat(metadata_json, ' ') FROM audit_events",
            [],
            |row| row.get(0),
        )
        .expect("read phase2 audit metadata");
    assert!(!audit_text.contains(secret_like));
    assert!(!audit_text.contains("prompt-body"));
    assert!(!audit_text.contains("cookie"));
    let balance = reopened
        .balance(USER_ID, RESOURCE_KIND)
        .expect("read final bounded balance");
    assert!(balance.available >= 0);
    assert!(balance.held >= 0);
    assert!(balance.available + balance.held <= 3, "quota must never overdraw");
    drop(connection);
    drop(reopened);
}

fn request_for_thread(
    principal: &Principal,
    idempotency_key: &str,
    account_ref: &str,
    provider: &str,
    region: &str,
    now_ms: i64,
) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: principal.user_id.clone(),
                api_key_id: principal.key_id.clone(),
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
        provider_hint: Some(provider.to_owned()),
        required_capabilities: vec!["chat".to_owned()],
        region: Some(region.to_owned()),
        predicted_units: 1,
        safety_margin_units: 0,
        observation_max_age_ms: 60_000,
        allowed_accounts: Some(vec![account_ref.to_owned()]),
        dedicated_account: Some(account_ref.to_owned()),
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms,
        lease_ttl_ms: 60_000,
        reconcile_ttl_ms: 600_000,
    }
}
