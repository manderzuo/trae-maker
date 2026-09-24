use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
};

use aiwork_core::{
    BeginRequest, BeginRequestInput, BillingQuote, BillingReceipt, BillingReceiptStatus,
    BillingReservationResult, BillingReceiptResult, CoreError, CoreStore, CreditAmount,
    KeyQuotaGrant, NewUser, Principal, UpstreamCreditSnapshot, UserRole,
};
use serde_json::json;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aiwork-core-upstream-credit-allocation-{}",
            rand::random::<u64>()
        ));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn setup() -> (TestDir, CoreStore, Principal, String, String) {
    let dir = TestDir::new();
    let store = CoreStore::open(dir.path()).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "admin".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    for (id, name) in [("user-a", "Studio A"), ("user-b", "Studio B")] {
        store
            .create_user(
                NewUser {
                    id: id.into(),
                    name: name.into(),
                    role: UserRole::User,
                },
                "admin",
            )
            .unwrap();
    }
    let admin_key = store
        .issue_api_key(
            "admin",
            "admin",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let key_a = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user-a",
            "Studio A",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
            &admin,
        )
        .unwrap();
    let key_b = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user-b",
            "Studio B",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
            &admin,
        )
        .unwrap();
    (dir, store, admin, key_a.id, key_b.id)
}

fn fresh_snapshot(total: &str) -> UpstreamCreditSnapshot {
    UpstreamCreditSnapshot {
        total: CreditAmount::parse(total, "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn grant(key_id: &str, amount: i64) -> KeyQuotaGrant {
    KeyQuotaGrant {
        api_key_id: key_id.into(),
        resource_kind: "credits".into(),
        amount,
        actor_user_id: "admin".into(),
        reason: "upstream allocation test".into(),
    }
}

#[test]
fn global_upstream_cap_counts_held_once_and_spans_different_users() {
    let (_dir, store, admin, key_a, key_b) = setup();
    let snapshot = fresh_snapshot("10.000000");
    let (allocated, remaining) = store
        .key_quota_allocate_from_upstream_as_admin(
            &admin,
            grant(&key_a, 10_000_000),
            snapshot.clone(),
        )
        .unwrap();
    assert_eq!(allocated.available, 10_000_000);
    assert_eq!(remaining, 0);

    let request = match store
        .begin_billed_request(BeginRequestInput {
            user_id: "user-a".into(),
            api_key_id: key_a.clone(),
            protocol: "openai".into(),
            endpoint: "/v1/chat/completions".into(),
            model: "text-model".into(),
            idempotency_key: "held-on-one-key".into(),
            body: json!({"model":"text-model","messages":[{"role":"user","content":"hello"}]}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) | BeginRequest::Existing(request) => request,
        BeginRequest::Conflict => panic!("unexpected idempotency conflict"),
    };
    let quote = BillingQuote {
        request_id: request.id.clone(),
        quote_id: "held-on-one-key-quote".into(),
        request_fingerprint: store
            .request_fingerprint_for_billing(&request.id)
            .unwrap(),
        endpoint: request.endpoint,
        model: request.model,
        max_credits: CreditAmount::parse("2.000000", "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: "upstream-quote-held-on-one-key".into(),
    };
    let reservation = store
        .reserve_credit_quote_with_upstream_snapshot(quote, snapshot.clone())
        .unwrap();
    assert!(matches!(reservation, BillingReservationResult::Created { .. }));
    let balance = store
        .key_quota_balance_as_admin(&admin, &key_a, "credits")
        .unwrap();
    assert_eq!(balance.available, 8_000_000);
    assert_eq!(balance.held, 2_000_000);

    let rejected = store.key_quota_allocate_from_upstream_as_admin(
        &admin,
        grant(&key_b, 1),
        snapshot,
    );
    assert!(matches!(
        rejected,
        Err(CoreError::UpstreamCreditLimitExceeded {
            available: 0,
            required: 1
        })
    ));
    assert!(matches!(
        store.key_quota_balance_as_admin(&admin, &key_b, "credits"),
        Err(CoreError::KeyQuotaNotConfigured { .. })
    ));
}

#[test]
fn simultaneous_allocations_cannot_exceed_the_same_fresh_upstream_total() {
    let (_dir, store, admin, key_a, key_b) = setup();
    let store = Arc::new(store);
    let barrier = Arc::new(Barrier::new(3));
    let snapshot = fresh_snapshot("10.000000");
    let mut workers = Vec::new();
    for key_id in [key_a, key_b] {
        let store = Arc::clone(&store);
        let admin = admin.clone();
        let barrier = Arc::clone(&barrier);
        let snapshot = snapshot.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            store.key_quota_allocate_from_upstream_as_admin(
                &admin,
                grant(&key_id, 7_000_000),
                snapshot,
            )
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(CoreError::UpstreamCreditLimitExceeded {
                    available: 3_000_000,
                    required: 7_000_000
                })
            ))
            .count(),
        1
    );
}

#[test]
fn expired_upstream_snapshot_cannot_change_a_key_balance() {
    let (_dir, store, admin, key_a, _) = setup();
    let stale = UpstreamCreditSnapshot {
        total: CreditAmount::parse("10.000000", "credits").unwrap(),
        updated_at_ms: chrono::Utc::now().timestamp_millis() - 300_001,
    };
    let result = store.key_quota_allocate_from_upstream_as_admin(
        &admin,
        grant(&key_a, 1_000_000),
        stale,
    );
    assert!(matches!(result, Err(CoreError::UpstreamCreditsUnavailable { .. })));
    assert!(matches!(
        store.key_quota_balance_as_admin(&admin, &key_a, "credits"),
        Err(CoreError::KeyQuotaNotConfigured { .. })
    ));
}

#[test]
fn an_over_quote_debt_on_one_key_cannot_increase_another_keys_allocatable_balance() {
    let (_dir, store, admin, key_a, key_b) = setup();
    let ten_credits = fresh_snapshot("10.000000");
    store
        .key_quota_allocate_from_upstream_as_admin(
            &admin,
            grant(&key_a, 5_000_000),
            ten_credits.clone(),
        )
        .unwrap();
    let request = match store
        .begin_billed_request(BeginRequestInput {
            user_id: "user-a".into(),
            api_key_id: key_a.clone(),
            protocol: "openai".into(),
            endpoint: "/v1/chat/completions".into(),
            model: "text-model".into(),
            idempotency_key: "over-quote-isolation".into(),
            body: json!({"model":"text-model","messages":[{"role":"user","content":"hello"}]}),
        })
        .unwrap()
    {
        BeginRequest::Created(request) | BeginRequest::Existing(request) => request,
        BeginRequest::Conflict => panic!("unexpected idempotency conflict"),
    };
    let quote = BillingQuote {
        request_id: request.id.clone(),
        quote_id: "over-quote-isolation-quote".into(),
        request_fingerprint: store
            .request_fingerprint_for_billing(&request.id)
            .unwrap(),
        endpoint: request.endpoint,
        model: request.model,
        max_credits: CreditAmount::parse("5", "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: "verified-quote-source".into(),
    };
    assert!(matches!(
        store
            .reserve_credit_quote_with_upstream_snapshot(quote, ten_credits)
            .unwrap(),
        BillingReservationResult::Created { .. }
    ));
    assert!(matches!(
        store
            .apply_credit_receipt(BillingReceipt {
                request_id: request.id,
                status: BillingReceiptStatus::Final,
                actual_credits: Some(CreditAmount::parse("6", "credits").unwrap()),
                unit: "credits".into(),
                source_ref: "verified-final-source".into(),
                task_ref: None,
                observed_at_ms: chrono::Utc::now().timestamp_millis(),
            })
            .unwrap(),
        BillingReceiptResult::Settled { over_quote: true, .. }
    ));

    let allocation = store.key_quota_allocate_from_upstream_as_admin(
        &admin,
        grant(&key_b, 1),
        fresh_snapshot("0"),
    );
    assert!(matches!(
        allocation,
        Err(CoreError::UpstreamCreditLimitExceeded {
            available: 0,
            required: 1
        })
    ));
    assert!(matches!(
        store.key_quota_balance_as_admin(&admin, &key_b, "credits"),
        Err(CoreError::KeyQuotaNotConfigured { .. })
    ));
}
