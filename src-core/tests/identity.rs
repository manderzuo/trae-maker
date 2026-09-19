use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{require_scope, AuthError, CoreStore, NewUser, UserRole};

fn test_dir(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-identity-{prefix}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn test_store() -> CoreStore {
    let store = CoreStore::open(&test_dir("store")).unwrap();
    store.migrate().unwrap();
    store
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn scopes(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

#[test]
fn creates_admin_and_user_with_their_requested_roles() {
    let store = test_store();

    let admin = store
        .create_user(user("admin-1", UserRole::Admin), "bootstrap")
        .unwrap();
    let regular_user = store
        .create_user(user("user-1", UserRole::User), "bootstrap")
        .unwrap();

    assert_eq!(admin.role, UserRole::Admin);
    assert_eq!(regular_user.role, UserRole::User);
}

#[test]
fn issued_key_is_hash_only_and_authenticates_its_user() {
    let store = test_store();
    let user = store
        .create_user(user("u1", UserRole::User), "bootstrap")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "bootstrap")
        .unwrap();

    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert!(issued.plaintext.starts_with("aw_live_"));
    assert_eq!(principal.user_id, "u1");
    assert_eq!(principal.key_id, issued.id);
    assert!(require_scope(&principal, "models:read").is_ok());
    assert!(store
        .raw_text_search("api_keys", &issued.plaintext)
        .unwrap()
        .is_empty());
    assert!(store
        .raw_text_search("audit_events", &issued.plaintext)
        .unwrap()
        .is_empty());
}

#[test]
fn rejects_unknown_and_revoked_keys() {
    let store = test_store();
    let user = store
        .create_user(user("u1", UserRole::User), "bootstrap")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&[]), "bootstrap")
        .unwrap();

    assert!(matches!(
        store.authenticate_api_key("aw_live_not-a-real-key"),
        Err(AuthError::InvalidApiKey)
    ));
    store.revoke_api_key(&issued.id, "bootstrap").unwrap();
    store.revoke_api_key(&issued.id, "bootstrap").unwrap();
    assert!(matches!(
        store.authenticate_api_key(&issued.plaintext),
        Err(AuthError::InvalidApiKey)
    ));
}

#[test]
fn scope_check_rejects_a_scope_the_key_does_not_hold() {
    let store = test_store();
    let user = store
        .create_user(user("u1", UserRole::User), "bootstrap")
        .unwrap();
    let issued = store
        .issue_api_key(&user.id, "test", scopes(&["models:read"]), "bootstrap")
        .unwrap();
    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert!(matches!(
        require_scope(&principal, "videos:write"),
        Err(AuthError::MissingScope { .. })
    ));
}

#[test]
fn key_identity_cannot_be_used_as_another_users_identity() {
    let store = test_store();
    let first_user = store
        .create_user(user("u1", UserRole::User), "bootstrap")
        .unwrap();
    let second_user = store
        .create_user(user("u2", UserRole::User), "bootstrap")
        .unwrap();
    let issued = store
        .issue_api_key(&first_user.id, "test", scopes(&["models:read"]), "bootstrap")
        .unwrap();

    let principal = store.authenticate_api_key(&issued.plaintext).unwrap();

    assert_eq!(principal.user_id, first_user.id);
    assert_ne!(principal.user_id, second_user.id);
}
