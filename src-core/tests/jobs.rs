use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequestInput, CoreStore, CostPolicy, CreateVideoJobInput, LeaseOutcome, LeaseState, NewUser, ObservationStatus,
    PreflightReserveInput, Principal, QuotaGrant, RegisterUpstreamAccount, SchedulerLeaseRequest,
    SelectionStrategy, UpstreamObservation, UserRole, VideoJobEnqueueResult, VideoJobLeaseResult,
    CURRENT_SCHEMA_VERSION,
};
use rusqlite::Connection;
use serde_json::json;

fn test_dir(tag: &str) -> PathBuf {
    let root = PathBuf::from(r"D:\gpt");
    fs::create_dir_all(&root).unwrap();
    let dir = root.join(format!("aiwork-core-video-jobs-{tag}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture() -> (CoreStore, Principal, Principal, PathBuf) {
    let dir = test_dir("schema");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "admin".into(),
                name: "admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser {
                id: "video-user".into(),
                name: "video-user".into(),
                role: UserRole::User,
            },
            "admin",
        )
        .unwrap();
    let scopes = BTreeSet::from([
        "videos:submit".to_string(),
        "videos:read".to_string(),
        "videos:cancel".to_string(),
    ]);
    let key = store
        .issue_api_key("video-user", "video-key", scopes.clone(), "admin")
        .unwrap();
    let admin_key = store
        .issue_api_key("admin", "admin-key", BTreeSet::new(), "bootstrap")
        .unwrap();
    (
        store,
        Principal {
            user_id: "admin".into(),
            key_id: admin_key.id,
            scopes: BTreeSet::new(),
        },
        Principal {
            user_id: "video-user".into(),
            key_id: key.id,
            scopes,
        },
        dir,
    )
}

fn prepare_video_scheduler(store: &CoreStore, admin: &Principal) {
    store
        .upsert_cost_policy(CostPolicy {
            id: "video-policy".into(),
            endpoint: "videos".into(),
            model_pattern: "mock-video".into(),
            resource_kind: "video_job".into(),
            reserve_amount: 4,
            max_actual_amount: Some(4),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .grant(QuotaGrant {
            user_id: "video-user".into(),
            resource_kind: "video_job".into(),
            amount: 10,
            actor_user_id: "admin".into(),
            reason: "phase3c fixture".into(),
        })
        .unwrap();
    let mut account = RegisterUpstreamAccount::new(
        "video-account".into(),
        "mock-video".into(),
        "vault://phase3c/video-ref".into(),
    );
    account.capabilities.insert("video".into());
    account.max_concurrency = 1;
    store.upsert_upstream_account(account, admin).unwrap();
    store
        .append_upstream_observation(UpstreamObservation::new(
            "video-observation".into(),
            "video-account".into(),
            "video_job".into(),
            Some(100),
            1,
            "reader".into(),
            ObservationStatus::Fresh,
            1_800_000_000_000,
            1_800_000_060_000,
            json!({"available": 100, "source": "reader"}),
        ))
        .unwrap();
}

fn scheduler_request(principal: &Principal, key: &str, now_ms: i64) -> SchedulerLeaseRequest {
    SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: "video-user".into(),
                api_key_id: principal.key_id.clone(),
                protocol: "openai".into(),
                endpoint: "videos".into(),
                model: "mock-video".into(),
                idempotency_key: key.into(),
                body: json!({"model": "mock-video", "prompt": "redacted"}),
            },
            resource_kind: "video_job".into(),
            amount: 4,
            ttl_ms: 10_000,
        },
        provider_hint: Some("mock-video".into()),
        required_capabilities: vec!["video".into()],
        region: None,
        predicted_units: 4,
        safety_margin_units: 0,
        observation_max_age_ms: 60_000,
        allowed_accounts: Some(vec!["video-account".into()]),
        dedicated_account: None,
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms,
        lease_ttl_ms: 60_000,
        reconcile_ttl_ms: 600_000,
    }
}

#[test]
fn durable_video_queue_claim_round_robins_users_and_claims_each_job_once() {
    let (store, admin, principal, dir) = fixture();
    store
        .create_user(
            NewUser {
                id: "video-user-2".into(),
                name: "video-user-2".into(),
                role: UserRole::User,
            },
            "admin",
        )
        .unwrap();
    let user2_key = store
        .issue_api_key("video-user-2", "video-key-2", BTreeSet::from([
            "videos:submit".to_string(),
            "videos:read".to_string(),
            "videos:cancel".to_string(),
        ]), "admin")
        .unwrap();
    let principal2 = Principal {
        user_id: "video-user-2".into(),
        key_id: user2_key.id,
        scopes: BTreeSet::from([
            "videos:submit".to_string(),
            "videos:read".to_string(),
            "videos:cancel".to_string(),
        ]),
    };
    prepare_video_scheduler(&store, &admin);
    let mut account = RegisterUpstreamAccount::new(
        "video-account".into(),
        "mock-video".into(),
        "vault://phase4e/video-ref".into(),
    );
    account.capabilities.insert("video".into());
    account.max_concurrency = 8;
    store.upsert_upstream_account(account, &admin).unwrap();
    store
        .grant(QuotaGrant {
            user_id: "video-user-2".into(),
            resource_kind: "video_job".into(),
            amount: 20,
            actor_user_id: "admin".into(),
            reason: "phase4e queue fixture".into(),
        })
        .unwrap();

    let queue_request = |owner: &Principal, user_id: &str, key: &str, now_ms: i64| {
        SchedulerLeaseRequest {
            preflight: PreflightReserveInput {
                request: BeginRequestInput {
                    user_id: user_id.into(),
                    api_key_id: owner.key_id.clone(),
                    protocol: "openai".into(),
                    endpoint: "videos".into(),
                    model: "mock-video".into(),
                    idempotency_key: key.into(),
                    body: json!({"model": "mock-video", "prompt": "redacted"}),
                },
                resource_kind: "video_job".into(),
                amount: 4,
                ttl_ms: 60_000,
            },
            provider_hint: Some("mock-video".into()),
            required_capabilities: vec!["video".into()],
            region: None,
            predicted_units: 4,
            safety_margin_units: 0,
            observation_max_age_ms: 60_000,
            allowed_accounts: Some(vec!["video-account".into()]),
            dedicated_account: None,
            selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
            now_ms,
            lease_ttl_ms: 60_000,
            reconcile_ttl_ms: 600_000,
        }
    };

    for (index, (owner, user_id)) in [
        (&principal, "video-user"),
        (&principal2, "video-user-2"),
        (&principal, "video-user"),
        (&principal2, "video-user-2"),
    ]
    .into_iter()
    .enumerate()
    {
        let result = store
            .enqueue_video_job(
                owner,
                queue_request(owner, user_id, &format!("queue-{index}"), 1_800_000_000_000 + index as i64),
                CreateVideoJobInput {
                    id: format!("queue-job-{index}"),
                    input_hash: vec![index as u8 + 1; 32],
                },
            )
            .unwrap();
        assert!(matches!(result, VideoJobEnqueueResult::Created { .. }));
    }

    assert_eq!(store.count_rows("upstream_leases").unwrap(), 0);

    let claimed = (0..4)
        .map(|index| {
            store
                .claim_next_video_job("worker-phase4e", 1_800_000_010_000 + index)
                .unwrap()
                .unwrap()
                .job
                .user_id
        })
        .collect::<Vec<_>>();
    assert_eq!(claimed, ["video-user", "video-user-2", "video-user", "video-user-2"]);
    assert!(store
        .claim_next_video_job("worker-phase4e", 1_800_000_010_004)
        .unwrap()
        .is_none());
    assert_eq!(store.count_rows("jobs").unwrap(), 4);
    assert_eq!(store.count_rows("job_attempts").unwrap(), 4);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bootstrap_creates_authoritative_video_job_tables() {
    let (store, _admin, _principal, dir) = fixture();
    assert_eq!(CURRENT_SCHEMA_VERSION, 11);
    assert_eq!(store.schema_version().unwrap(), 11);
    for table in ["jobs", "job_attempts", "dispatch_queue_cursors"] {
        assert_eq!(store.table_count(table).unwrap(), 1, "missing table {table}");
        assert_eq!(store.count_rows(table).unwrap(), 0);
    }
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v8_migration_preserves_assets_and_does_not_import_legacy_video_jobs() {
    let dir = test_dir("v8-migration");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    drop(store);

    let database = dir.join("data").join("core.sqlite3");
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "INSERT INTO legacy_jobs
               (id, owner_key_id, user_id, status, reconcile_required, created_at_ms, updated_at_ms,
                migration_id, actor_user_id, reason)
             SELECT 'legacy-video', 'legacy-key', id, 'processing', 1, 1, 1,
                    'phase3c-test', id, 'fixture'
               FROM users WHERE id = 'video-user';
             DROP TABLE dispatch_queue_cursors;
             DROP TABLE jobs;
             DROP TABLE job_attempts;
             UPDATE schema_meta SET value = '8' WHERE key = 'schema_version';",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 11);
    assert_eq!(store.count_rows("jobs").unwrap(), 0);
    assert_eq!(store.count_rows("job_attempts").unwrap(), 0);
    assert_eq!(store.table_count("assets").unwrap(), 1);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn job_tables_reject_invalid_states_and_unbounded_references() {
    let (store, _admin, _principal, dir) = fixture();
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    assert!(connection
        .execute(
            "INSERT INTO jobs
             (id, request_id, user_id, kind, model, input_hash, state, reconcile_required, created_at_ms, updated_at_ms)
             VALUES ('job-1', 'missing-request', 'video-user', 'video', 'mock-video', zeroblob(32), 'processing', 0, 1, 1)",
            [],
        )
        .is_err());
    assert!(connection
        .execute(
            "INSERT INTO jobs
             (id, request_id, user_id, kind, model, input_hash, state, reconcile_required, created_at_ms, updated_at_ms)
             VALUES ('job-2', 'missing-request', 'video-user', 'video', 'mock-video', zeroblob(32), 'queued', 0, 1, 1)",
            [],
        )
        .is_err());
    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn video_preflight_creates_one_job_and_attempt_and_replays_idempotently() {
    let (store, admin, principal, dir) = fixture();
    store
        .upsert_cost_policy(CostPolicy {
            id: "video-policy".into(),
            endpoint: "videos".into(),
            model_pattern: "mock-video".into(),
            resource_kind: "video_job".into(),
            reserve_amount: 4,
            max_actual_amount: Some(4),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .grant(QuotaGrant {
            user_id: "video-user".into(),
            resource_kind: "video_job".into(),
            amount: 10,
            actor_user_id: "admin".into(),
            reason: "phase3c fixture".into(),
        })
        .unwrap();
    let mut account = RegisterUpstreamAccount::new(
        "video-account".into(),
        "mock-video".into(),
        "vault://phase3c/video-ref".into(),
    );
    account.capabilities.insert("video".into());
    account.max_concurrency = 1;
    store.upsert_upstream_account(account, &admin).unwrap();
    store
        .append_upstream_observation(UpstreamObservation::new(
            "video-observation".into(),
            "video-account".into(),
            "video_job".into(),
            Some(100),
            1,
            "reader".into(),
            ObservationStatus::Fresh,
            1_800_000_000_000,
            1_800_000_060_000,
            json!({"available": 100, "source": "reader"}),
        ))
        .unwrap();
    let scheduler = || SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: "video-user".into(),
                api_key_id: principal.key_id.clone(),
                protocol: "openai".into(),
                endpoint: "videos".into(),
                model: "mock-video".into(),
                idempotency_key: "video-idem-1".into(),
                body: json!({"model": "mock-video", "prompt": "redacted"}),
            },
            resource_kind: "video_job".into(),
            amount: 4,
            ttl_ms: 10_000,
        },
        provider_hint: Some("mock-video".into()),
        required_capabilities: vec!["video".into()],
        region: None,
        predicted_units: 4,
        safety_margin_units: 0,
        observation_max_age_ms: 60_000,
        allowed_accounts: Some(vec!["video-account".into()]),
        dedicated_account: None,
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms: 1_800_000_000_000,
        lease_ttl_ms: 60_000,
        reconcile_ttl_ms: 600_000,
    };
    let first = store
        .preflight_video_job(
            &principal,
            scheduler(),
            CreateVideoJobInput {
                id: "job-video-1".into(),
                input_hash: vec![7; 32],
            },
        )
        .unwrap();
    let (job_id, lease_id) = match first {
        VideoJobLeaseResult::Acquired { job, attempt, lease } => {
            assert_eq!(job.state, aiwork_core::JobState::Queued);
            assert_eq!(job.user_id, "video-user");
            assert_eq!(attempt.state, aiwork_core::JobAttemptState::Queued);
            assert_eq!(attempt.account_ref, "video-account");
            (job.id, lease.lease_id)
        }
        other => panic!("expected acquired video job, got {other:?}"),
    };
    let running = store
        .mark_video_job_running(&principal, &job_id, 1_800_000_000_001)
        .unwrap();
    assert_eq!(running.state, aiwork_core::JobState::Running);
    let settled = store
        .settle_upstream_lease(
            &principal,
            &lease_id,
            LeaseOutcome::Success {
                actual_units: Some(4),
                upstream_request_ref: Some("mock-upstream-request".into()),
                now_ms: 1_800_000_000_002,
            },
        )
        .unwrap();
    assert_eq!(settled.state, LeaseState::Succeeded);
    assert_eq!(
        store
            .video_job_for_user(&principal, &job_id)
            .unwrap()
            .unwrap()
            .state,
        aiwork_core::JobState::Succeeded
    );
    assert_eq!(
        store
            .video_job_attempt_for_user(&principal, &job_id)
            .unwrap()
            .unwrap()
            .state,
        aiwork_core::JobAttemptState::Succeeded
    );
    assert_eq!(store.balance("video-user", "video_job").unwrap().available, 6);
    let replay = store
        .preflight_video_job(
            &principal,
            scheduler(),
            CreateVideoJobInput {
                id: "job-video-different-id-is-ignored-on-replay".into(),
                input_hash: vec![7; 32],
            },
        )
        .unwrap();
    match replay {
        VideoJobLeaseResult::Replay { job, attempt, lease } => {
            assert_eq!(job.id, job_id);
            assert_eq!(attempt.lease_id, lease_id);
            assert_eq!(lease.id, lease_id);
        }
        other => panic!("expected replay video job, got {other:?}"),
    }
    assert_eq!(store.count_rows("jobs").unwrap(), 1);
    assert_eq!(store.count_rows("job_attempts").unwrap(), 1);
    assert_eq!(store.count_rows("quota_reservations").unwrap(), 1);
    assert_eq!(store.count_rows("upstream_leases").unwrap(), 1);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cancel_request_is_persisted_without_releasing_until_confirmed() {
    let (store, admin, principal, dir) = fixture();
    prepare_video_scheduler(&store, &admin);
    let acquired = store
        .preflight_video_job(
            &principal,
            scheduler_request(&principal, "video-cancel", 1_800_000_000_000),
            CreateVideoJobInput {
                id: "job-video-cancel".into(),
                input_hash: vec![8; 32],
            },
        )
        .unwrap();
    let (job_id, request_id) = match acquired {
        VideoJobLeaseResult::Acquired { job, .. } => (job.id, job.request_id),
        other => panic!("expected acquired video job, got {other:?}"),
    };
    let audit_before_cancel = store.count_rows("audit_events").unwrap();
    let canceled = store
        .request_video_cancel(&principal, &job_id, 1_800_000_000_001)
        .unwrap();
    assert_eq!(canceled.state, aiwork_core::JobState::CancelRequested);
    assert_eq!(
        store
            .video_job_attempt_for_user(&principal, &job_id)
            .unwrap()
            .unwrap()
            .state,
        aiwork_core::JobAttemptState::CancelRequested
    );
    assert_eq!(store.request_state(&request_id).unwrap(), aiwork_core::RequestState::CancelRequested);
    let balance = store.balance("video-user", "video_job").unwrap();
    assert_eq!(balance.available, 6);
    assert_eq!(balance.held, 4);
    let audit_after_cancel = store.count_rows("audit_events").unwrap();
    assert_eq!(audit_after_cancel, audit_before_cancel + 1);
    let replay = store
        .request_video_cancel(&principal, &job_id, 1_800_000_000_002)
        .unwrap();
    assert_eq!(replay.state, aiwork_core::JobState::CancelRequested);
    assert_eq!(store.count_rows("audit_events").unwrap(), audit_after_cancel);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn expired_video_lease_recovers_job_to_unknown_without_release() {
    let (store, admin, principal, dir) = fixture();
    prepare_video_scheduler(&store, &admin);
    let acquired = store
        .preflight_video_job(
            &principal,
            scheduler_request(&principal, "video-recovery", 1_800_000_000_000),
            CreateVideoJobInput {
                id: "job-video-recovery".into(),
                input_hash: vec![9; 32],
            },
        )
        .unwrap();
    let (job_id, request_id) = match acquired {
        VideoJobLeaseResult::Acquired { job, .. } => (job.id, job.request_id),
        other => panic!("expected acquired video job, got {other:?}"),
    };
    let recovered = store
        .recover_expired_upstream_leases(1_800_000_060_001)
        .unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].state, LeaseState::Unknown);
    let job = store.video_job_for_user(&principal, &job_id).unwrap().unwrap();
    assert_eq!(job.state, aiwork_core::JobState::Unknown);
    assert!(job.reconcile_required);
    assert_eq!(store.request_state(&request_id).unwrap(), aiwork_core::RequestState::Unknown);
    let balance = store.balance("video-user", "video_job").unwrap();
    assert_eq!(balance.available, 6);
    assert_eq!(balance.held, 4);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
