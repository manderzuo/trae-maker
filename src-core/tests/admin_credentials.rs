use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{CoreStore, NewAdminCredential, NewUser, UserRole};

fn test_dir(label: &str) -> PathBuf {
    let dir = PathBuf::from(format!(r"D:\gpt\starlink-admin-auth-test-{label}-{}", rand::random::<u64>()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn create_v13_fixture_with_admin_and_key(dir: &PathBuf) {
    let store = CoreStore::open(dir).unwrap();
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser { id: "admin".into(), name: "系统管理员".into(), role: UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    store
        .issue_api_key(
            "admin",
            "legacy-admin-key",
            BTreeSet::from(["admin:*".to_string()]),
            "bootstrap",
        )
        .unwrap();
}

#[test]
fn credential_round_trip_does_not_store_plaintext() {
    let dir = test_dir("admin-credential-round-trip");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser { id: "admin".into(), name: "系统管理员".into(), role: UserRole::Admin },
            "bootstrap",
        )
        .unwrap();
    store
        .upsert_admin_credential(NewAdminCredential {
            user_id: "admin".into(),
            username: "admin".into(),
            password_hash: "hash".into(),
            salt: "salt".into(),
            iterations: 600_000,
            must_change_password: true,
        })
        .unwrap();
    let saved = store.find_admin_credential("admin").unwrap().unwrap();
    assert_eq!(saved.user_id, "admin");
    assert_eq!(saved.password_hash, "hash");
    assert!(store.find_admin_credential("zuo123").unwrap().is_none());
    let _ = fs::remove_dir_all(dir);
}

#[test]
fn schema_v14_migrates_existing_v13_without_touching_users_or_keys() {
    let dir = test_dir("admin-credential-v13-migration");
    create_v13_fixture_with_admin_and_key(&dir);
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), 16);
    assert_eq!(store.count_rows("users").unwrap(), 1);
    assert_eq!(store.count_rows("api_keys").unwrap(), 1);
    assert_eq!(store.table_count("admin_credentials").unwrap(), 1);
    let _ = fs::remove_dir_all(dir);
}
