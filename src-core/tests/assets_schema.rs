use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    AssetState, CoreStore, CreateAssetInput, NewUser, Principal, UserRole, CURRENT_SCHEMA_VERSION,
};
use rusqlite::Connection;
use aiwork_core::CORE_DB_FILE;

fn test_dir(prefix: &str) -> PathBuf {
    let root = PathBuf::from(r"D:\gpt");
    fs::create_dir_all(&root).unwrap();
    let dir = root.join(format!("aiwork-core-assets-{prefix}-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture() -> (CoreStore, Principal, Principal, PathBuf) {
    let dir = test_dir("store");
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
                id: "user-a".into(),
                name: "user-a".into(),
                role: UserRole::User,
            },
            "admin",
        )
        .unwrap();
    store
        .create_user(
            NewUser {
                id: "user-b".into(),
                name: "user-b".into(),
                role: UserRole::User,
            },
            "admin",
        )
        .unwrap();
    let scopes = BTreeSet::from(["assets:write".to_string(), "assets:read".to_string()]);
    let key_a = store.issue_api_key("user-a", "asset-a", scopes.clone(), "admin").unwrap();
    let key_b = store.issue_api_key("user-b", "asset-b", scopes.clone(), "admin").unwrap();
    let principal_a = Principal {
        user_id: "user-a".into(),
        key_id: key_a.id,
        scopes: scopes.clone(),
    };
    let principal_b = Principal {
        user_id: "user-b".into(),
        key_id: key_b.id,
        scopes,
    };
    (store, principal_a, principal_b, dir)
}

fn input(id: &str, expires_at_ms: i64, token: u8) -> CreateAssetInput {
    CreateAssetInput {
        id: id.into(),
        filename: "frame.png".into(),
        mime_type: "image/png".into(),
        extension: "png".into(),
        size: 4,
        sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
        storage_ref: format!("assets/{id}.png"),
        content_token_digest: vec![token; 32],
        created_at_ms: 1_000,
        expires_at_ms,
    }
}

#[test]
fn bootstrap_and_migration_create_authoritative_assets_table() {
    let (store, _principal_a, _principal_b, dir) = fixture();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    assert_eq!(CURRENT_SCHEMA_VERSION, 20);
    assert_eq!(store.table_count("assets").unwrap(), 1);
    assert_eq!(store.count_rows("assets").unwrap(), 0);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn v7_databases_migrate_to_v11_without_importing_legacy_assets() {
    let dir = test_dir("v7-migration");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    drop(store);
    let database = dir.join("data").join(CORE_DB_FILE);
    let connection = Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "DROP TABLE job_attempts;
             DROP TABLE jobs;
             DROP TABLE assets;
             DROP TABLE dispatch_queue_cursors;
             ALTER TABLE api_keys DROP COLUMN secret_key_version;
             ALTER TABLE api_keys DROP COLUMN secret_ciphertext;
             UPDATE schema_meta SET value = '7' WHERE key = 'schema_version';",
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    assert_eq!(store.count_rows("assets").unwrap(), 0);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn assets_are_owned_by_user_and_content_tokens_are_not_cross_user_capabilities() {
    let (store, principal_a, principal_b, dir) = fixture();
    let asset = store
        .create_asset(&principal_a, input("asset-a", 10_000, 7))
        .unwrap();
    assert_eq!(asset.user_id, "user-a");
    assert_eq!(asset.state, AssetState::Active);
    assert!(store.asset_for_user(&principal_a, "asset-a").unwrap().is_some());
    assert!(store.asset_for_user(&principal_b, "asset-a").unwrap().is_none());
    assert!(store
        .asset_by_content_token("asset-a", &[7; 32], 2_000)
        .unwrap()
        .is_some());
    assert!(store
        .asset_by_content_token("asset-a", &[8; 32], 2_000)
        .unwrap()
        .is_none());
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn expired_assets_are_not_readable_and_invalid_storage_refs_fail_closed() {
    let (store, principal_a, _principal_b, dir) = fixture();
    let expired = store
        .create_asset(&principal_a, input("expired", 10_000, 9))
        .unwrap();
    assert_eq!(
        store
            .asset_by_content_token(&expired.id, &[9; 32], 10_001)
            .unwrap(),
        None
    );
    assert_eq!(store.expire_assets(10_001).unwrap(), vec!["assets/expired.png"]);
    let mut invalid = input("invalid", 10_000, 10);
    invalid.storage_ref = "..\\secrets\\token".into();
    assert!(store.create_asset(&principal_a, invalid).is_err());
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn duplicate_asset_id_and_token_digest_are_rejected_without_partial_rows() {
    let (store, principal_a, _principal_b, dir) = fixture();
    store
        .create_asset(&principal_a, input("same", 10_000, 11))
        .unwrap();
    assert!(store
        .create_asset(&principal_a, input("same", 10_001, 12))
        .is_err());
    assert!(store
        .create_asset(&principal_a, input("other", 10_001, 11))
        .is_err());
    assert_eq!(store.count_rows("assets").unwrap(), 1);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
