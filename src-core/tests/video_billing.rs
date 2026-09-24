use std::{collections::BTreeSet, fs, path::PathBuf};

use aiwork_core::{
    CoreStore, NewUser, UserRole, VideoBillingControlInput, VideoBillingMode,
    CURRENT_SCHEMA_VERSION,
};

fn test_dir(label: &str) -> PathBuf {
    let dir = PathBuf::from(format!(
        r"D:\gpt\aiwork-core-video-billing-{label}-{}",
        rand::random::<u64>()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn new_store_defaults_to_paused_without_a_claim() {
    let dir = test_dir("default");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();

    let control = store.video_billing_control().unwrap();
    assert_eq!(control.mode, VideoBillingMode::Paused);
    assert!(control.reason.contains("未取得"));
    assert!(control.diagnostic_key_id.is_none());

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn diagnostic_claim_is_bound_to_key_and_request_hash_and_is_one_shot() {
    let dir = test_dir("claim");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "user-week".into(),
                name: "周".into(),
                role: UserRole::User,
            },
            "test",
        )
        .unwrap();
    let issued = store
        .issue_api_key(
            "user-week",
            "video-test",
            BTreeSet::from(["videos:submit".to_owned()]),
            "test",
        )
        .unwrap();
    let request_hash = "a".repeat(64);
    let other_hash = "b".repeat(64);

    store
        .set_video_billing_control(VideoBillingControlInput::diagnostic(
            &issued.id,
            &request_hash,
            "真实验收",
        ))
        .unwrap();
    assert!(store
        .claim_video_diagnostic(&issued.id, &other_hash)
        .unwrap()
        .is_none());
    assert!(store
        .claim_video_diagnostic("key-other", &request_hash)
        .unwrap()
        .is_none());
    assert!(store
        .claim_video_diagnostic(&issued.id, &request_hash)
        .unwrap()
        .is_some());
    assert!(store
        .claim_video_diagnostic(&issued.id, &request_hash)
        .unwrap()
        .is_none());

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn diagnostic_registration_can_be_rearmed_with_the_same_key_and_request_hash() {
    let dir = test_dir("rearm");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_user(
            NewUser {
                id: "user-rearm".into(),
                name: "周".into(),
                role: UserRole::User,
            },
            "test",
        )
        .unwrap();
    let issued = store
        .issue_api_key(
            "user-rearm",
            "video-rearm",
            BTreeSet::from(["videos:submit".to_owned()]),
            "test",
        )
        .unwrap();
    let request_hash = "c".repeat(64);

    let diagnostic = || {
        store
            .set_video_billing_control(VideoBillingControlInput::diagnostic(
                &issued.id,
                &request_hash,
                "真实验收",
            ))
            .unwrap();
    };

    diagnostic();
    assert!(store
        .claim_video_diagnostic(&issued.id, &request_hash)
        .unwrap()
        .is_some());

    // A later controlled acceptance may intentionally use the same request
    // contract. The previous claim remains audit history, while the new
    // registration gets its own claim row.
    diagnostic();
    assert!(store
        .claim_video_diagnostic(&issued.id, &request_hash)
        .unwrap()
        .is_some());

    assert_eq!(store.count_rows("video_diagnostic_claims").unwrap(), 2);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn migration_preserves_existing_quota_rows_and_sets_versioned_gate() {
    let dir = test_dir("migration");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    let before = store.schema_version().unwrap();

    store.migrate().unwrap();

    assert!(store.schema_version().unwrap() >= before);
    assert_eq!(store.schema_version().unwrap(), CURRENT_SCHEMA_VERSION);
    assert_eq!(store.table_count("video_billing_control").unwrap(), 1);
    assert_eq!(store.table_count("video_diagnostic_claims").unwrap(), 1);
    assert_eq!(store.count_rows("quota_ledger").unwrap(), 0);
    assert_eq!(store.video_billing_control().unwrap().mode, VideoBillingMode::Paused);

    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
