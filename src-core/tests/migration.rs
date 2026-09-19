use std::{collections::{BTreeMap, BTreeSet}, fs, path::PathBuf};

use aiwork_core::{
    CoreError, CoreStore, LegacyMigrationAsset, LegacyMigrationBatch, LegacyMigrationJob,
    LegacyMigrationKey, LegacyMigrationObservation, NewUser, Principal, QuotaGrant, UserRole,
};
use rusqlite::Connection;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiwork-core-migration-{name}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn store() -> (CoreStore, PathBuf, String) {
    let dir = test_dir("store");
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
        .issue_api_key(
            "admin",
            "admin",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    (store, dir, admin_key.id)
}

fn prepare_v11_quota_database(prefix: &str, mismatched_reservation_key: bool) -> (CoreStore, PathBuf) {
    let dir = test_dir(prefix);
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    drop(store);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             DROP INDEX IF EXISTS quota_budget_accounts_user_cap_uq;
             DROP INDEX IF EXISTS quota_budget_accounts_key_uq;
             DROP TABLE IF EXISTS quota_budget_accounts;
             DROP TABLE quota_ledger;
             CREATE TABLE quota_ledger (
               entry_id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL REFERENCES users(id),
               resource_kind TEXT NOT NULL,
               event_kind TEXT NOT NULL,
               amount INTEGER NOT NULL CHECK(amount >= 0),
               delta INTEGER NOT NULL,
               request_id TEXT,
               actor_user_id TEXT,
               reason TEXT,
               created_at_ms INTEGER NOT NULL
             );
             DROP TABLE quota_reservations;
             CREATE TABLE quota_reservations (
               id TEXT PRIMARY KEY,
               user_id TEXT NOT NULL REFERENCES users(id),
               request_id TEXT NOT NULL UNIQUE,
               resource_kind TEXT NOT NULL,
               amount INTEGER NOT NULL CHECK(amount > 0),
               state TEXT NOT NULL CHECK(state IN ('held','committed','released','unknown')),
               expires_at_ms INTEGER NOT NULL,
               created_at_ms INTEGER NOT NULL,
               settled_at_ms INTEGER
             );
             UPDATE schema_meta SET value = '11' WHERE key = 'schema_version';
             INSERT INTO users (id, name, role, status, created_at_ms, updated_at_ms)
               VALUES ('quota-user-1', 'Quota User 1', 'user', 'active', 1, 1),
                      ('quota-user-2', 'Quota User 2', 'user', 'active', 1, 1);
             INSERT INTO api_keys
               (id, user_id, name, prefix, key_digest, scopes_json, status, created_at_ms)
               VALUES ('quota-key-1', 'quota-user-1', 'key-1', 'ak-test', X'01', '[]', 'active', 1),
                      ('quota-key-2', 'quota-user-2', 'key-2', 'ak-test-2', X'02', '[]', 'active', 1);
             INSERT INTO quota_ledger
               (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id, actor_user_id, reason, created_at_ms)
               VALUES ('quota-entry-1', 'quota-user-1', 'chat_request', 'adjust', 10, 10, NULL, 'admin', 'legacy grant', 1),
                      ('quota-entry-2', 'quota-user-2', 'chat_request', 'adjust', 7, 7, NULL, 'admin', 'legacy grant', 1);
             INSERT INTO quota_reservations
               (id, user_id, request_id, resource_kind, amount, state, expires_at_ms, created_at_ms)
               VALUES ('quota-reservation-1', 'quota-user-1', 'quota-request-1', 'chat_request', 3, 'held', 100, 1);",
        )
        .unwrap();
    let request_key = if mismatched_reservation_key {
        "quota-key-2"
    } else {
        "quota-key-1"
    };
    connection
        .execute(
            "INSERT INTO requests
               (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms)
               VALUES ('quota-request-1', 'quota-user-1', ?1, 'openai', '/v1/chat/completions', 'mock', X'01', 'reserved', 1, 1)",
            [request_key],
        )
        .unwrap();
    drop(connection);

    (CoreStore::open(&dir).unwrap(), dir)
}

#[test]
fn v11_user_ledger_is_backfilled_once_without_key_copy() {
    let (store, dir) = prepare_v11_quota_database("v11-quota-backfill", false);
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 12);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let user_cap_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_budget_accounts WHERE scope = 'user_cap'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let key_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_budget_accounts WHERE scope = 'key'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(user_cap_count, 2);
    assert_eq!(key_count, 0);

    let ledger_account_id: Option<String> = connection
        .query_row(
            "SELECT budget_account_id FROM quota_ledger WHERE entry_id = 'quota-entry-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(ledger_account_id.is_some());

    let reservation_key: Option<String> = connection
        .query_row(
            "SELECT key_budget_account_id FROM quota_reservations WHERE id = 'quota-reservation-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(reservation_key.is_none());

    let migration_state: String = connection
        .query_row(
            "SELECT migration_state FROM quota_budget_accounts WHERE scope = 'user_cap' AND user_id = 'quota-user-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(migration_state, "reconcile_required");

    drop(connection);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v12_migration_failure_rolls_back_budget_tables_and_columns() {
    let (store, dir) = prepare_v11_quota_database("v11-quota-rollback", true);
    assert!(store.migrate().is_err());
    assert_eq!(store.schema_version().unwrap(), 11);
    drop(store);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let budget_table_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'quota_budget_accounts'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let budget_column_count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('quota_ledger') WHERE name = 'budget_account_id'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let ledger_account: Option<String> = connection
        .query_row(
            "SELECT budget_account_id FROM quota_ledger WHERE entry_id = 'quota-entry-1'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(None);
    assert_eq!(budget_table_count, 0);
    assert_eq!(budget_column_count, 0);
    assert!(ledger_account.is_none());

    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}

fn batch(id: &str, admin_key_id: &str) -> LegacyMigrationBatch {
    LegacyMigrationBatch {
        migration_id: id.into(),
        actor: Principal {
            user_id: "admin".into(),
            key_id: admin_key_id.into(),
            scopes: BTreeSet::from(["admin:*".into()]),
        },
        reason: "test migration".into(),
        scopes: BTreeSet::from(["videos:read".into()]),
        source_hashes: BTreeMap::from([
            ("api_keys.json".into(), "hash-a".into()),
            ("remaining_credits.json".into(), "hash-b".into()),
            ("video_tasks.json".into(), "hash-c".into()),
            ("assets.json".into(), "hash-d".into()),
        ]),
        keys: vec![LegacyMigrationKey {
            legacy_key_id: "legacy-1".into(),
            legacy_key: "legacy-secret".into(),
            user_id: "user".into(),
        }],
        assets: vec![LegacyMigrationAsset {
            id: "asset-1".into(),
            owner_key_id: "legacy-1".into(),
            user_id: "user".into(),
            filename: "input.png".into(),
            mime_type: "image/png".into(),
            extension: "png".into(),
            size: 3,
            content_sha256: "content-hash".into(),
            created_at_ms: 1,
            expires_at_ms: 2,
            storage_ref: "assets/asset-1.png".into(),
            migration_status: "verified".into(),
        }],
        jobs: vec![LegacyMigrationJob {
            id: "job-1".into(),
            owner_key_id: "legacy-1".into(),
            user_id: "user".into(),
            status: "completed".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        }],
        observations: vec![LegacyMigrationObservation {
            id: "observation-1".into(),
            account_ref: "account-1".into(),
            resource_kind: "remaining_credit".into(),
            observed_value: None,
            summary_json: "{\"credits\":12.5}".into(),
            observed_at_ms: 3,
        }],
    }
}

#[test]
fn management_operations_fail_closed_for_unknown_or_non_admin_actors() {
    let (store, _, admin_key_id) = store();
    assert!(matches!(store.authorize_admin("unknown"), Err(CoreError::AdminRequired)));
    let forged_user_principal = Principal {
        user_id: "user".into(),
        key_id: admin_key_id.clone(),
        scopes: BTreeSet::from(["admin:*".into()]),
    };
    assert!(matches!(
        store.issue_api_key_as_admin("user", "test", BTreeSet::new(), &forged_user_principal),
        Err(CoreError::AdminRequired)
    ));
    assert!(matches!(
        store.grant_as_admin(QuotaGrant {
            user_id: "user".into(),
            resource_kind: "chat_request".into(),
            amount: 1,
            actor_user_id: "user".into(),
            reason: "not allowed".into(),
        }, &forged_user_principal),
        Err(CoreError::AdminRequired)
    ));

    let mut migration = batch("unauthorized-migration", &admin_key_id);
    migration.actor = Principal {
        user_id: "user".into(),
        key_id: "user-key".into(),
        scopes: BTreeSet::new(),
    };
    assert!(matches!(
        store.apply_legacy_migration(migration),
        Err(CoreError::AdminRequired)
    ));
}

#[test]
fn legacy_batch_imports_records_atomically_and_disables_old_key() {
    let (store, dir, admin_key_id) = store();
    let result = store.apply_legacy_migration(batch("migration-1", &admin_key_id)).unwrap();
    assert_eq!(result.issued_keys.len(), 1);
    assert!(store.authenticate_api_key(&result.issued_keys[0].plaintext).is_ok());
    assert!(CoreStore::legacy_key_is_disabled(&dir, "legacy-secret").unwrap());

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let job: (String, i64) = connection
        .query_row("SELECT status, reconcile_required FROM legacy_jobs WHERE id = 'job-1'", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(job, ("completed".into(), 0));
    assert_eq!(connection.query_row("SELECT source FROM legacy_observations WHERE id = 'observation-1'", [], |row| row.get::<_, String>(0)).unwrap(), "json_cache");
    assert!(!String::from_utf8_lossy(&fs::read(dir.join("data").join("core.sqlite3")).unwrap()).contains("legacy-secret"));
}

#[test]
fn processing_legacy_job_is_rejected_before_any_batch_write() {
    let (store, dir, admin_key_id) = store();
    let mut migration = batch("processing-migration", &admin_key_id);
    migration.jobs[0].status = "processing".into();
    assert!(matches!(
        store.apply_legacy_migration(migration),
        Err(CoreError::MigrationValidation { .. })
    ));
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM api_keys WHERE user_id = 'user'", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_jobs", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn verified_legacy_asset_requires_valid_storage_ref_before_any_write() {
    let (store, dir, admin_key_id) = store();
    let mut migration = batch("invalid-asset-ref", &admin_key_id);
    migration.assets[0].storage_ref.clear();
    assert!(matches!(
        store.apply_legacy_migration(migration),
        Err(CoreError::MigrationValidation { reason }) if reason.contains("storage_ref")
    ));

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_assets", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
}

#[test]
fn sqlite_rejects_verified_legacy_asset_with_invalid_storage_ref() {
    let (_store, dir, _) = store();
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let result = connection.execute(
        "INSERT INTO legacy_assets
         (id, owner_key_id, user_id, filename, mime_type, extension, size,
          content_sha256, created_at_ms, expires_at_ms, storage_ref,
          migration_status, migration_id, actor_user_id, reason)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        rusqlite::params![
            "direct-invalid-asset",
            "legacy-1",
            "user",
            "input.png",
            "image/png",
            "png",
            3_i64,
            "hash",
            1_i64,
            2_i64,
            "assets/../secret.png",
            "verified",
            "direct-invalid",
            "admin",
            "test",
        ],
    );
    assert!(result.is_err());
}

#[test]
fn a_late_batch_constraint_failure_rolls_back_earlier_key_writes() {
    let (store, dir, admin_key_id) = store();
    let first = store.apply_legacy_migration(batch("migration-1", &admin_key_id)).unwrap();
    assert_eq!(first.issued_keys.len(), 1);

    let mut second = batch("migration-2", &admin_key_id);
    second.keys.push(LegacyMigrationKey {
        legacy_key_id: "legacy-2".into(),
        legacy_key: "legacy-secret-2".into(),
        user_id: "user".into(),
    });
    second.assets[0].id = "asset-1".into();
    let result = store.apply_legacy_migration(second);
    assert!(result.is_err());

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_key_registry", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM api_keys", [], |row| row.get::<_, i64>(0)).unwrap(), 2);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_migration_records", [], |row| row.get::<_, i64>(0)).unwrap(), 4);
}
