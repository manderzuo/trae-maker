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
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '5');",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&future_dir).unwrap();
    assert!(matches!(
        store.migrate(),
        Err(CoreError::UnsupportedSchemaVersion { version: 5 })
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

fn create_v1_database(dir: &PathBuf, state: &str) {
    let database_dir = dir.join("data");
    fs::create_dir_all(&database_dir).unwrap();
    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '1');
             CREATE TABLE users (id TEXT PRIMARY KEY);
             CREATE TABLE api_keys (id TEXT PRIMARY KEY, user_id TEXT NOT NULL REFERENCES users(id));
             CREATE TABLE requests (
               id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL REFERENCES users(id),
               api_key_id TEXT NOT NULL REFERENCES api_keys(id),
               protocol TEXT NOT NULL,
               endpoint TEXT NOT NULL,
               model TEXT NOT NULL,
               request_hash BLOB NOT NULL,
               state TEXT NOT NULL,
               result_status INTEGER,
               error_code TEXT,
               created_at_ms INTEGER NOT NULL,
               updated_at_ms INTEGER NOT NULL
             );
             CREATE INDEX requests_by_model ON requests(model);
             CREATE TABLE idempotency_keys (
               scope TEXT NOT NULL,
               client_key TEXT NOT NULL,
               request_hash BLOB NOT NULL,
               request_id TEXT NOT NULL REFERENCES requests(id),
               created_at_ms INTEGER NOT NULL,
               PRIMARY KEY(scope, client_key)
             );
             INSERT INTO users (id) VALUES ('u1');
             INSERT INTO api_keys (id, user_id) VALUES ('key-1', 'u1');",
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO requests \
             (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms) \
             VALUES ('request-1', 'u1', 'key-1', 'openai', '/v1/chat/completions', 'mock-1', X'01', ?1, 1, 1)",
            [state],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO idempotency_keys (scope, client_key, request_hash, request_id, created_at_ms) \
             VALUES ('u1:/v1/chat/completions', 'idem-1', X'01', 'request-1', 1)",
            [],
        )
        .unwrap();
}

#[test]
fn migrates_v1_requests_to_a_checked_state_machine_without_losing_data_or_indexes() {
    let dir = test_dir("v1-requests");
    create_v1_database(&dir, "queued");
    let store = CoreStore::open(&dir).unwrap();

    store.migrate().unwrap();
    store.migrate().unwrap();

    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    drop(store);
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let state: String = connection
        .query_row("SELECT state FROM requests WHERE id = 'request-1'", [], |row| row.get(0))
        .unwrap();
    let idempotency_request_id: String = connection
        .query_row(
            "SELECT request_id FROM idempotency_keys WHERE scope = 'u1:/v1/chat/completions' AND client_key = 'idem-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let index_exists: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'index' AND name = 'requests_by_model')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let foreign_key_targets = {
        let mut statement = connection.prepare("PRAGMA foreign_key_list('idempotency_keys')").unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(2))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };

    assert_eq!(state, "queued");
    assert_eq!(idempotency_request_id, "request-1");
    assert!(index_exists);
    assert_eq!(foreign_key_targets, vec!["requests"]);
}

#[test]
fn rejects_and_rolls_back_a_v1_database_with_an_illegal_request_state() {
    let dir = test_dir("v1-illegal-state");
    create_v1_database(&dir, "invented");
    let store = CoreStore::open(&dir).unwrap();

    assert!(store.migrate().is_err());
    assert_eq!(store.schema_version().unwrap(), 1);
    drop(store);
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let state: String = connection
        .query_row("SELECT state FROM requests WHERE id = 'request-1'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(state, "invented");
}

#[test]
fn migrates_v3_assets_as_unverified_until_physical_validation() {
    let dir = test_dir("v3-assets");
    let database_dir = dir.join("data");
    fs::create_dir_all(&database_dir).unwrap();
    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '3');
             CREATE TABLE users (id TEXT PRIMARY KEY);
             CREATE TABLE legacy_assets (
               id TEXT PRIMARY KEY,
               owner_key_id TEXT NOT NULL,
               user_id TEXT NOT NULL REFERENCES users(id),
               filename TEXT NOT NULL,
               mime_type TEXT NOT NULL,
               extension TEXT NOT NULL,
               size INTEGER NOT NULL CHECK(size >= 0),
               content_sha256 TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL,
               expires_at_ms INTEGER NOT NULL,
               migration_id TEXT NOT NULL,
               actor_user_id TEXT NOT NULL REFERENCES users(id),
               reason TEXT NOT NULL
             );
             CREATE TABLE legacy_observations (
               id TEXT PRIMARY KEY,
               account_ref TEXT NOT NULL,
               resource_kind TEXT NOT NULL,
               value_json TEXT NOT NULL,
               source TEXT NOT NULL CHECK(source = 'json_cache'),
               observed_at_ms INTEGER NOT NULL,
               migration_id TEXT NOT NULL,
               actor_user_id TEXT NOT NULL REFERENCES users(id),
               reason TEXT NOT NULL
             );
             INSERT INTO users (id) VALUES ('admin');
             INSERT INTO legacy_assets
               (id, owner_key_id, user_id, filename, mime_type, extension, size,
                content_sha256, created_at_ms, expires_at_ms, migration_id,
                actor_user_id, reason)
             VALUES
               ('asset-v3', 'legacy-key', 'admin', 'old.png', 'image/png', 'png', 7,
                'old-hash', 1, 2, 'old-migration', 'admin', 'old import');",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    drop(store);

    let connection = Connection::open(database_dir.join(CORE_DB_FILE)).unwrap();
    let asset: (String, String) = connection
        .query_row(
            "SELECT storage_ref, migration_status FROM legacy_assets WHERE id = 'asset-v3'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(asset, ("".into(), "legacy_unverified".into()));

    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}
