use std::{fs, path::PathBuf};

use aiwork_core::{CoreStore, CORE_DB_FILE, CURRENT_SCHEMA_VERSION};

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
