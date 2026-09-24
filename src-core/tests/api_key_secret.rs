use std::{collections::BTreeSet, fs, path::{Path, PathBuf}};

use aiwork_core::{CoreError, CoreStore, NewUser, UserRole, CORE_DB_FILE};
use rusqlite::Connection;

struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "aiwork-api-key-secret-{}",
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

#[test]
fn v19_migration_adds_encrypted_api_key_storage_columns() {
    let dir = TestDir::new();
    let store = CoreStore::open(dir.path()).unwrap();
    store.migrate().unwrap();
    drop(store);

    let database = dir.path().join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE api_keys DROP COLUMN secret_key_version;
             ALTER TABLE api_keys DROP COLUMN secret_ciphertext;
             UPDATE schema_meta SET value = '19' WHERE key = 'schema_version';",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(dir.path()).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), aiwork_core::CURRENT_SCHEMA_VERSION);
    drop(store);

    let connection = Connection::open(database).unwrap();
    let has_secret_columns: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('api_keys')
             WHERE name IN ('secret_ciphertext', 'secret_key_version')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(has_secret_columns, 2);
}

#[test]
fn encryption_failure_creates_no_user_or_key_and_does_not_revoke_on_rotation() {
    let dir = TestDir::new();
    let store = CoreStore::open(dir.path()).unwrap();
    store.migrate().unwrap();
    store.create_bootstrap_admin(
        NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin },
        "bootstrap",
    ).unwrap();
    let admin_key = store.issue_api_key(
        "admin",
        "administrator",
        BTreeSet::from(["admin:*".into()]),
        "bootstrap",
    ).unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let existing = store.issue_api_key_for_new_user_as_admin(
        &admin,
        "Existing Key",
        BTreeSet::from(["chat:invoke".into()]),
        1,
    ).unwrap();

    let create = store.issue_api_key_for_new_user_as_admin_with_encrypted_copy(
        &admin,
        "Must Not Persist",
        BTreeSet::from(["chat:invoke".into()]),
        1,
        |_, _| Err(CoreError::ApiKeyEncryptionUnavailable),
    );
    assert!(matches!(create, Err(CoreError::ApiKeyEncryptionUnavailable)));

    let rotate = store.rotate_api_key_as_admin_with_encrypted_copy(
        &admin,
        &existing.id,
        |_, _| Err(CoreError::ApiKeyEncryptionUnavailable),
    );
    assert!(matches!(rotate, Err(CoreError::ApiKeyEncryptionUnavailable)));
    assert!(store.authenticate_api_key(&existing.plaintext).is_ok());

    let connection = Connection::open(dir.path().join("data").join(CORE_DB_FILE)).unwrap();
    let orphan_user_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM users WHERE name = 'Must Not Persist'",
        [],
        |row| row.get(0),
    ).unwrap();
    let active_key_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM api_keys WHERE user_id = ?1 AND status = 'active'",
        [&existing.user_id],
        |row| row.get(0),
    ).unwrap();
    assert_eq!(orphan_user_count, 0);
    assert_eq!(active_key_count, 1);
}
