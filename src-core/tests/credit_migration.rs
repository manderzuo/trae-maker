use std::{fs, path::PathBuf};

use aiwork_core::{CoreStore, CURRENT_SCHEMA_VERSION};
use rand::random;
use rusqlite::Connection;

fn prepare_v17_database(label: &str) -> (CoreStore, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-credit-migration-{label}-{}",
        random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();

    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    drop(store);

    let database = dir.join("data").join("core.sqlite3");
    let connection = Connection::open(database).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE api_keys DROP COLUMN secret_key_version;
             ALTER TABLE api_keys DROP COLUMN secret_ciphertext;
             UPDATE schema_meta SET value = '17' WHERE key = 'schema_version';
             INSERT INTO users (id, name, role, status, created_at_ms, updated_at_ms)
               VALUES ('credit-user', 'Credit User', 'user', 'active', 1, 1);
             INSERT INTO api_keys
               (id, user_id, name, prefix, key_digest, scopes_json, status, created_at_ms)
               VALUES ('credit-key', 'credit-user', 'credit key', 'ck_test', X'010203', '[]', 'active', 1);
             INSERT INTO quota_ledger
               (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id, api_key_id, created_at_ms)
               VALUES ('credit-ledger', 'credit-user', 'credits', 'adjust', 7, -2, 'credit-request', 'credit-key', 1),
                      ('other-ledger', 'credit-user', 'chat_request', 'adjust', 4, -4, NULL, NULL, 1);
             INSERT INTO quota_reservations
               (id, user_id, request_id, resource_kind, amount, state, expires_at_ms, created_at_ms)
               VALUES ('credit-reservation', 'credit-user', 'credit-request', 'credits', 3, 'held', 100, 1),
                      ('other-reservation', 'credit-user', 'other-request', 'chat_request', 2, 'held', 100, 1);
             INSERT INTO cost_policies
               (id, endpoint, model_pattern, resource_kind, reserve_amount, max_actual_amount, version, enabled)
               VALUES ('credit-policy', '/v1/chat/completions', '*', 'credits', 5, 12, 1, 1),
                      ('credit-policy-unbounded', '/v1/videos', '*', 'credits', 1, NULL, 1, 1),
                      ('other-policy', '/v1/chat/completions', '*', 'chat_request', 8, 10, 1, 1);",
        )
        .unwrap();
    drop(connection);

    (CoreStore::open(&dir).unwrap(), dir)
}

#[test]
fn v17_migration_scales_only_credit_values_to_microcredits() {
    let (store, dir) = prepare_v17_database("scale");

    store.migrate().unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let ledger: (String, String, String, String, i64, i64, String, String) = connection
        .query_row(
            "SELECT entry_id, user_id, resource_kind, event_kind, amount, delta, request_id, api_key_id
             FROM quota_ledger WHERE entry_id = 'credit-ledger'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        ledger,
        (
            "credit-ledger".into(),
            "credit-user".into(),
            "credits".into(),
            "adjust".into(),
            7_000_000,
            -2_000_000,
            "credit-request".into(),
            "credit-key".into(),
        )
    );

    let non_credit_ledger: (i64, i64) = connection
        .query_row(
            "SELECT amount, delta FROM quota_ledger WHERE entry_id = 'other-ledger'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(non_credit_ledger, (4, -4));

    let reservation: (String, String, i64, String) = connection
        .query_row(
            "SELECT request_id, resource_kind, amount, state FROM quota_reservations
             WHERE id = 'credit-reservation'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(reservation, ("credit-request".into(), "credits".into(), 3_000_000, "held".into()));

    let non_credit_reservation: i64 = connection
        .query_row(
            "SELECT amount FROM quota_reservations WHERE id = 'other-reservation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(non_credit_reservation, 2);

    let policy: (i64, Option<i64>) = connection
        .query_row(
            "SELECT reserve_amount, max_actual_amount FROM cost_policies WHERE id = 'credit-policy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(policy, (5_000_000, Some(12_000_000)));

    let unbounded_policy: (i64, Option<i64>) = connection
        .query_row(
            "SELECT reserve_amount, max_actual_amount FROM cost_policies WHERE id = 'credit-policy-unbounded'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(unbounded_policy, (1_000_000, None));

    let non_credit_policy: (i64, Option<i64>) = connection
        .query_row(
            "SELECT reserve_amount, max_actual_amount FROM cost_policies WHERE id = 'other-policy'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(non_credit_policy, (8, Some(10)));

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v17_migration_overflow_rolls_back_all_scaled_values_and_version() {
    let (store, dir) = prepare_v17_database("overflow");
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE cost_policies SET reserve_amount = 9223372036855 WHERE id = 'credit-policy'",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(store.migrate().is_err());
    assert_eq!(store.schema_version().unwrap(), 17);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let ledger_amount: i64 = connection
        .query_row(
            "SELECT amount FROM quota_ledger WHERE entry_id = 'credit-ledger'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let reservation_amount: i64 = connection
        .query_row(
            "SELECT amount FROM quota_reservations WHERE id = 'credit-reservation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_amount, 7);
    assert_eq!(reservation_amount, 3);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v17_migration_invalid_credit_storage_rolls_back_all_scaled_values_and_version() {
    let (store, dir) = prepare_v17_database("invalid-storage");
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "UPDATE cost_policies SET max_actual_amount = 'not-an-integer' WHERE id = 'credit-policy'",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(store.migrate().is_err());
    assert_eq!(store.schema_version().unwrap(), 17);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let ledger_amount: i64 = connection
        .query_row(
            "SELECT amount FROM quota_ledger WHERE entry_id = 'credit-ledger'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let reservation_amount: i64 = connection
        .query_row(
            "SELECT amount FROM quota_reservations WHERE id = 'credit-reservation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(ledger_amount, 7);
    assert_eq!(reservation_amount, 3);

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
