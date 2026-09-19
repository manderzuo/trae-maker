use std::{
    collections::{BTreeSet, HashMap},
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use aiwork_core::{
    CoreStore, MockObservationReader, NewUser, ObservationReader, ObservationRequest,
    ObservationSnapshot, ObservationStatus, Principal, RegisterUpstreamAccount,
    UpstreamAccountState, UpstreamObservation, UserRole,
};
use serde_json::json;

const NOW_MS: i64 = 1_725_000_000_000;

fn test_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-observation-ports-{name}-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn store_with_account() -> (CoreStore, PathBuf) {
    let dir = test_dir("store");
    let store = CoreStore::open(&dir).unwrap();
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser {
                id: "admin-1".into(),
                name: "Admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let key = store
        .issue_api_key("admin-1", "admin", BTreeSet::new(), "admin-1")
        .unwrap();
    let principal = Principal {
        user_id: "admin-1".into(),
        key_id: key.id,
        scopes: BTreeSet::new(),
    };
    let mut account = RegisterUpstreamAccount::new(
        "trae-account".into(),
        "trae".into(),
        "vault://trae-account/opaque-ref".into(),
    );
    account.state = UpstreamAccountState::Available;
    store.upsert_upstream_account(account, &principal).unwrap();
    (store, dir)
}

fn snapshot(resource_kind: &str, available_units: Option<i64>) -> ObservationSnapshot {
    ObservationSnapshot {
        account_ref: "trae-account".into(),
        resource_kind: resource_kind.into(),
        available_units,
        value_scale: 100,
        source: "reader".into(),
        observed_at_ms: NOW_MS,
        stale_at_ms: NOW_MS + 60_000,
        capabilities: vec!["chat".into()],
        region: Some("cn".into()),
        summary: json!({
            "available": available_units,
            "value_scale": 100,
            "source": "reader",
            "resource_kind": resource_kind,
        }),
    }
}

#[test]
fn mock_observer_returns_scaled_value_and_never_reads_network() {
    const AGENT_HOST: &str = "https://trae-api-cn.mchost.guru";
    const WORKBUDDY_BILLING_URL: &str = "https://www.workbuddy.cn/v2/billing/meter";
    let fixture_reads = Arc::new(AtomicUsize::new(0));
    let agent_host_touches = Arc::new(AtomicUsize::new(0));
    let workbuddy_url_touches = Arc::new(AtomicUsize::new(0));
    let expected = snapshot("chat.general", Some(1_250));
    let reader = MockObservationReader::with_read_probe(HashMap::from([(
        ("trae-account".into(), "chat.general".into()),
        expected.clone(),
    )]), {
        let fixture_reads = Arc::clone(&fixture_reads);
        let agent_host_touches = Arc::clone(&agent_host_touches);
        let workbuddy_url_touches = Arc::clone(&workbuddy_url_touches);
        move |request| {
            fixture_reads.fetch_add(1, Ordering::SeqCst);
            for value in [&request.account_ref, &request.provider, &request.resource_kind] {
                if value == AGENT_HOST {
                    agent_host_touches.fetch_add(1, Ordering::SeqCst);
                    panic!("MockObservationReader attempted AGENT_HOST instead of its fixture");
                }
                if value == WORKBUDDY_BILLING_URL {
                    workbuddy_url_touches.fetch_add(1, Ordering::SeqCst);
                    panic!("MockObservationReader attempted a WorkBuddy billing URL instead of its fixture");
                }
            }
        }
    });

    let actual = reader
        .read(ObservationRequest {
            account_ref: "trae-account".into(),
            provider: "trae".into(),
            resource_kind: "chat.general".into(),
        })
        .unwrap();

    assert_eq!(actual, expected);
    assert_eq!(actual.available_units, Some(1_250));
    assert_eq!(actual.value_scale, 100);
    assert!(matches!(
        reader.read(ObservationRequest {
            account_ref: "trae-account".into(),
            provider: "trae".into(),
            resource_kind: "chat.work".into(),
        }),
        Err(aiwork_core::ObservationError::MissingFixture)
    ));
    assert_eq!(fixture_reads.load(Ordering::SeqCst), 2);
    assert_eq!(agent_host_touches.load(Ordering::SeqCst), 0);
    assert_eq!(workbuddy_url_touches.load(Ordering::SeqCst), 0);
}

#[test]
fn failed_observation_keeps_previous_value_and_marks_latest_attempt_failed() {
    let (store, dir) = store_with_account();
    let saved = store.record_observation_snapshot(snapshot("chat.general", Some(1_250))).unwrap();
    let failed = store
        .append_upstream_observation(UpstreamObservation::new(
            "failed-observation".into(),
            "trae-account".into(),
            "chat.general".into(),
            Some(0),
            100,
            "reader".into(),
            ObservationStatus::Failed,
            NOW_MS + 1,
            NOW_MS + 60_000,
            json!({
                "available": 0,
                "value_scale": 100,
                "source": "reader",
                "resource_kind": "chat.general",
                "status": "failed",
            }),
        ))
        .unwrap();

    assert_eq!(failed.observed_value, saved.observed_value);
    let latest = store
        .get_latest_observation("trae-account", "chat.general")
        .unwrap()
        .unwrap();
    assert_eq!(latest.id, "failed-observation");
    assert_eq!(latest.status, ObservationStatus::Failed);
    assert_eq!(latest.observed_value, Some(1_250));
    assert_eq!(store.count_rows("quota_ledger").unwrap(), 0);
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn work_and_general_observations_cannot_be_cross_selected() {
    let (store, dir) = store_with_account();
    let general = store
        .record_observation_snapshot(snapshot("chat.general", Some(1_250)))
        .unwrap();
    let work = store
        .record_observation_snapshot(snapshot("chat.work", Some(400)))
        .unwrap();

    assert_eq!(
        store
            .get_latest_observation("trae-account", "chat.general")
            .unwrap()
            .unwrap()
            .id,
        general.id
    );
    assert_eq!(
        store
            .get_latest_observation("trae-account", "chat.work")
            .unwrap()
            .unwrap()
            .id,
        work.id
    );
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn diagnostic_json_cache_snapshot_is_recorded_as_stale() {
    let (store, dir) = store_with_account();
    let mut cached = snapshot("chat.work", Some(625));
    cached.source = "json_cache".into();
    cached.stale_at_ms = cached.observed_at_ms;
    cached.summary["source"] = json!("json_cache");
    cached.summary["status"] = json!("stale");

    let saved = store.record_observation_snapshot(cached).unwrap();
    assert_eq!(saved.status, ObservationStatus::Stale);
    assert_eq!(saved.observed_value, Some(625));
    drop(store);
    fs::remove_dir_all(dir).unwrap();
}
