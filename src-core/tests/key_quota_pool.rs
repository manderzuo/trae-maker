use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{CoreError, CoreStore, KeyQuotaGrant, NewUser, QuotaGrant, UserRole};

fn test_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("aiwork-key-quota-pool-{}", rand::random::<u64>()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn setup() -> (CoreStore, PathBuf, aiwork_core::Principal, String, String) {
    let dir = test_dir();
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
            NewUser { id: "user-1".into(), name: "User".into(), role: UserRole::User },
            "admin",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
        .unwrap();
    let admin = store.authenticate_api_key(&admin_key.plaintext).unwrap();
    let key_a = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user-1",
            "video-a",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
            &admin,
        )
        .unwrap();
    let key_b = store
        .issue_api_key_as_admin_with_max_concurrency(
            "user-1",
            "video-b",
            BTreeSet::from(["chat:invoke".into(), "videos:submit".into()]),
            2,
            &admin,
        )
        .unwrap();
    (store, dir, admin, key_a.id, key_b.id)
}

#[test]
fn pool_allocation_migrates_legacy_credits_and_rejects_overallocation_atomically() {
    let (store, dir, admin, key_a, key_b) = setup();
    store
        .grant_as_admin(
            QuotaGrant {
                user_id: "user-1".into(),
                resource_kind: "credits".into(),
                amount: 10,
                actor_user_id: "forged".into(),
                reason: "initial pool".into(),
            },
            &admin,
        )
        .unwrap();

    let pool = store
        .quota_pool_balance_as_admin(&admin, "user-1", "credits")
        .unwrap();
    assert_eq!(pool.available, 10);
    let assigned = store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: key_a.clone(),
                resource_kind: "credits".into(),
                amount: 6,
                actor_user_id: "forged".into(),
                reason: "allocate to key".into(),
            },
        )
        .unwrap();
    assert_eq!(assigned.available, 6);

    let before_failed_pool = store
        .quota_pool_balance_as_admin(&admin, "user-1", "credits")
        .unwrap();
    let failed = store.key_quota_allocate_from_pool_as_admin(
        &admin,
        KeyQuotaGrant {
            api_key_id: key_b.clone(),
            resource_kind: "credits".into(),
            amount: 5,
            actor_user_id: "forged".into(),
            reason: "too much".into(),
        },
    );
    assert!(matches!(
        failed,
        Err(CoreError::QuotaPoolInsufficient { available: 4, required: 5 })
    ));
    let after_failed_pool = store
        .quota_pool_balance_as_admin(&admin, "user-1", "credits")
        .unwrap();
    assert_eq!(after_failed_pool, before_failed_pool);

    let listed = store.list_api_keys_as_admin(&admin, Some("user-1")).unwrap();
    let listed_key = listed.into_iter().find(|key| key.id == key_a).unwrap();
    assert_eq!(listed_key.key_quota[0].available, 6);
    assert_eq!(listed_key.pool_allocatable[0].available, 4);

    store.revoke_api_key_as_admin(&admin, &key_a).unwrap();
    assert_eq!(
        store
            .quota_pool_allocatable_as_admin(&admin, "user-1", "credits")
            .unwrap(),
        10
    );
    store
        .key_quota_allocate_from_pool_as_admin(
            &admin,
            KeyQuotaGrant {
                api_key_id: key_b,
                resource_kind: "credits".into(),
                amount: 5,
                actor_user_id: "forged".into(),
                reason: "allocate after revoking old key".into(),
            },
        )
        .unwrap();

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
