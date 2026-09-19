use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{require_scope, AuthError, CoreStore, NewUser, UserRole, CORE_DB_FILE};
use rusqlite::Connection;

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-identity-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_store() -> (CoreStore, PathBuf) {
    let dir = test_dir("store");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    (store, dir)
}

fn database_contains_plaintext(dir: &PathBuf, table: &str, plaintext: &str) -> bool {
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    let query = match table {
        "api_keys" => {
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE \
             instr(id, ?1) > 0 OR instr(user_id, ?1) > 0 OR instr(name, ?1) > 0 OR \
             instr(prefix, ?1) > 0 OR instr(CAST(key_digest AS TEXT), ?1) > 0 OR \
             instr(scopes_json, ?1) > 0 OR instr(status, ?1) > 0)"
        }
        "audit_events" => {
            "SELECT EXISTS(SELECT 1 FROM audit_events WHERE \
             instr(id, ?1) > 0 OR instr(actor_user_id, ?1) > 0 OR instr(action, ?1) > 0 OR \
             instr(target_type, ?1) > 0 OR instr(target_id, ?1) > 0 OR \
             instr(request_id, ?1) > 0 OR instr(metadata_json, ?1) > 0)"
        }
        _ => panic!("unsupported test table: {table}"),
    };
    connection.query_row(query, [plaintext], |row| row.get(0)).unwrap()
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn create_admin(store: &CoreStore) {
    store
        .create_user(user("admin-1", UserRole::Admin), "bootstrap")
        .unwrap();
}

fn scopes(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn creates_admin_and_user_with_their_requested_roles() {
    let (store, _) = test_store();

    let admin = store
        .create_user(user("admin-1", UserRole::Admin), "bootstrap")
        .unwrap();
    let regular_user = store
        .create_user(user("user-1", UserRole::User), "admin-1")
        .unwrap();

    assert_eq!(admin.role, UserRole::Admin);
    assert_eq!(regular_user.role, UserRole::User);
}

#[test]
fn issued_key_is_hash_only_and_authenticates_its_user() {
    let (store, dir) = test_store();
    create_admin(&store);
    let user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "admin-1")
        .unwrap();

    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert!(issued.plaintext.starts_with("aw_live_"));
    assert_eq!(principal.user_id, "u1");
    assert_eq!(principal.key_id, issued.id);
    assert!(require_scope(&principal, "models:read").is_ok());
    assert!(!database_contains_plaintext(&dir, "api_keys", &issued.plaintext));
    assert!(!database_contains_plaintext(&dir, "audit_events", &issued.plaintext));
}

#[test]
fn issued_key_debug_output_does_not_include_plaintext() {
    let (store, _) = test_store();
    create_admin(&store);
    let user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "admin-1")
        .unwrap();

    let debug = format!("{issued:?}");

    assert!(!debug.contains(&issued.plaintext));
    assert!(debug.contains(&issued.prefix));
}

#[test]
fn rejects_unknown_and_revoked_keys() {
    let (store, _) = test_store();
    create_admin(&store);
    let user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&[]), "admin-1")
        .unwrap();

    assert!(matches!(
        store.authenticate_api_key("aw_live_not-a-real-key"),
        Err(AuthError::InvalidApiKey)
    ));
    store.revoke_api_key(&issued.id, "admin-1").unwrap();
    store.revoke_api_key(&issued.id, "admin-1").unwrap();
    assert!(matches!(
        store.authenticate_api_key(&issued.plaintext),
        Err(AuthError::InvalidApiKey)
    ));
}

#[test]
fn scope_check_rejects_a_scope_the_key_does_not_hold() {
    let (store, _) = test_store();
    create_admin(&store);
    let user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "admin-1")
        .unwrap();
    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert!(matches!(
        require_scope(&principal, "videos:write"),
        Err(AuthError::MissingScope { .. })
    ));
}

#[test]
fn key_identity_cannot_be_used_as_another_users_identity() {
    let (store, _) = test_store();
    create_admin(&store);
    let first_user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let second_user = store
        .create_user(user("u2", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&first_user.id, "test", scopes(&["models:read"]), "admin-1")
        .unwrap();

    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert_eq!(principal.user_id, first_user.id);
    assert_ne!(principal.user_id, second_user.id);
}

#[test]
fn active_key_for_disabled_user_is_rejected() {
    let (store, dir) = test_store();
    create_admin(&store);
    let user = store
        .create_user(user("u1", UserRole::User), "admin-1")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "admin-1")
        .unwrap();
    let connection = Connection::open(dir.join("data").join(CORE_DB_FILE)).unwrap();
    connection
        .execute("UPDATE users SET status = 'disabled' WHERE id = ?1", [&user.id])
        .unwrap();

    assert!(matches!(
        store.authenticate_api_key(&issued.plaintext),
        Err(AuthError::InvalidApiKey)
    ));
}
