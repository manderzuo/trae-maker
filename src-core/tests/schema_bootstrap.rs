use std::{fs, path::PathBuf};

use aiwork_core::{CoreError, CoreStore, CORE_DB_FILE, CURRENT_SCHEMA_VERSION};
use rusqlite::Connection;

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn bootstrap_creates_authoritative_schema() {
    let dir = test_dir("schema");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();

    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    assert!(dir.join("data").join(CORE_DB_FILE).is_file());
    assert!(store.foreign_keys_enabled().unwrap());

    for table in [
        "schema_meta",
        "users",
        "api_keys",
        "cost_policies",
        "quota_ledger",
        "quota_reservations",
        "requests",
        "idempotency_keys",
        "upstream_observations",
        "audit_events",
    ] {
        assert_eq!(store.table_count(table).unwrap(), 1, "missing table {table}");
    }

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn migrate_is_idempotent_and_rejects_future_schema_versions() {
    let dir = test_dir("idempotent");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    drop(store);
    fs::remove_dir_all(dir).unwrap();

    let future_dir = test_dir("future-version");
    let database_dir = future_dir.join("data");
    fs::create_dir_all(&database_dir).unwrap();
    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);\
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '2');",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&future_dir).unwrap();
    assert!(matches!(
        store.migrate(),
        Err(CoreError::UnsupportedSchemaVersion { version: 2 })
    ));
    drop(store);
    fs::remove_dir_all(future_dir).unwrap();
}

#[test]
fn failed_migration_rolls_back_schema_bootstrap() {
    let dir = test_dir("rollback");
    let database_dir = dir.join("data");
    fs::create_dir_all(&database_dir).unwrap();
    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    connection.execute_batch("CREATE TABLE users (id TEXT PRIMARY KEY);").unwrap();
    drop(connection);

    let store = CoreStore::open(&dir).unwrap();
    assert!(store.migrate().is_err());
    drop(store);

    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    let schema_meta_count: u32 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'schema_meta'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(schema_meta_count, 0);
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}
