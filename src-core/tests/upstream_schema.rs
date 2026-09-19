use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    CoreError, CoreStore, LeaseState, ObservationStatus, Principal, RegisterUpstreamAccount,
    UpstreamAccountState, UpstreamObservation, UserRole,
};
use rusqlite::{params, Connection};

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-upstream-schema-{name}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn fixture_at_schema_v5() -> (CoreStore, PathBuf) {
    let dir = test_dir("v5");
    let database_dir = dir.join("data");
    fs::create_dir_all(&database_dir).unwrap();
    let connection = Connection::open(database_dir.join("core.sqlite3")).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE schema_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO schema_meta (key, value) VALUES ('schema_version', '5');
             CREATE TABLE quota_ledger (entry_id TEXT PRIMARY KEY);
             CREATE TABLE requests (id TEXT PRIMARY KEY);
             CREATE TABLE upstream_observations (
               id TEXT PRIMARY KEY,
               account_ref TEXT NOT NULL,
               resource_kind TEXT NOT NULL,
               observed_value INTEGER,
               source TEXT NOT NULL,
               observed_at_ms INTEGER NOT NULL,
               stale_at_ms INTEGER,
               summary_json TEXT NOT NULL
             );
             CREATE TABLE audit_events (
               id TEXT PRIMARY KEY,
               actor_user_id TEXT,
               action TEXT NOT NULL,
               target_type TEXT NOT NULL,
               target_id TEXT,
               request_id TEXT,
               metadata_json TEXT NOT NULL,
               created_at_ms INTEGER NOT NULL
             );",
        )
        .unwrap();
    drop(connection);
    (CoreStore::open(&dir).unwrap(), dir)
}

fn fresh_store() -> (CoreStore, PathBuf, Principal) {
    let dir = test_dir("fresh");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            aiwork_core::NewUser {
                id: "admin".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let issued = store
        .issue_api_key(
            "admin",
            "admin",
            BTreeSet::from(["admin:*".into()]),
            "bootstrap",
        )
        .unwrap();
    (
        store,
        dir,
        Principal {
            user_id: "admin".into(),
            key_id: issued.id,
            scopes: BTreeSet::from(["admin:*".into()]),
        },
    )
}

fn account(credentials_ref: &str) -> RegisterUpstreamAccount {
    RegisterUpstreamAccount {
        id: "account-1".into(),
        provider: "mock".into(),
        credentials_ref: credentials_ref.into(),
        region: Some("test-region".into()),
        capabilities: BTreeSet::from(["chat".into()]),
        enabled: true,
        max_concurrency: 1,
        state: UpstreamAccountState::Available,
        cooldown_until_ms: None,
        cooldown_reason: None,
        consecutive_errors: 0,
    }
}

fn observation(value_scale: i64) -> UpstreamObservation {
    UpstreamObservation {
        id: "observation-1".into(),
        account_ref: "account-1".into(),
        resource_kind: "chat".into(),
        observed_value: Some(1250),
        value_scale,
        source: "mock_fixture".into(),
        status: ObservationStatus::Fresh,
        observed_at_ms: 100,
        stale_at_ms: 200,
        summary: serde_json::json!({"available": 12.5}),
    }
}

#[test]
fn migrates_v5_to_v6_without_importing_user_quota_or_secrets() {
    let (store, dir) = fixture_at_schema_v5();
    store.migrate().unwrap();
    store.migrate().unwrap();

    assert_eq!(store.schema_version().unwrap(), 6);
    assert!(store.table_exists("upstream_accounts").unwrap());
    assert!(store.table_exists("upstream_leases").unwrap());
    assert_eq!(store.count_rows("quota_ledger").unwrap(), 0);

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn lease_and_observation_constraints_reject_invalid_values() {
    let (store, dir, principal) = fresh_store();
    store
        .upsert_upstream_account(account("vault://mock/account-1"), &principal)
        .unwrap();

    let err = store.append_upstream_observation(observation(0)).unwrap_err();
    assert!(matches!(err, CoreError::Validation { .. }));

    drop(store);
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO requests
             (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                "request-1",
                "admin",
                principal.key_id,
                "openai",
                "/v1/chat/completions",
                "mock-1",
                vec![0_u8],
                "received",
                1_i64,
                1_i64,
            ],
        )
        .unwrap();
    let unknown_insert = connection.execute(
        "INSERT INTO upstream_leases
         (id, request_id, account_ref, resource_kind, predicted_units, state,
          lease_expires_at_ms, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            "lease-unknown",
            "request-1",
            "account-1",
            "chat-invalid",
            1_i64,
            LeaseState::Unknown.as_str(),
            1_i64,
            1_i64,
            1_i64,
        ],
    );
    assert!(unknown_insert.is_ok(), "unknown lease insert failed: {unknown_insert:?}");

    let invalid_insert = connection.execute(
        "INSERT INTO upstream_leases
         (id, request_id, account_ref, resource_kind, predicted_units, state,
          lease_expires_at_ms, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            "lease-invalid",
            "request-1",
            "account-1",
            "chat",
            0_i64,
            "held",
            1_i64,
            1_i64,
            1_i64,
        ],
    );
    assert!(invalid_insert.is_err());
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn credentials_ref_is_required_and_never_persisted_in_summaries_or_audit_rows() {
    let (store, dir, principal) = fresh_store();
    let credentials_ref = "vault://mock/opaque-credential-reference";
    let err = store
        .upsert_upstream_account(account(""), &principal)
        .unwrap_err();
    assert!(matches!(err, CoreError::Validation { .. }));

    store
        .upsert_upstream_account(account(credentials_ref), &principal)
        .unwrap();
    store.append_upstream_observation(observation(100)).unwrap();
    assert_eq!(
        store
            .get_latest_observation("account-1", "chat")
            .unwrap()
            .unwrap()
            .id,
        "observation-1"
    );

    drop(store);
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let summary: String = connection
        .query_row(
            "SELECT summary_json FROM upstream_observations WHERE id = 'observation-1'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let audit_rows: Vec<String> = connection
        .prepare("SELECT metadata_json FROM audit_events")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!summary.contains(credentials_ref));
    assert!(audit_rows.iter().all(|row| !row.contains(credentials_ref)));
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}
