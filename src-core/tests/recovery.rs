use std::{
    collections::BTreeSet,
    fs,
    path::PathBuf,
    sync::Arc,
};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, KeyQuotaGrant, LeaseOutcome, LeaseState, NewUser,
    ObservationStatus, PreflightReserveInput, Principal, QuotaGrant, RegisterUpstreamAccount,
    SchedulerLeaseRequest, SchedulerLeaseResult, SelectionStrategy, UpstreamAccountState,
    UpstreamObservation, UserRole,
};
use rusqlite::Connection;
use serde_json::json;

const NOW_MS: i64 = 1_800_000_000_000;
const RESOURCE_KIND: &str = "chat_request";

fn test_dir(tag: &str) -> PathBuf {
    let root = PathBuf::from(r"D:\gpt");
    fs::create_dir_all(&root).unwrap();
    let dir = root.join(format!("aiwork-task6-recovery-{tag}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

struct Fixture {
    dir: PathBuf,
    store: Arc<CoreStore>,
    admin: Principal,
    user: Principal,
}

impl Fixture {
    fn new() -> Self {
        let dir = test_dir("fixture");
        let store = Arc::new(CoreStore::open(&dir).unwrap());
        store.migrate().unwrap();
        store
            .create_bootstrap_admin(
                NewUser {
                    id: "admin-1".into(),
                    name: "Task 6 admin".into(),
                    role: UserRole::Admin,
                },
                "bootstrap",
            )
            .unwrap();
        store
            .create_user(
                NewUser {
                    id: "user-1".into(),
                    name: "Task 6 user".into(),
                    role: UserRole::User,
                },
                "admin-1",
            )
            .unwrap();
        let admin_key = store
            .issue_api_key("admin-1", "admin", BTreeSet::new(), "bootstrap")
            .unwrap();
        let user_key = store
            .issue_api_key(
                "user-1",
                "user",
                BTreeSet::from(["chat:invoke".to_owned()]),
                "admin-1",
            )
            .unwrap();
        store
            .upsert_cost_policy(CostPolicy {
                id: "task6-chat-policy".into(),
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
                user_id: "user-1".into(),
                resource_kind: RESOURCE_KIND.into(),
                amount: 10,
                actor_user_id: "admin-1".into(),
                reason: "task6 fixture".into(),
            })
            .unwrap();
        store
            .key_quota_grant_as_admin(
                &Principal {
                    user_id: "admin-1".into(),
                    key_id: admin_key.id.clone(),
                    scopes: BTreeSet::new(),
                },
                KeyQuotaGrant {
                    api_key_id: user_key.id.clone(),
                    resource_kind: RESOURCE_KIND.into(),
                    amount: 10,
                    actor_user_id: "ignored-by-principal".into(),
                    reason: "task6 key quota fixture".into(),
                },
            )
            .unwrap();
        Self {
            dir,
            store,
            admin: Principal {
                user_id: "admin-1".into(),
                key_id: admin_key.id,
                scopes: BTreeSet::new(),
            },
            user: Principal {
                user_id: "user-1".into(),
                key_id: user_key.id,
                scopes: BTreeSet::from(["chat:invoke".to_owned()]),
            },
        }
    }

    fn account(&self) {
        let mut account = RegisterUpstreamAccount::new(
            "task6-account".into(),
            "mock".into(),
            "vault://task6/opaque-secret-ref".into(),
        );
        account.capabilities.insert("chat".into());
        account.max_concurrency = 1;
        self.store
            .upsert_upstream_account(account, &self.admin)
            .unwrap();
        self.store
            .append_upstream_observation(UpstreamObservation::new(
                "task6-observation".into(),
                "task6-account".into(),
                RESOURCE_KIND.into(),
                Some(100),
                1,
                "reader".into(),
                ObservationStatus::Fresh,
                NOW_MS,
                NOW_MS + 60_000,
                json!({"available": 100, "source": "reader", "status": "fresh"}),
            ))
            .unwrap();
    }

    fn acquire(&self, key: &str) -> String {
        let result = self
            .store
            .preflight_reserve_with_lease(
                &self.user,
                SchedulerLeaseRequest {
                    preflight: PreflightReserveInput {
                        request: BeginRequestInput {
                            user_id: "user-1".into(),
                            api_key_id: self.user.key_id.clone(),
                            protocol: "openai".into(),
                            endpoint: "chat".into(),
                            model: "mock-1".into(),
                            idempotency_key: key.into(),
                            body: json!({"model": "mock-1", "messages": []}),
                        },
                        resource_kind: RESOURCE_KIND.into(),
                        amount: 3,
                        ttl_ms: 10_000,
                    },
                    provider_hint: Some("mock".into()),
                    required_capabilities: vec!["chat".into()],
                    region: None,
                    predicted_units: 3,
                    safety_margin_units: 0,
                    observation_max_age_ms: 60_000,
                    allowed_accounts: Some(vec!["task6-account".into()]),
                    dedicated_account: None,
                    selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
                    now_ms: NOW_MS,
                    lease_ttl_ms: 1_000,
                    reconcile_ttl_ms: 60_000,
                },
            )
            .unwrap();
        match result {
            SchedulerLeaseResult::Acquired(lease) => lease.lease_id,
            SchedulerLeaseResult::Replay { .. } => panic!("fixture key unexpectedly replayed"),
        }
    }
}

#[test]
fn expired_active_lease_becomes_unknown_after_store_reopen() {
    let fixture = Fixture::new();
    fixture.account();
    let lease_id = fixture.acquire("recovery-key");
    let dir = fixture.dir.clone();
    let admin = fixture.admin.clone();
    let user = fixture.user.clone();
    let store = fixture.store.clone();
    drop(fixture);
    drop(store);

    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    let recovered = reopened
        .recover_expired_upstream_leases(NOW_MS + 2_000)
        .unwrap();
    let lease = recovered.iter().find(|lease| lease.id == lease_id).unwrap();
    assert_eq!(lease.state, LeaseState::Unknown);
    let balance = reopened
        .key_quota_balance_as_admin(&admin, &user.key_id, RESOURCE_KIND)
        .unwrap();
    assert_eq!(balance.held, 3);

    let connection = Connection::open(dir.join("data/core.sqlite3")).unwrap();
    let reservation_state: String = connection
        .query_row(
            "SELECT state FROM quota_reservations WHERE request_id = (SELECT request_id FROM upstream_leases WHERE id = ?1)",
            [&lease_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reservation_state, "unknown");
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM quota_ledger WHERE event_kind = 'release'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        0
    );
    drop(connection);
    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn transport_and_credit_errors_update_account_state_without_logging_secrets() {
    let fixture = Fixture::new();
    fixture.account();
    fixture
        .store
        .record_upstream_health_transition(
            "task6-account",
            Some("request-safe"),
            Some("lease-safe"),
            RESOURCE_KIND,
            Some("task6-observation"),
            "transport_timeout",
            NOW_MS + 1,
        )
        .unwrap();
    fixture
        .store
        .record_upstream_health_transition(
            "task6-account",
            Some("request-safe"),
            Some("lease-safe"),
            RESOURCE_KIND,
            Some("task6-observation"),
            "HardCredit",
            NOW_MS + 2,
        )
        .unwrap();

    let connection = Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
    let (state, errors): (String, i64) = connection
        .query_row(
            "SELECT state, consecutive_errors FROM upstream_accounts WHERE id = 'task6-account'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(state, UpstreamAccountState::Cooling.as_str());
    assert_eq!(errors, 2);
    let audit: String = connection
        .query_row(
            "SELECT group_concat(metadata_json, ' ') FROM audit_events WHERE action = 'upstream.health_transition'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(audit.contains("transport_timeout") && audit.contains("hard_credit"));
    assert!(!audit.contains("vault://task6/opaque-secret-ref"));
    assert!(!audit.contains("prompt"));
    assert!(!audit.contains("jwt") && !audit.contains("cookie"));
    assert!(audit.contains("account_hash") && audit.contains("observation"));
}

#[test]
fn scheduler_status_exposes_counts_but_not_credentials_or_other_users() {
    let fixture = Fixture::new();
    fixture.account();
    let _lease_id = fixture.acquire("status-key");

    let status = fixture
        .store
        .scheduler_status_for_admin(&fixture.admin, NOW_MS)
        .unwrap();
    assert_eq!(status["accounts"], 1);
    assert_eq!(status["enabled_accounts"], 1);
    assert_eq!(status["fresh_observations"], 1);
    assert_eq!(status["active_leases"], 1);
    assert_eq!(status["unknown_leases"], 0);
    assert_eq!(status["slot_saturated"], 1);
    assert_eq!(status["reader_failures"], 0);
    let serialized = status.to_string();
    assert!(!serialized.contains("vault://task6/opaque-secret-ref"));
    assert!(!serialized.contains("user-1"));
    assert!(!serialized.contains("credentials_ref"));
    assert!(fixture
        .store
        .scheduler_status_for_admin(&fixture.user, NOW_MS)
        .is_err());
    drop(fixture);
}

#[test]
fn scheduler_status_uses_latest_observation_and_counts_unknown_slots() {
    let fixture = Fixture::new();
    fixture.account();
    let lease_id = fixture.acquire("status-unknown");
    fixture
        .store
        .append_upstream_observation(UpstreamObservation::new(
            "task6-latest-failed".into(),
            "task6-account".into(),
            RESOURCE_KIND.into(),
            Some(100),
            1,
            "reader".into(),
            ObservationStatus::Failed,
            NOW_MS + 1,
            NOW_MS + 60_000,
            json!({"status": "failed"}),
        ))
        .unwrap();
    fixture
        .store
        .settle_upstream_lease(
            &fixture.user,
            &lease_id,
            LeaseOutcome::TransportUnknown {
                reason: "transport_timeout".into(),
                upstream_request_ref: None,
                now_ms: NOW_MS + 2,
            },
        )
        .unwrap();

    let status = fixture
        .store
        .scheduler_status_for_admin(&fixture.admin, NOW_MS + 3)
        .unwrap();
    assert_eq!(status["fresh_observations"], 0);
    assert_eq!(status["stale_observations"], 1);
    assert_eq!(status["reader_failures"], 1);
    assert_eq!(status["active_leases"], 0);
    assert_eq!(status["unknown_leases"], 1);
    assert_eq!(status["slot_saturated"], 1);
}
