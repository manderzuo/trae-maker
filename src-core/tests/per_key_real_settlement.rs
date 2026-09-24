use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    BeginRequest, BeginRequestInput, BillingQuote, BillingReceipt, BillingReceiptResult,
    BillingReceiptStatus, BillingReservationResult, CoreError, CoreStore, CreditAmount,
    KeyQuotaGrant, NewUser, Principal, RequestHandle, UserRole,
};
use rusqlite::Connection;
use serde_json::json;

fn test_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-per-key-settlement-{label}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup(label: &str) -> (CoreStore, PathBuf, String, String, Principal) {
    let dir = test_dir(label);
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    store
        .create_user(
            NewUser { id: "user".into(), name: "User".into(), role: UserRole::User },
            "admin",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let first_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user", "first", BTreeSet::from(["chat:invoke".into()]), 4, &admin,
        )
        .unwrap();
    let second_key = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user", "second", BTreeSet::from(["chat:invoke".into()]), 4, &admin,
        )
        .unwrap();
    for key in [&first_key, &second_key] {
        store
            .key_quota_grant_as_admin(
                &admin,
                KeyQuotaGrant {
                    api_key_id: key.id.clone(),
                    resource_kind: "credits".into(),
                    amount: 10_000_000,
                    actor_user_id: "ignored".into(),
                    reason: "settlement test fixture".into(),
                },
            )
            .unwrap();
    }
    (store, dir, first_key.id, second_key.id, admin)
}

fn begin(store: &CoreStore, key_id: &str, idempotency_key: &str) -> aiwork_core::RequestHandle {
    let input = BeginRequestInput {
        user_id: "user".into(),
        api_key_id: key_id.into(),
        protocol: "openai".into(),
        endpoint: "/v1/chat/completions".into(),
        model: "text-model".into(),
        idempotency_key: idempotency_key.into(),
        body: json!({"model":"text-model","messages":[{"role":"user","content":"hello"}]}),
    };
    match store.begin_billed_request(input).unwrap() {
        BeginRequest::Created(request) | BeginRequest::Existing(request) => request,
        BeginRequest::Conflict => panic!("unexpected idempotency conflict"),
    }
}

fn quote(store: &CoreStore, request: &RequestHandle, quote_id: &str, max_credits: &str) -> BillingQuote {
    BillingQuote {
        request_id: request.id.clone(),
        quote_id: quote_id.into(),
        request_fingerprint: store.request_fingerprint_for_billing(&request.id).unwrap(),
        endpoint: request.endpoint.clone(),
        model: request.model.clone(),
        max_credits: CreditAmount::parse(max_credits, "credits").unwrap(),
        unit: "credits".into(),
        expires_at_ms: chrono::Utc::now().timestamp_millis() + 60_000,
        source_ref: format!("upstream-quote-{quote_id}"),
    }
}

fn receipt(
    request_id: &str,
    status: BillingReceiptStatus,
    actual_credits: Option<&str>,
    source_ref: &str,
) -> BillingReceipt {
    BillingReceipt {
        request_id: request_id.into(),
        status,
        actual_credits: actual_credits.map(|value| CreditAmount::parse(value, "credits").unwrap()),
        unit: "credits".into(),
        source_ref: source_ref.into(),
        task_ref: None,
        observed_at_ms: chrono::Utc::now().timestamp_millis(),
    }
}

fn reserve(store: &CoreStore, request: &RequestHandle, quote_id: &str, max_credits: &str) -> String {
    match store.reserve_credit_quote(quote(store, request, quote_id, max_credits)).unwrap() {
        BillingReservationResult::Created { reservation, .. } => reservation.id,
        BillingReservationResult::Existing { reservation, .. } => reservation.id,
        other => panic!("unexpected reservation result: {other:?}"),
    }
}

#[test]
fn core_generated_request_and_idempotency_are_isolated_by_key() {
    let (store, dir, first_key, second_key, _) = setup("identity");
    let first = begin(&store, &first_key, "same-client-key");
    let retry = begin(&store, &first_key, "same-client-key");
    let second = begin(&store, &second_key, "same-client-key");

    assert_eq!(first.id, retry.id);
    assert_ne!(first.id, second.id);
    assert_eq!(first.api_key_id, first_key);
    assert_eq!(second.api_key_id, second_key);
    assert!(matches!(
        store.begin_billed_request(BeginRequestInput {
            user_id: "user".into(),
            api_key_id: first.api_key_id.clone(),
            protocol: "openai".into(),
            endpoint: first.endpoint.clone(),
            model: first.model.clone(),
            idempotency_key: "same-client-key".into(),
            body: json!({"model":"text-model","messages":[{"role":"user","content":"different"}]}),
        }).unwrap(),
        BeginRequest::Conflict
    ));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v18_migration_rekeys_existing_idempotency_records_to_their_original_key() {
    let (store, dir, first_key, _, _) = setup("v18-idempotency");
    let request = begin(&store, &first_key, "legacy-idempotency");
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE idempotency_keys SET scope = 'user:/v1/chat/completions'
             WHERE request_id = ?1",
            [&request.id],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE schema_meta SET value = '18' WHERE key = 'schema_version'",
            [],
        )
        .unwrap();
    connection
        .execute_batch(
            "ALTER TABLE api_keys DROP COLUMN secret_key_version;
             ALTER TABLE api_keys DROP COLUMN secret_ciphertext;",
        )
        .unwrap();
    drop(connection);

    store.migrate().unwrap();
    let replay = begin(&store, &first_key, "legacy-idempotency");
    assert_eq!(replay.id, request.id);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let scope: String = connection
        .query_row(
            "SELECT scope FROM idempotency_keys WHERE request_id = ?1",
            [&request.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(scope, format!("user:{first_key}:/v1/chat/completions"));

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn quote_must_match_request_and_repeat_quote_never_creates_a_second_hold() {
    let (store, dir, first_key, _, admin) = setup("quote");
    let request = begin(&store, &first_key, "quote-idem");
    let mut invalid_quote = quote(&store, &request, "quote-1", "5");
    invalid_quote.model = "different-model".into();
    assert!(matches!(
        store.reserve_credit_quote(invalid_quote),
        Err(CoreError::BillingQuoteMismatch { .. })
    ));
    assert!(store.reservation_for_request(&request.id).unwrap().is_none());

    let mut invalid_fingerprint = quote(&store, &request, "bad-fingerprint", "5");
    invalid_fingerprint.request_fingerprint = "not-the-core-request-fingerprint".into();
    assert!(matches!(
        store.reserve_credit_quote(invalid_fingerprint),
        Err(CoreError::BillingQuoteMismatch { .. })
    ));
    let mut expired_quote = quote(&store, &request, "expired-quote", "5");
    expired_quote.expires_at_ms = chrono::Utc::now().timestamp_millis() - 1;
    assert!(matches!(
        store.reserve_credit_quote(expired_quote),
        Err(CoreError::BillingQuoteExpired { .. })
    ));

    let valid_quote = quote(&store, &request, "quote-1", "5");
    let first = store.reserve_credit_quote(valid_quote.clone()).unwrap();
    let second = store.reserve_credit_quote(valid_quote).unwrap();
    let (first_reservation, second_reservation) = match (first, second) {
        (BillingReservationResult::Created { reservation: first, .. },
         BillingReservationResult::Existing { reservation: second, .. }) => (first, second),
        other => panic!("unexpected idempotent quote results: {other:?}"),
    };
    assert_eq!(first_reservation.id, second_reservation.id);
    assert_eq!(first_reservation.amount, 5_000_000);
    assert_eq!(
        store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap().held,
        5_000_000
    );

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn recovery_scan_survives_reopening_and_returns_only_held_receipt_work() {
    let (store, dir, first_key, _, _) = setup("restart-scan");
    let request = begin(&store, &first_key, "restart-scan-idem");
    reserve(&store, &request, "restart-scan-quote", "5");

    drop(store);
    let reopened = CoreStore::open(&dir).unwrap();
    reopened.migrate().unwrap();
    let recoverable = reopened.recoverable_billing_requests(20).unwrap();
    assert_eq!(
        recoverable,
        vec![aiwork_core::RecoverableBillingRequest {
            request_id: request.id,
            endpoint: "/v1/chat/completions".into(),
        }]
    );

    drop(reopened);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn final_receipt_settles_actual_amount_once_and_conflict_blocks_only_its_key() {
    let (store, dir, first_key, second_key, admin) = setup("receipt");
    let request = begin(&store, &first_key, "receipt-idem");
    let reservation_id = reserve(&store, &request, "quote-2", "5");
    let final_receipt = receipt(
        &request.id,
        BillingReceiptStatus::Final,
        Some("2.375"),
        "upstream-request-2",
    );
    assert!(matches!(
        store.apply_credit_receipt(final_receipt.clone()).unwrap(),
        BillingReceiptResult::Settled { actual_credits, over_quote: false, .. }
            if actual_credits == CreditAmount::parse("2.375", "credits").unwrap()
    ));
    assert!(matches!(
        store.apply_credit_receipt(final_receipt).unwrap(),
        BillingReceiptResult::Duplicate
    ));
    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &request.id,
            BillingReceiptStatus::Final,
            Some("2.5"),
            "conflicting-upstream-request",
        )).unwrap(),
        BillingReceiptResult::Conflict
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (commit_count, actual, reservation_state, reconcile_required): (i64, i64, String, i64) = connection
        .query_row(
            "SELECT
               (SELECT COUNT(*) FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'commit' AND budget_account_id = (SELECT key_budget_account_id FROM quota_reservations WHERE id = ?2)),
               (SELECT amount FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'commit' AND budget_account_id = (SELECT key_budget_account_id FROM quota_reservations WHERE id = ?2)),
               (SELECT state FROM quota_reservations WHERE id = ?2),
               (SELECT reconcile_required FROM billing_settlements WHERE request_id = ?1)",
            rusqlite::params![&request.id, &reservation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((commit_count, actual, reservation_state.as_str(), reconcile_required), (1, 2_375_000, "committed", 1));
    drop(connection);
    let balance = store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (7_625_000, 0, 2_375_000));

    let blocked = begin(&store, &first_key, "blocked-after-conflict");
    assert!(matches!(
        store.reserve_credit_quote(quote(&store, &blocked, "quote-blocked", "1")),
        Err(CoreError::ApiKeyBillingBlocked { .. })
    ));
    let unaffected = begin(&store, &second_key, "other-key-unaffected");
    assert!(matches!(
        store.reserve_credit_quote(quote(&store, &unaffected, "quote-other-key", "1")).unwrap(),
        BillingReservationResult::Created { .. }
    ));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn receipt_rejects_a_reservation_bound_to_another_key() {
    let (store, dir, first_key, second_key, _) = setup("reservation-key-mismatch");
    let request = begin(&store, &first_key, "reservation-key-mismatch");
    reserve(&store, &request, "quote-key-mismatch", "5");

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE quota_reservations SET api_key_id = ?1 WHERE request_id = ?2",
            rusqlite::params![&second_key, &request.id],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &request.id,
            BillingReceiptStatus::Final,
            Some("2"),
            "mismatched-reservation-key",
        )),
        Err(CoreError::BillingReceiptInvalid { .. })
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let reservation_state: String = connection
        .query_row(
            "SELECT state FROM quota_reservations WHERE request_id = ?1",
            [&request.id],
            |row| row.get(0),
        )
        .unwrap();
    let receipt_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM billing_receipts WHERE request_id = ?1",
            [&request.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(reservation_state, "held");
    assert_eq!(receipt_count, 0);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn over_quote_debits_the_proven_amount_and_unknown_keeps_the_hold_until_final() {
    let (store, dir, first_key, second_key, _) = setup("overquote");
    let request = begin(&store, &first_key, "overquote-idem");
    let reservation_id = reserve(&store, &request, "quote-3", "5");
    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &request.id,
            BillingReceiptStatus::Final,
            Some("6"),
            "upstream-request-3",
        )).unwrap(),
        BillingReceiptResult::Settled { over_quote: true, .. }
    ));

    let unknown = begin(&store, &second_key, "unknown-idem");
    let unknown_reservation_id = reserve(&store, &unknown, "quote-4", "3");
    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &unknown.id,
            BillingReceiptStatus::Unknown,
            None,
            "upstream-pending-4",
        )).unwrap(),
        BillingReceiptResult::Pending
    ));
    let held: i64 = Connection::open(dir.join("data").join("core.sqlite3"))
        .unwrap()
        .query_row(
            "SELECT amount FROM quota_reservations WHERE id = ?1 AND state = 'unknown'",
            [&unknown_reservation_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(held, 3_000_000);
    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &unknown.id,
            BillingReceiptStatus::Final,
            Some("1.25"),
            "upstream-final-4",
        )).unwrap(),
        BillingReceiptResult::Settled { over_quote: false, .. }
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (actual, state, blocks, reconcile_required): (i64, String, i64, i64) = connection
        .query_row(
            "SELECT
               (SELECT amount FROM quota_ledger WHERE request_id = ?1 AND event_kind = 'commit' AND budget_account_id = (SELECT key_budget_account_id FROM quota_reservations WHERE id = ?2)),
               (SELECT state FROM quota_reservations WHERE id = ?2),
               (SELECT COUNT(*) FROM api_key_billing_blocks WHERE key_id = ?3),
               (SELECT reconcile_required FROM billing_settlements WHERE request_id = ?1)",
            rusqlite::params![&request.id, &reservation_id, &first_key],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!((actual, state.as_str(), blocks, reconcile_required), (6_000_000, "committed", 1, 1));

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_no_charge_final_receipt_releases_the_full_reservation() {
    let (store, dir, first_key, _, admin) = setup("no-charge");
    let request = begin(&store, &first_key, "no-charge-idem");
    let reservation_id = reserve(&store, &request, "quote-no-charge", "4");

    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &request.id,
            BillingReceiptStatus::FailedNoCharge,
            None,
            "upstream-rejected-before-charge",
        )).unwrap(),
        BillingReceiptResult::Settled { actual_credits, over_quote: false, .. }
            if actual_credits == CreditAmount::default()
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (actual, state): (i64, String) = connection
        .query_row(
            "SELECT
               (SELECT actual_credits FROM billing_settlements WHERE request_id = ?1),
               (SELECT state FROM quota_reservations WHERE id = ?2)",
            rusqlite::params![&request.id, &reservation_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((actual, state.as_str()), (0, "committed"));
    drop(connection);
    let balance = store.key_quota_balance_as_admin(&admin, &first_key, "credits").unwrap();
    assert_eq!((balance.available, balance.held, balance.settled), (10_000_000, 0, 0));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn invalid_unit_never_settles_and_keeps_hold_for_a_later_verified_receipt() {
    let (store, dir, first_key, _, _) = setup("invalid-unit");
    let request = begin(&store, &first_key, "invalid-unit-idem");
    let reservation_id = reserve(&store, &request, "quote-invalid-unit", "3");
    let mut invalid = receipt(
        &request.id,
        BillingReceiptStatus::Final,
        Some("1"),
        "unverified-source",
    );
    invalid.unit = "tokens".into();
    assert!(matches!(
        store.apply_credit_receipt(invalid).unwrap(),
        BillingReceiptResult::Pending
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let (state, settlement_count, receipt_status): (String, i64, String) = connection
        .query_row(
            "SELECT
               (SELECT state FROM quota_reservations WHERE id = ?1),
               (SELECT COUNT(*) FROM billing_settlements WHERE request_id = ?2),
               (SELECT status FROM billing_receipts WHERE request_id = ?2)",
            rusqlite::params![&reservation_id, &request.id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!((state.as_str(), settlement_count, receipt_status.as_str()), ("unknown", 0, "unverified"));
    drop(connection);

    assert!(matches!(
        store.apply_credit_receipt(receipt(
            &request.id,
            BillingReceiptStatus::Final,
            Some("1.25"),
            "verified-source",
        )).unwrap(),
        BillingReceiptResult::Settled { over_quote: false, .. }
    ));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
