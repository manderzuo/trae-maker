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
             );
             INSERT INTO upstream_observations
               (id, account_ref, resource_kind, observed_value, source, observed_at_ms, stale_at_ms, summary_json)
             VALUES
               ('legacy-observation', 'legacy-account', 'chat', 12, 'json_cache', 1, 1,
                '{\"prompt\":\"legacy private prompt\"}');",
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

fn failed_observation(id: &str, observed_value: Option<i64>, observed_at_ms: i64) -> UpstreamObservation {
    UpstreamObservation {
        id: id.into(),
        account_ref: "account-1".into(),
        resource_kind: "chat".into(),
        observed_value,
        value_scale: 100,
        source: "mock_fixture".into(),
        status: ObservationStatus::Failed,
        observed_at_ms,
        stale_at_ms: observed_at_ms,
        summary: serde_json::json!({"reason": "reader_error"}),
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
    let legacy = store
        .get_latest_observation("legacy-account", "chat")
        .unwrap()
        .unwrap();
    assert_eq!(legacy.observed_value, Some(12));
    assert_eq!(legacy.status, ObservationStatus::Stale);
    assert_eq!(legacy.summary, serde_json::json!({}));

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

#[test]
fn rejects_raw_credentials_before_they_reach_core_storage() {
    let (store, dir, principal) = fresh_store();
    for raw_credential in [
        "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJhY2NvdW50In0.signature",
        "session=raw-cookie-value",
        "refresh_token=raw-refresh-token",
    ] {
        let err = store
            .upsert_upstream_account(account(raw_credential), &principal)
            .unwrap_err();
        assert!(matches!(err, CoreError::Validation { .. }));
    }
    assert_eq!(store.count_rows("upstream_accounts").unwrap(), 0);
    drop(store);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let audit_rows: Vec<String> = connection
        .prepare("SELECT metadata_json FROM audit_events")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(audit_rows.iter().all(|row| !row.contains("raw-cookie-value")));
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn rejects_credentials_prompts_outputs_and_responses_from_observation_summaries() {
    let (store, dir, principal) = fresh_store();
    let credentials_ref = "vault://mock/opaque-credential-reference";
    store
        .upsert_upstream_account(account(credentials_ref), &principal)
        .unwrap();

    for (index, summary) in [
        serde_json::json!({"credential": credentials_ref}),
        serde_json::json!({"prompt": "private user prompt"}),
        serde_json::json!({"output": "full upstream output"}),
        serde_json::json!({"response": {"body": "complete upstream response"}}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut rejected = observation(100);
        rejected.id = format!("rejected-summary-{index}");
        rejected.summary = summary;
        let err = store.append_upstream_observation(rejected).unwrap_err();
        assert!(matches!(err, CoreError::Validation { .. }));
    }
    assert_eq!(store.count_rows("upstream_observations").unwrap(), 0);
    drop(store);

    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let persisted: String = connection
        .query_row(
            "SELECT group_concat(metadata_json, ' ') FROM audit_events",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for forbidden in [
        credentials_ref,
        "private user prompt",
        "full upstream output",
        "complete upstream response",
    ] {
        assert!(!persisted.contains(forbidden));
    }
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn failed_observations_retain_the_last_fresh_value_instead_of_zero_or_none() {
    let (store, dir, principal) = fresh_store();
    store
        .upsert_upstream_account(account("vault://mock/account-1"), &principal)
        .unwrap();
    store.append_upstream_observation(observation(100)).unwrap();

    let failed_zero = store
        .append_upstream_observation(failed_observation("failed-zero", Some(0), 150))
        .unwrap();
    assert_eq!(failed_zero.observed_value, Some(1250));
    assert_eq!(failed_zero.status, ObservationStatus::Failed);

    let failed_none = store
        .append_upstream_observation(failed_observation("failed-none", None, 160))
        .unwrap();
    assert_eq!(failed_none.observed_value, Some(1250));
    let latest = store
        .get_latest_observation("account-1", "chat")
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, "failed-none");
    assert_eq!(latest.status, ObservationStatus::Failed);
    assert_eq!(latest.observed_value, Some(1250));

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn idempotent_migrate_quarantines_existing_raw_credential_references() {
    let (store, dir, _) = fresh_store();
    drop(store);
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    connection
        .execute(
            "INSERT INTO upstream_accounts
             (id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency,
              state, consecutive_errors, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                "unsafe-account",
                "mock",
                "eyJhbGciOiJIUzI1NiJ9.unsafe.jwt",
                Option::<String>::None,
                "[]",
                1_i64,
                1_i64,
                "available",
                0_i64,
                1_i64,
                1_i64,
            ],
        )
        .unwrap();
    drop(connection);

    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    drop(store);
    let connection = Connection::open(dir.join("data").join("core.sqlite3")).unwrap();
    let quarantined: (String, i64, String) = connection
        .query_row(
            "SELECT credentials_ref, enabled, state FROM upstream_accounts WHERE id = 'unsafe-account'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert!(quarantined.0.starts_with("opaque://redacted/"));
    assert_eq!(quarantined.1, 0);
    assert_eq!(quarantined.2, "disabled");
    drop(connection);
    fs::remove_dir_all(dir).unwrap();
}
