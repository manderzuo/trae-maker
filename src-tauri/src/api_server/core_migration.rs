//! Legacy JSON inspection and explicitly mapped Core migration.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;
use std::sync::Arc;

use aiwork_core::{CoreError, CoreStore, IssuedApiKey};
use rusqlite::{Connection, OptionalExtension};
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

#[derive(Deserialize)]
struct LegacyApiKey {
    id: String,
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "key")]
    _key: String,
    #[serde(default)]
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

#[derive(Deserialize)]
struct LegacyRemainingCredits {
    #[serde(default)]
    credits: std::collections::HashMap<String, f64>,
    #[serde(default)]
    _expire_times: std::collections::HashMap<String, i64>,
    #[serde(default)]
    _general: std::collections::HashMap<String, f64>,
    #[serde(default)]
    _work: std::collections::HashMap<String, f64>,
    #[serde(default)]
    _membership_expire: std::collections::HashMap<String, i64>,
    #[serde(default)]
    _membership_next_billing: std::collections::HashMap<String, i64>,
    #[serde(default)]
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
    videos: Vec<LegacyVideoTask>,
    assets: Vec<LegacyAsset>,
}

fn source_path(data_dir: &Path, file_name: &str) -> std::path::PathBuf {
    data_dir.join("data").join(file_name)
}

fn read_source(data_dir: &Path, file_name: &str, report: &mut MigrationReport) -> Result<Option<Vec<u8>>, CoreError> {
    let path = source_path(data_dir, file_name);
    if !path.is_file() {
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
            videos,
            assets: assets_file.assets,
        },
    ))
}

/// Inspect legacy files without opening or creating Core storage.
pub fn inspect_legacy(data_dir: &Path) -> Result<MigrationReport, CoreError> {
    scan_legacy(data_dir).map(|(report, _)| report)
}

fn validate_core_owners(
    data_dir: &Path,
    owner_ids: impl Iterator<Item = String>,
    actor_user_id: &str,
    report: &mut MigrationReport,
) -> Result<(), CoreError> {
    let db_path = data_dir.join("data").join(aiwork_core::CORE_DB_FILE);
    if !db_path.is_file() {
        report.errors.push("Core owner does not exist".into());
        return Ok(());
    }
    let connection = Connection::open(db_path)?;
    let mut owners = HashSet::new();
    for owner_id in owner_ids {
        if owners.insert(owner_id.clone()) {
            let exists = connection
                .query_row(
                    "SELECT 1 FROM users WHERE id = ?1 AND status = 'active'",
                    [&owner_id],
                    |_| Ok(()),
                )
                .optional()?;
            if exists.is_none() {
                report.errors.push("mapped Core owner does not exist or is disabled".into());
            }
        }
    }
    let actor_role = connection
        .query_row(
            "SELECT role FROM users WHERE id = ?1 AND status = 'active'",
            [actor_user_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if actor_role.as_deref() != Some("admin") {
        report.errors.push("actor_user_id is not an active admin".into());
    }
    Ok(())
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
    actor_user_id: &str,
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
    if actor_user_id.trim().is_empty() {
        report.errors.push("actor_user_id is required".into());
    }
    if !report.unmapped_keys.is_empty() {
        report.errors.push("legacy key mapping is incomplete".into());
    }
    if actor_user_id.trim().is_empty() == false && report.unmapped_keys.is_empty() {
        validate_core_owners(
            data_dir,
            mappings.iter().map(|mapping| mapping.user_id.clone()),
            actor_user_id,
            &mut report,
        )?;
    }
    Ok((report, snapshot))
}

fn issue_migrated_keys(
    store: Arc<CoreStore>,
    mappings: &[LegacyMigrationMapping],
    actor_user_id: &str,
    report: MigrationReport,
) -> Result<MigrationApplyResponse, CoreError> {
    store.migrate()?;
    let scopes = MIGRATION_SCOPES
        .into_iter()
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let mut issued_keys = Vec::with_capacity(mappings.len());
    for mapping in mappings {
        let issued = store.issue_api_key(
            &mapping.user_id,
            &format!("legacy migration {}", mapping.legacy_key_id),
            scopes.clone(),
            actor_user_id,
        )?;
        issued_keys.push(response_from_issued(issued));
    }
    Ok(MigrationApplyResponse { report, issued_keys })
}

pub fn apply_legacy_with_store(
    data_dir: &Path,
    store: Arc<CoreStore>,
    mappings: &[LegacyMigrationMapping],
    actor_user_id: &str,
) -> Result<MigrationApplyResponse, CoreError> {
    let (report, _snapshot) = prepare_legacy_apply(data_dir, mappings, actor_user_id)?;
    if !report.errors.is_empty() {
        return Ok(MigrationApplyResponse {
            report,
            issued_keys: Vec::new(),
        });
    }
    issue_migrated_keys(store, mappings, actor_user_id, report)
}

pub fn apply_legacy(
    data_dir: &Path,
    mappings: &[LegacyMigrationMapping],
    actor_user_id: &str,
) -> Result<MigrationReport, CoreError> {
    let (report, _) = prepare_legacy_apply(data_dir, mappings, actor_user_id)?;
    if !report.errors.is_empty() {
        return Ok(report);
    }
    let store = Arc::new(CoreStore::open(data_dir)?);
    Ok(issue_migrated_keys(store, mappings, actor_user_id, report)?.report)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

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
            "sha256": "asset-content-hash",
            "created_at": 1,
            "expires_at": 2,
            "public_token": "asset-public-token"
        })
    }

    fn valid_fixture() -> (Fixture, [String; 4]) {
        let fixture = Fixture::new("valid");
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
                    "expire_times": {},
                    "membership_expire": {},
                    "membership_next_billing": {},
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
    fn apply_without_complete_mapping_returns_report_and_writes_nothing() {
        let (fixture, _) = valid_fixture();
        let report = apply_legacy(&fixture.dir, &[], "admin-1").unwrap();

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
            "admin-1",
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
            "admin-1",
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
                    id: "user-1".into(),
                    name: "User 1".into(),
                    role: aiwork_core::UserRole::User,
                },
                "admin-1",
            )
            .unwrap();
        store
            .create_user(
                aiwork_core::NewUser {
                    id: "admin-1".into(),
                    name: "Admin 1".into(),
                    role: aiwork_core::UserRole::Admin,
                },
                "admin-1",
            )
            .unwrap();
        drop(store);

        let report = apply_legacy(
            &fixture.dir,
            &[LegacyMigrationMapping {
                legacy_key_id: "legacy-key-1".into(),
                user_id: "user-1".into(),
            }],
            "admin-1",
        )
        .unwrap();
        let rendered = serde_json::to_string(&report).unwrap();
        let db = fs::read(fixture.dir.join("data/core.sqlite3")).unwrap();
        assert!(report.errors.is_empty(), "{report:?}");
        assert!(!rendered.contains(secret));
        assert!(!String::from_utf8_lossy(&db).contains(secret));
    }
}
