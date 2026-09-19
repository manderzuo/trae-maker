//! Legacy JSON inspection and explicitly mapped Core migration.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use aiwork_core::{
    CoreError, CoreStore, IssuedApiKey, LegacyMigrationAsset, LegacyMigrationBatch,
    LegacyMigrationJob, LegacyMigrationKey, LegacyMigrationObservation,
    Principal,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const LEGACY_FILES: [&str; 4] = [
    "api_keys.json",
    "remaining_credits.json",
    "video_tasks.json",
    "assets.json",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LegacyMigrationMapping {
    pub legacy_key_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct MigrationReport {
    pub source_hashes: BTreeMap<String, String>,
    pub key_count: usize,
    pub unmapped_keys: Vec<String>,
    pub asset_count: usize,
    pub video_count: usize,
    pub processing_video_count: usize,
    pub cached_credit_count: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct IssuedApiKeyResponse {
    pub id: String,
    pub plaintext: String,
    pub prefix: String,
    pub user_id: String,
    pub scopes: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MigrationApplyResponse {
    pub report: MigrationReport,
    pub issued_keys: Vec<IssuedApiKeyResponse>,
}

#[derive(Deserialize)]
struct LegacyApiKeys {
    #[serde(default)]
    keys: Vec<LegacyApiKey>,
}

#[derive(Deserialize)]
struct LegacyKeyDailyStat {
    #[serde(rename = "date")]
    _date: String,
    #[serde(rename = "requests")]
    _requests: u64,
}

fn default_legacy_key_enabled() -> bool {
    true
}

#[derive(Deserialize)]
struct LegacyApiKey {
    id: String,
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "key")]
    _key: String,
    #[serde(default = "default_legacy_key_enabled")]
    _enabled: bool,
    #[serde(default)]
    _daily_limit: u64,
    #[serde(default)]
    _created_at: u64,
    #[serde(default)]
    _used_date: String,
    #[serde(default)]
    _used_today: u64,
    #[serde(default)]
    _allowed_accounts: Vec<String>,
    #[serde(default)]
    _schedule_mode: String,
    #[serde(default)]
    _dedicated_account: String,
    #[serde(default)]
    _daily_stats: Vec<LegacyKeyDailyStat>,
}

#[derive(Clone, Deserialize)]
struct LegacyRemainingCredits {
    #[serde(default)]
    credits: std::collections::HashMap<String, f64>,
    #[serde(default)]
    #[serde(rename = "expire_times")]
    _expire_times: std::collections::HashMap<String, i64>,
    #[serde(default)]
    #[serde(rename = "general")]
    _general: std::collections::HashMap<String, f64>,
    #[serde(default)]
    #[serde(rename = "work")]
    _work: std::collections::HashMap<String, f64>,
    #[serde(default)]
    #[serde(rename = "membership_expire")]
    _membership_expire: std::collections::HashMap<String, i64>,
    #[serde(default)]
    #[serde(rename = "membership_next_billing")]
    _membership_next_billing: std::collections::HashMap<String, i64>,
    #[serde(default)]
    #[serde(rename = "updated_at")]
    _updated_at: Option<String>,
}

fn default_legacy_owner_key() -> String {
    "anonymous".into()
}

#[derive(Deserialize)]
struct LegacyVideoTask {
    id: String,
    #[serde(rename = "object")]
    _object: String,
    #[serde(rename = "model")]
    _model: String,
    status: String,
    #[serde(rename = "prompt")]
    _prompt: String,
    #[serde(rename = "created_at")]
    _created_at: u64,
    #[serde(rename = "updated_at")]
    _updated_at: u64,
    #[serde(rename = "error")]
    _error: Option<String>,
    #[serde(rename = "transport")]
    _transport: String,
    #[serde(rename = "video_url")]
    _video_url: Option<String>,
    #[serde(rename = "resource_uri")]
    _resource_uri: Option<String>,
    #[serde(rename = "video_duration")]
    _video_duration: Option<f64>,
    #[serde(default)]
    _content_url: Option<String>,
    #[serde(default)]
    _artifact_error: Option<String>,
    #[serde(default)]
    _request_key: Option<String>,
    #[serde(default = "default_legacy_owner_key")]
    owner_key_id: String,
}

#[derive(Deserialize)]
struct LegacyAssets {
    #[serde(rename = "version")]
    _version: u32,
    assets: Vec<LegacyAsset>,
}

#[derive(Deserialize)]
struct LegacyAsset {
    id: String,
    #[serde(default)]
    owner_key_id: String,
    #[serde(rename = "filename")]
    _filename: String,
    #[serde(rename = "mime_type")]
    _mime_type: String,
    #[serde(rename = "extension")]
    _extension: String,
    #[serde(rename = "size")]
    _size: u64,
    #[serde(rename = "sha256")]
    _sha256: String,
    #[serde(rename = "created_at")]
    _created_at: u64,
    #[serde(rename = "expires_at")]
    _expires_at: u64,
    #[serde(default)]
    _public_token: String,
}

struct LegacySnapshot {
    keys: Vec<LegacyApiKey>,
    credits: LegacyRemainingCredits,
    videos: Vec<LegacyVideoTask>,
    assets: Vec<LegacyAsset>,
}

fn source_path(data_dir: &Path, file_name: &str) -> std::path::PathBuf {
    data_dir.join("data").join(file_name)
}

fn read_source(data_dir: &Path, file_name: &str, report: &mut MigrationReport) -> Result<Option<Vec<u8>>, CoreError> {
    let path = source_path(data_dir, file_name);
    if !path.is_file() {
        report.source_hashes.insert(file_name.to_string(), "missing".into());
        report.errors.push(format!("missing legacy file: {file_name}"));
        return Ok(None);
    }
    let bytes = fs::read(path)?;
    report.source_hashes.insert(
        file_name.to_string(),
        format!("{:x}", Sha256::digest(&bytes)),
    );
    Ok(Some(bytes))
}

fn parse_source<T: for<'de> Deserialize<'de>>(
    bytes: Option<Vec<u8>>,
    file_name: &str,
    report: &mut MigrationReport,
) -> Option<T> {
    let Some(bytes) = bytes else { return None };
    match serde_json::from_slice(&bytes) {
        Ok(value) => Some(value),
        Err(_) => {
            report.errors.push(format!("invalid JSON: {file_name}"));
            None
        }
    }
}

fn add_duplicate_errors(kind: &str, ids: impl Iterator<Item = String>, report: &mut MigrationReport) {
    let mut seen = HashSet::new();
    for id in ids {
        if !seen.insert(id) {
            report.errors.push(format!("duplicate {kind} id"));
        }
    }
}

fn scan_legacy(data_dir: &Path) -> Result<(MigrationReport, LegacySnapshot), CoreError> {
    let mut report = MigrationReport::default();
    let keys: LegacyApiKeys = parse_source(
        read_source(data_dir, LEGACY_FILES[0], &mut report)?,
        LEGACY_FILES[0],
        &mut report,
    )
    .unwrap_or(LegacyApiKeys { keys: Vec::new() });
    let credits: LegacyRemainingCredits = parse_source(
        read_source(data_dir, LEGACY_FILES[1], &mut report)?,
        LEGACY_FILES[1],
        &mut report,
    )
    .unwrap_or(LegacyRemainingCredits {
        credits: std::collections::HashMap::new(),
        _expire_times: std::collections::HashMap::new(),
        _general: std::collections::HashMap::new(),
        _work: std::collections::HashMap::new(),
        _membership_expire: std::collections::HashMap::new(),
        _membership_next_billing: std::collections::HashMap::new(),
        _updated_at: None,
    });
    let videos: Vec<LegacyVideoTask> = parse_source(
        read_source(data_dir, LEGACY_FILES[2], &mut report)?,
        LEGACY_FILES[2],
        &mut report,
    )
    .unwrap_or_default();
    let assets_file: LegacyAssets = parse_source(
        read_source(data_dir, LEGACY_FILES[3], &mut report)?,
        LEGACY_FILES[3],
        &mut report,
    )
    .unwrap_or(LegacyAssets {
        _version: 0,
        assets: Vec::new(),
    });

    report.key_count = keys.keys.len();
    report.cached_credit_count = credits.credits.len();
    report.video_count = videos.len();
    report.asset_count = assets_file.assets.len();
    report.processing_video_count = videos
        .iter()
        .filter(|task| task.status == "processing")
        .count();
    add_duplicate_errors(
        "legacy key",
        keys.keys.iter().map(|key| key.id.clone()),
        &mut report,
    );
    add_duplicate_errors(
        "video task",
        videos.iter().map(|task| task.id.clone()),
        &mut report,
    );
    add_duplicate_errors(
        "asset",
        assets_file.assets.iter().map(|asset| asset.id.clone()),
        &mut report,
    );

    let allowed_statuses = ["queued", "processing", "completed", "failed"];
    for task in &videos {
        if !allowed_statuses.contains(&task.status.as_str()) {
            report.errors.push("unknown video status".into());
        }
    }
    if report.processing_video_count > 0 {
        report.errors.push(format!(
            "reconcile_required: {} processing video task(s)",
            report.processing_video_count
        ));
    }

    Ok((
        report,
        LegacySnapshot {
            keys: keys.keys,
            credits,
            videos,
            assets: assets_file.assets,
        },
    ))
}

/// Inspect legacy files without opening or creating Core storage.
pub fn inspect_legacy(data_dir: &Path) -> Result<MigrationReport, CoreError> {
    scan_legacy(data_dir).map(|(report, _)| report)
}

fn response_from_issued(key: IssuedApiKey) -> IssuedApiKeyResponse {
    IssuedApiKeyResponse {
        id: key.id,
        plaintext: key.plaintext,
        prefix: key.prefix,
        user_id: key.user_id,
        scopes: key.scopes,
    }
}

const MIGRATION_SCOPES: [&str; 7] = [
    "models:read",
    "chat:invoke",
    "assets:read",
    "assets:write",
    "videos:read",
    "videos:submit",
    "usage:read",
];

fn prepare_legacy_apply(
    data_dir: &Path,
    mappings: &[LegacyMigrationMapping],
) -> Result<(MigrationReport, LegacySnapshot), CoreError> {
    let (mut report, snapshot) = scan_legacy(data_dir)?;
    let mut mapping_by_key = BTreeMap::new();
    let key_ids: HashSet<&str> = snapshot.keys.iter().map(|key| key.id.as_str()).collect();
    for mapping in mappings {
        if mapping.legacy_key_id.trim().is_empty() || mapping.user_id.trim().is_empty() {
            report.errors.push("legacy mapping has an empty owner".into());
        }
        if mapping_by_key
            .insert(mapping.legacy_key_id.as_str(), mapping.user_id.as_str())
            .is_some()
        {
            report.errors.push("duplicate legacy mapping id".into());
        }
        if !key_ids.contains(mapping.legacy_key_id.as_str()) {
            report.errors.push("mapping references an unknown legacy key".into());
        }
    }
    for key in &snapshot.keys {
        if !mapping_by_key.contains_key(key.id.as_str()) {
            report.unmapped_keys.push(key.id.clone());
        }
    }
    for asset in &snapshot.assets {
        if !mapping_by_key.contains_key(asset.owner_key_id.as_str()) {
            report.errors.push("asset owner mapping is missing".into());
        }
    }
    for task in &snapshot.videos {
        if !mapping_by_key.contains_key(task.owner_key_id.as_str()) {
            report.errors.push("video owner mapping is missing".into());
        }
    }
    if !report.unmapped_keys.is_empty() {
        report.errors.push("legacy key mapping is incomplete".into());
    }
    if !data_dir.join("data").join(aiwork_core::CORE_DB_FILE).is_file() {
        report.errors.push("mapped Core owner or admin actor does not exist".into());
    }
    validate_legacy_assets(data_dir, &snapshot.assets, &mut report);
    Ok((report, snapshot))
}

fn legacy_asset_storage_path(data_dir: &Path, asset: &LegacyAsset) -> Option<std::path::PathBuf> {
    if asset.id.trim().is_empty()
        || !asset.id.bytes().all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
    {
        return None;
    }
    let extension = asset._extension.trim_start_matches('.');
    if extension.is_empty() || !extension.bytes().all(|value| value.is_ascii_alphanumeric()) {
        return None;
    }
    let root = std::env::var_os("AIWORK_ASSET_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| data_dir.join("data").join("assets"));
    Some(root.join(format!("{}.{}", asset.id, extension)))
}

fn validate_legacy_assets(data_dir: &Path, assets: &[LegacyAsset], report: &mut MigrationReport) {
    for asset in assets {
        let Some(path) = legacy_asset_storage_path(data_dir, asset) else {
            report.errors.push("legacy asset storage reference is invalid".into());
            continue;
        };
        let Ok(metadata) = fs::metadata(&path) else {
            report.errors.push("legacy asset file is missing".into());
            continue;
        };
        if metadata.len() != asset._size {
            report.errors.push("legacy asset size does not match metadata".into());
            continue;
        }
        let Ok(bytes) = fs::read(&path) else {
            report.errors.push("legacy asset file cannot be read".into());
            continue;
        };
        let actual_hash = format!("{:x}", Sha256::digest(bytes));
        if actual_hash != asset._sha256 {
            report.errors.push("legacy asset hash does not match metadata".into());
        }
    }
}

fn build_migration_batch(
    snapshot: &LegacySnapshot,
    mappings: &[LegacyMigrationMapping],
    principal: &Principal,
    report: &MigrationReport,
) -> Result<LegacyMigrationBatch, CoreError> {
    let mapping_by_key = mappings
        .iter()
        .map(|mapping| (mapping.legacy_key_id.as_str(), mapping.user_id.as_str()))
        .collect::<BTreeMap<_, _>>();
    let keys = snapshot
        .keys
        .iter()
        .map(|key| LegacyMigrationKey {
            legacy_key_id: key.id.clone(),
            legacy_key: key._key.clone(),
            user_id: mapping_by_key[key.id.as_str()].to_owned(),
        })
        .collect::<Vec<_>>();
    let assets = snapshot
        .assets
        .iter()
        .map(|asset| LegacyMigrationAsset {
            id: asset.id.clone(),
            owner_key_id: asset.owner_key_id.clone(),
            user_id: mapping_by_key[asset.owner_key_id.as_str()].to_owned(),
            filename: asset._filename.clone(),
            mime_type: asset._mime_type.clone(),
            extension: asset._extension.clone(),
            size: asset._size as i64,
            content_sha256: asset._sha256.clone(),
            created_at_ms: asset._created_at as i64 * 1000,
            expires_at_ms: asset._expires_at as i64 * 1000,
            storage_ref: format!("assets/{}.{}", asset.id, asset._extension.trim_start_matches('.')),
            migration_status: "verified".into(),
        })
        .collect::<Vec<_>>();
    let jobs = snapshot
        .videos
        .iter()
        .map(|task| LegacyMigrationJob {
            id: task.id.clone(),
            owner_key_id: task.owner_key_id.clone(),
            user_id: mapping_by_key[task.owner_key_id.as_str()].to_owned(),
            status: task.status.clone(),
            created_at_ms: task._created_at as i64 * 1000,
            updated_at_ms: task._updated_at as i64 * 1000,
        })
        .collect::<Vec<_>>();
    let observations = snapshot
        .credits
        .credits
        .iter()
        .enumerate()
        .map(|(index, (account_ref, _value))| Ok(LegacyMigrationObservation {
            id: format!("{}-credit-{index}", report.source_hashes.get("remaining_credits.json").unwrap_or(&String::new())),
            account_ref: account_ref.clone(),
            resource_kind: "remaining_credit".into(),
            observed_value: None,
            summary_json: serde_json::to_string(&serde_json::json!({
                "credits": snapshot.credits.credits,
                "expire_times": snapshot.credits._expire_times,
                "general": snapshot.credits._general,
                "work": snapshot.credits._work,
                "membership_expire": snapshot.credits._membership_expire,
                "membership_next_billing": snapshot.credits._membership_next_billing,
                "updated_at": snapshot.credits._updated_at,
            }))?,
            observed_at_ms: chrono::Utc::now().timestamp_millis(),
        }))
        .collect::<Result<Vec<_>, CoreError>>()?;
    Ok(LegacyMigrationBatch {
        migration_id: format!("legacy-migration-{:x}", rand::random::<u64>()),
        actor: principal.clone(),
        reason: "explicit legacy JSON migration".into(),
        scopes: MIGRATION_SCOPES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>(),
        source_hashes: report.source_hashes.clone(),
        keys,
        assets,
        jobs,
        observations,
    })
}

fn is_blocking_report(report: &MigrationReport) -> bool {
    !report.errors.is_empty()
}

pub fn apply_legacy_with_store(
    data_dir: &Path,
    store: Arc<CoreStore>,
    mappings: &[LegacyMigrationMapping],
    principal: &Principal,
) -> Result<MigrationApplyResponse, CoreError> {
    let (mut report, snapshot) = prepare_legacy_apply(data_dir, mappings)?;
    if is_blocking_report(&report) {
        return Ok(MigrationApplyResponse {
            report,
            issued_keys: Vec::new(),
        });
    }
    store.migrate()?;
    let batch = build_migration_batch(&snapshot, mappings, principal, &report)?;
    match store.apply_legacy_migration(batch) {
        Ok(result) => Ok(MigrationApplyResponse {
            report,
            issued_keys: result.issued_keys.into_iter().map(response_from_issued).collect(),
        }),
        Err(error) => {
            report.errors.push(safe_migration_error(&error));
            Ok(MigrationApplyResponse { report, issued_keys: Vec::new() })
        }
    }
}

pub fn apply_legacy(
    data_dir: &Path,
    mappings: &[LegacyMigrationMapping],
    principal: &Principal,
) -> Result<MigrationReport, CoreError> {
    let (report, _) = prepare_legacy_apply(data_dir, mappings)?;
    if is_blocking_report(&report) {
        return Ok(report);
    }
    let store = Arc::new(CoreStore::open(data_dir)?);
    Ok(apply_legacy_with_store(data_dir, store, mappings, principal)?.report)
}

fn safe_migration_error(error: &CoreError) -> String {
    match error {
        CoreError::AdminRequired => "admin API key is not authorized".into(),
        CoreError::UserNotActive => "mapped Core owner does not exist or is disabled".into(),
        CoreError::MigrationValidation { reason } => format!("migration batch rejected: {reason}"),
        _ => "migration batch rejected by Core storage".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::fs;
    use std::path::{Path, PathBuf};

    use serde_json::json;
    use sha2::{Digest, Sha256};

    use super::{apply_legacy, inspect_legacy, LegacyMigrationMapping};

    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "aiwork-core-migration-{name}-{}",
                rand::random::<u64>()
            ));
            fs::create_dir_all(dir.join("data")).unwrap();
            Self { dir }
        }

        fn write(&self, name: &str, value: &serde_json::Value) -> String {
            let bytes = serde_json::to_vec(value).unwrap();
            fs::write(self.path(name), &bytes).unwrap();
            format!("{:x}", Sha256::digest(bytes))
        }

        fn write_raw(&self, name: &str, contents: &str) {
            fs::write(self.path(name), contents).unwrap();
        }

        fn path(&self, name: &str) -> PathBuf {
            self.dir.join("data").join(name)
        }

        fn write_asset(&self, id: &str, extension: &str, bytes: &[u8]) {
            fs::create_dir_all(self.dir.join("data").join("assets")).unwrap();
            fs::write(
                self.dir.join("data").join("assets").join(format!("{id}.{extension}")),
                bytes,
            )
            .unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn legacy_key(id: &str, key: &str) -> serde_json::Value {
        json!({
            "id": id,
            "name": id,
            "key": key,
            "enabled": true,
            "daily_limit": 0,
            "created_at": 1,
            "used_date": "",
            "used_today": 0,
            "allowed_accounts": [],
            "schedule_mode": "",
            "dedicated_account": "",
            "daily_stats": []
        })
    }

    fn video(id: &str, status: &str, owner_key_id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "object": "video",
            "model": "seedance",
            "status": status,
            "prompt": "do not put this in a report",
            "created_at": 1,
            "updated_at": 2,
            "error": null,
            "transport": "native_sse",
            "video_url": null,
            "resource_uri": null,
            "video_duration": null,
            "content_url": null,
            "artifact_error": null,
            "request_key": null,
            "owner_key_id": owner_key_id
        })
    }

    fn asset(id: &str, owner_key_id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "owner_key_id": owner_key_id,
            "filename": "input.png",
            "mime_type": "image/png",
            "extension": "png",
            "size": 3,
            "sha256": "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "created_at": 1,
            "expires_at": 2,
            "public_token": "asset-public-token"
        })
    }

    fn valid_fixture() -> (Fixture, [String; 4]) {
        let fixture = Fixture::new("valid");
        fixture.write_asset("asset-1", "png", b"abc");
        let hashes = [
            fixture.write(
                "api_keys.json",
                &json!({
                    "keys": [legacy_key("legacy-key-1", "legacy-secret-key")],
                    "auth_disabled": false
                }),
            ),
            fixture.write(
                "remaining_credits.json",
                &json!({
                    "credits": {"account-1": 12.5, "account-2": 3.0},
                    "general": {"account-1": 10.0},
                    "work": {"account-1": 2.5},
                    "expire_times": {"account-1": 1700000000},
                    "membership_expire": {"account-1": 1800000000},
                    "membership_next_billing": {"account-1": 1710000000},
                    "updated_at": "2026-09-19T00:00:00Z"
                }),
            ),
            fixture.write(
                "video_tasks.json",
                &json!([video("video-1", "processing", "legacy-key-1"), video("video-2", "completed", "legacy-key-1")]),
            ),
            fixture.write(
                "assets.json",
                &json!({"version": 1, "assets": [asset("asset-1", "legacy-key-1")]}),
            ),
        ];
        (fixture, hashes)
    }

    fn complete_fixture() -> Fixture {
        let (fixture, _) = valid_fixture();
        fixture.write(
            "video_tasks.json",
            &json!([video("video-1", "completed", "legacy-key-1")]),
        );
        fixture
    }

    fn admin_principal(data_dir: &Path) -> aiwork_core::Principal {
        let store = aiwork_core::CoreStore::open(data_dir).unwrap();
        store.migrate().unwrap();
        store
            .create_user(
                aiwork_core::NewUser { id: "admin-1".into(), name: "Admin".into(), role: aiwork_core::UserRole::Admin },
                "bootstrap",
            )
            .unwrap();
        store
            .create_user(
                aiwork_core::NewUser { id: "user-1".into(), name: "User".into(), role: aiwork_core::UserRole::User },
                "admin-1",
            )
            .unwrap();
        let key = store
            .issue_api_key("admin-1", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
            .unwrap();
        aiwork_core::Principal {
            user_id: "admin-1".into(),
            key_id: key.id,
            scopes: BTreeSet::from(["admin:*".into()]),
        }
    }

    #[test]
    fn inspect_is_read_only_and_reports_hashes_counts_and_reconcile() {
        let (fixture, hashes) = valid_fixture();
        let before = fs::read(fixture.path("api_keys.json")).unwrap();

        let report = inspect_legacy(&fixture.dir).unwrap();

        assert_eq!(report.key_count, 1);
        assert_eq!(report.asset_count, 1);
        assert_eq!(report.video_count, 2);
        assert_eq!(report.processing_video_count, 1);
        assert_eq!(report.cached_credit_count, 2);
        assert_eq!(report.source_hashes["api_keys.json"], hashes[0]);
        assert_eq!(report.source_hashes["remaining_credits.json"], hashes[1]);
        assert!(report.errors.iter().any(|error| error.contains("reconcile_required")));
        assert_eq!(fs::read(fixture.path("api_keys.json")).unwrap(), before);
        assert!(!fixture.dir.join("data/core.sqlite3").exists());
    }

    #[test]
    fn inspect_reports_corrupt_json_duplicate_ids_and_unknown_status_without_secrets() {
        let fixture = Fixture::new("invalid");
        let secret = "legacy-secret-never-report";
        fixture.write(
            "api_keys.json",
            &json!({"keys": [legacy_key("duplicate-key", secret), legacy_key("duplicate-key", secret)]}),
        );
        fixture.write_raw("remaining_credits.json", "{not-json");
        fixture.write(
            "video_tasks.json",
            &json!([video("duplicate-video", "mystery", "duplicate-key"), video("duplicate-video", "queued", "duplicate-key")]),
        );
        fixture.write(
            "assets.json",
            &json!({"version": 1, "assets": [asset("duplicate-asset", "duplicate-key"), asset("duplicate-asset", "duplicate-key")]}),
        );

        let report = inspect_legacy(&fixture.dir).unwrap();
        let rendered = serde_json::to_string(&report).unwrap();
        assert!(report.errors.iter().any(|error| error.contains("duplicate")));
        assert!(report.errors.iter().any(|error| error.contains("unknown video status")));
        assert!(!rendered.contains(secret));
    }

    #[test]
    fn inspect_reports_missing_legacy_files_instead_of_treating_them_as_empty() {
        let fixture = Fixture::new("missing");
        let report = inspect_legacy(&fixture.dir).unwrap();

        for name in [
            "api_keys.json",
            "remaining_credits.json",
            "video_tasks.json",
            "assets.json",
        ] {
            assert_eq!(report.source_hashes.get(name).map(String::as_str), Some("missing"));
            assert!(report.errors.iter().any(|error| error.contains(name)));
        }
    }

    #[test]
    fn apply_without_complete_mapping_returns_report_and_writes_nothing() {
        let (fixture, _) = valid_fixture();
        let report = apply_legacy(&fixture.dir, &[], &dummy_principal()).unwrap();

        assert_eq!(report.unmapped_keys, vec!["legacy-key-1"]);
        assert!(!report.errors.is_empty());
        assert!(!fixture.dir.join("data/core.sqlite3").exists());
    }

    #[test]
    fn apply_rejects_invalid_owner_and_rolls_back_the_batch() {
        let (fixture, _) = valid_fixture();
        let report = apply_legacy(
            &fixture.dir,
            &[LegacyMigrationMapping {
                legacy_key_id: "legacy-key-1".into(),
                user_id: "missing-user".into(),
            }],
            &dummy_principal(),
        )
        .unwrap();

        assert!(report.errors.iter().any(|error| error.contains("owner")));
        assert!(!fixture.dir.join("data/core.sqlite3").exists());
    }

    #[test]
    fn apply_rejects_duplicate_and_unknown_records_before_core_write() {
        let fixture = Fixture::new("batch-invalid");
        fixture.write(
            "api_keys.json",
            &json!({
                "keys": [legacy_key("legacy-key-1", "secret-a"), legacy_key("legacy-key-1", "secret-b")]
            }),
        );
        fixture.write(
            "video_tasks.json",
            &json!([video("video-1", "unknown", "legacy-key-1")]),
        );
        fixture.write(
            "assets.json",
            &json!({"version": 1, "assets": []}),
        );
        fixture.write("remaining_credits.json", &json!({"credits": {}}));

        let report = apply_legacy(
            &fixture.dir,
            &[LegacyMigrationMapping {
                legacy_key_id: "legacy-key-1".into(),
                user_id: "user-1".into(),
            }],
            &dummy_principal(),
        )
        .unwrap();

        assert!(report.errors.iter().any(|error| error.contains("duplicate")));
        assert!(report.errors.iter().any(|error| error.contains("unknown video status")));
        assert!(!fixture.dir.join("data/core.sqlite3").exists());
    }

    #[test]
    fn apply_does_not_copy_legacy_plaintext_to_core_or_report() {
        let (fixture, _) = valid_fixture();
        let secret = "legacy-secret-key";
        fixture.write(
            "video_tasks.json",
            &json!([video("video-2", "completed", "legacy-key-1")]),
        );
        let store = aiwork_core::CoreStore::open(&fixture.dir).unwrap();
        store.migrate().unwrap();
        store
            .create_user(
                aiwork_core::NewUser {
                    id: "admin-1".into(),
                    name: "Admin 1".into(),
                    role: aiwork_core::UserRole::Admin,
                },
                "bootstrap",
            )
            .unwrap();
        store
            .create_user(
                aiwork_core::NewUser {
                    id: "user-1".into(),
                    name: "User 1".into(),
                    role: aiwork_core::UserRole::User,
                },
                "admin-1",
            )
            .unwrap();
        let admin_key = store
            .issue_api_key(
                "admin-1",
                "admin",
                std::collections::BTreeSet::from(["admin:*".into()]),
                "bootstrap",
            )
            .unwrap();
        let principal = aiwork_core::Principal {
            user_id: "admin-1".into(),
            key_id: admin_key.id,
            scopes: std::collections::BTreeSet::from(["admin:*".into()]),
        };
        drop(store);

        let report = apply_legacy(
            &fixture.dir,
            &[LegacyMigrationMapping {
                legacy_key_id: "legacy-key-1".into(),
                user_id: "user-1".into(),
            }],
            &principal,
        )
        .unwrap();
        let rendered = serde_json::to_string(&report).unwrap();
        let db = fs::read(fixture.dir.join("data/core.sqlite3")).unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(!rendered.contains(secret));
        assert!(!String::from_utf8_lossy(&db).contains(secret));
    }

    #[test]
    fn apply_rejects_processing_video_without_any_migration_rows() {
        let (fixture, _) = valid_fixture();
        let principal = admin_principal(&fixture.dir);
        let report = apply_legacy(&fixture.dir, &[LegacyMigrationMapping {
            legacy_key_id: "legacy-key-1".into(),
            user_id: "user-1".into(),
        }], &principal).unwrap();
        assert!(report.processing_video_count > 0);
        assert!(report.errors.iter().any(|error| error.contains("reconcile_required")));

        let connection = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        for table in ["legacy_migration_records", "legacy_key_registry", "legacy_assets", "legacy_jobs", "legacy_observations"] {
            assert_eq!(connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| row.get::<_, i64>(0)).unwrap(), 0, "partial migration in {table}");
        }
    }

    #[test]
    fn apply_rejects_missing_legacy_asset_file_without_writing() {
        let fixture = complete_fixture();
        fs::remove_file(fixture.dir.join("data/assets/asset-1.png")).unwrap();
        let principal = admin_principal(&fixture.dir);
        let report = apply_legacy(&fixture.dir, &[LegacyMigrationMapping {
            legacy_key_id: "legacy-key-1".into(),
            user_id: "user-1".into(),
        }], &principal).unwrap();
        assert!(report.errors.iter().any(|error| error.contains("missing")));
        let connection = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_assets", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_key_registry", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn apply_rejects_legacy_asset_hash_mismatch_without_writing() {
        let fixture = complete_fixture();
        fs::write(fixture.dir.join("data/assets/asset-1.png"), b"xyz").unwrap();
        let principal = admin_principal(&fixture.dir);
        let report = apply_legacy(&fixture.dir, &[LegacyMigrationMapping {
            legacy_key_id: "legacy-key-1".into(),
            user_id: "user-1".into(),
        }], &principal).unwrap();
        assert!(report.errors.iter().any(|error| error.contains("hash")));
        let connection = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_assets", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM legacy_key_registry", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    #[test]
    fn remaining_credit_history_is_observation_only_and_preserves_summary_fields() {
        let fixture = complete_fixture();
        let principal = admin_principal(&fixture.dir);
        let report = apply_legacy(&fixture.dir, &[LegacyMigrationMapping {
            legacy_key_id: "legacy-key-1".into(),
            user_id: "user-1".into(),
        }], &principal).unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        let connection = rusqlite::Connection::open(fixture.dir.join("data/core.sqlite3")).unwrap();
        let summary: String = connection.query_row("SELECT summary_json FROM legacy_observations WHERE account_ref = 'account-1'", [], |row| row.get(0)).unwrap();
        let summary: serde_json::Value = serde_json::from_str(&summary).unwrap();
        assert_eq!(summary["credits"]["account-1"], 12.5);
        assert_eq!(summary["expire_times"]["account-1"], 1700000000);
        assert_eq!(summary["general"]["account-1"], 10.0);
        assert_eq!(summary["work"]["account-1"], 2.5);
        assert_eq!(summary["membership_expire"]["account-1"], 1800000000);
        assert_eq!(summary["membership_next_billing"]["account-1"], 1710000000);
        assert_eq!(summary["updated_at"], "2026-09-19T00:00:00Z");
        assert_eq!(connection.query_row("SELECT COUNT(*) FROM quota_ledger", [], |row| row.get::<_, i64>(0)).unwrap(), 0);
    }

    fn dummy_principal() -> aiwork_core::Principal {
        aiwork_core::Principal {
            user_id: "admin-1".into(),
            key_id: "admin-key".into(),
            scopes: std::collections::BTreeSet::new(),
        }
    }
}
