use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    CoreError, CoreStore, NewUser, Principal, QuotaGrant, UserRole,
};

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-admin-projections-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn setup() -> (CoreStore, Principal, Principal, PathBuf) {
    let dir = test_dir();
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(user("admin-1", UserRole::Admin), "bootstrap")
        .unwrap();
    store
        .create_user(user("user-1", UserRole::User), "admin-1")
        .unwrap();
    let admin_key = store
        .issue_api_key(
            "admin-1",
            "admin",
            BTreeSet::from(["admin:*".to_owned()]),
            "bootstrap",
        )
        .unwrap();
    let user_key = store
        .issue_api_key("user-1", "user", BTreeSet::new(), "admin-1")
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let regular = store.authenticate_api_key(&user_key.plaintext).unwrap();
    (store, admin, regular, dir)
}

#[test]
fn admin_projections_are_redacted_and_reject_regular_principals() {
    let (store, admin, regular, dir) = setup();
    let user_key = store
        .issue_api_key(
            "user-1",
            "worker",
            BTreeSet::from(["videos:submit".to_owned()]),
            &admin.user_id,
        )
        .unwrap();

    let users = store.list_users_as_admin(&admin).unwrap();
    assert_eq!(users.len(), 2);
    assert_eq!(users.iter().find(|user| user.id == "user-1").unwrap().status, "active");

    let keys = store.list_api_keys_as_admin(&admin, Some("user-1")).unwrap();
    assert_eq!(keys.len(), 2);
    let projected = keys.iter().find(|key| key.id == user_key.id).unwrap();
    assert_eq!(projected.prefix, user_key.prefix);
    assert_eq!(projected.user_id, "user-1");
    let serialized = serde_json::to_string(projected).unwrap();
    assert!(!serialized.contains(&user_key.plaintext));
    assert!(!serialized.contains("key_digest"));

    assert!(matches!(
        store.list_users_as_admin(&regular),
        Err(CoreError::AdminRequired)
    ));
    assert!(matches!(
        store.list_api_keys_as_admin(&regular, None),
        Err(CoreError::AdminRequired)
    ));

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn user_status_is_owner_safe_and_preserves_auth_invariants() {
    let (store, admin, _regular, dir) = setup();
    let admin_two = store
        .create_user_as_admin(user("admin-2", UserRole::Admin), &admin)
        .unwrap();
    let admin_two_key = store
        .issue_api_key_as_admin("admin-2", "admin-two", BTreeSet::new(), &admin)
        .unwrap();
    let admin_two_principal = store.authenticate_api_key(&admin_two_key.plaintext).unwrap();

    assert!(matches!(
        store.set_user_status_as_admin(&admin, &admin.user_id, false),
        Err(CoreError::AdminRequired)
    ));
    let disabled = store
        .set_user_status_as_admin(&admin, &admin_two.id, false)
        .unwrap();
    assert_eq!(disabled.status, "disabled");
    assert!(matches!(
        store.set_user_status_as_admin(&admin_two_principal, &admin_two.id, false),
        Err(CoreError::AdminRequired)
    ));
    let reenabled = store
        .set_user_status_as_admin(&admin, &admin_two.id, true)
        .unwrap();
    assert_eq!(reenabled.status, "active");

    let _ = fs::remove_dir_all(dir);
}

#[test]
fn admin_quota_projection_and_key_revoke_are_guarded_and_idempotent() {
    let (store, admin, regular, dir) = setup();
    store
        .grant_as_admin(
            QuotaGrant {
                user_id: "user-1".into(),
                resource_kind: "video_job".into(),
                amount: 7,
                actor_user_id: String::new(),
                reason: "phase4d grant".into(),
            },
            &admin,
        )
        .unwrap();
    let balance = store
        .quota_balance_as_admin(&admin, "user-1", "video_job")
        .unwrap();
    assert_eq!(balance.available, 7);
    assert_eq!(balance.held, 0);
    assert!(matches!(
        store.quota_balance_as_admin(&regular, "user-1", "video_job"),
        Err(CoreError::AdminRequired)
    ));

    let user_key = store
        .issue_api_key_as_admin("user-1", "revoke-me", BTreeSet::new(), &admin)
        .unwrap();
    store
        .revoke_api_key_as_admin(&admin, &user_key.id)
        .unwrap();
    store
        .revoke_api_key_as_admin(&admin, &user_key.id)
        .unwrap();
    let projected = store
        .list_api_keys_as_admin(&admin, Some("user-1"))
        .unwrap()
        .into_iter()
        .find(|key| key.id == user_key.id)
        .unwrap();
    assert_eq!(projected.status, "revoked");

    let _ = fs::remove_dir_all(dir);
}
