use std::{collections::{BTreeMap, BTreeSet}, fs, path::PathBuf};

use aiwork_core::{
    CoreError, CoreStore, LegacyMigrationAsset, LegacyMigrationBatch, LegacyMigrationJob,
    LegacyMigrationKey, LegacyMigrationObservation, NewUser, QuotaGrant, UserRole,
};
use rusqlite::Connection;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiwork-core-migration-{name}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn store() -> (CoreStore, PathBuf) {
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
    (store, dir)
}

fn batch(id: &str) -> LegacyMigrationBatch {
    LegacyMigrationBatch {
        migration_id: id.into(),
        actor_user_id: "admin".into(),
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
        }],
        jobs: vec![LegacyMigrationJob {
            id: "job-1".into(),
            owner_key_id: "legacy-1".into(),
            user_id: "user".into(),
            status: "processing".into(),
            created_at_ms: 1,
            updated_at_ms: 2,
        }],
        observations: vec![LegacyMigrationObservation {
            id: "observation-1".into(),
            account_ref: "account-1".into(),
            resource_kind: "remaining_credit".into(),
            value_json: "12.5".into(),
            observed_at_ms: 3,
        }],
    }
}

#[test]
fn management_operations_fail_closed_for_unknown_or_non_admin_actors() {
    let (store, _) = store();
    assert!(matches!(store.authorize_admin("unknown"), Err(CoreError::AdminRequired)));
    assert!(matches!(
        store.issue_api_key_as_admin("user", "test", BTreeSet::new(), "user"),
        Err(CoreError::AdminRequired)
    ));
    assert!(matches!(
        store.grant_as_admin(QuotaGrant {
            user_id: "user".into(),
            resource_kind: "chat_request".into(),
            amount: 1,
            actor_user_id: "user".into(),
            reason: "not allowed".into(),
        }),
        Err(CoreError::AdminRequired)
    ));

    let mut migration = batch("unauthorized-migration");
    migration.actor_user_id = "user".into();
    assert!(matches!(
        store.apply_legacy_migration(migration),
        Err(CoreError::AdminRequired)
    ));
}

#[test]
fn legacy_batch_imports_records_atomically_and_disables_old_key() {
    let (store, dir) = store();
    let result = store.apply_legacy_migration(batch("migration-1")).unwrap();
    assert_eq!(result.issued_keys.len(), 1);
    assert!(store.authenticate_api_key(&result.issued_keys[0].plaintext).is_ok());
    assert!(CoreStore::legacy_key_is_disabled(&dir, "legacy-secret").unwrap());

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let job: (String, i64) = connection
        .query_row("SELECT status, reconcile_required FROM legacy_jobs WHERE id = 'job-1'", [], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap();
    assert_eq!(job, ("unknown".into(), 1));
    assert_eq!(connection.query_row("SELECT source FROM legacy_observations WHERE id = 'observation-1'", [], |row| row.get::<_, String>(0)).unwrap(), "json_cache");
    assert!(!String::from_utf8_lossy(&fs::read(dir.join("data").join("core.sqlite3")).unwrap()).contains("legacy-secret"));
}

#[test]
fn a_late_batch_constraint_failure_rolls_back_earlier_key_writes() {
    let (store, dir) = store();
    let first = store.apply_legacy_migration(batch("migration-1")).unwrap();
    assert_eq!(first.issued_keys.len(), 1);

    let mut second = batch("migration-2");
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
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM api_keys", [], |row| row.get::<_, i64>(0)).unwrap(), 1);
    assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_migration_records", [], |row| row.get::<_, i64>(0)).unwrap(), 4);
}
