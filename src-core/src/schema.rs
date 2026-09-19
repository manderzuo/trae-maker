pub(crate) const SCHEMA_V1: &str = r#"
CREATE TABLE schema_meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE users (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('admin','operator','user')),
  status TEXT NOT NULL CHECK(status IN ('active','disabled')),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE api_keys (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  name TEXT NOT NULL,
  prefix TEXT NOT NULL,
  key_digest BLOB NOT NULL UNIQUE,
  scopes_json TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN ('active','revoked')),
  created_at_ms INTEGER NOT NULL,
  revoked_at_ms INTEGER
);
CREATE TABLE cost_policies (
  id TEXT PRIMARY KEY,
  endpoint TEXT NOT NULL,
  model_pattern TEXT NOT NULL,
  resource_kind TEXT NOT NULL,
  reserve_amount INTEGER NOT NULL CHECK(reserve_amount > 0),
  max_actual_amount INTEGER,
  version INTEGER NOT NULL,
  enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
  UNIQUE(endpoint, model_pattern, resource_kind, version)
);
CREATE TABLE quota_ledger (
  entry_id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  resource_kind TEXT NOT NULL,
  event_kind TEXT NOT NULL,
  amount INTEGER NOT NULL CHECK(amount >= 0),
  delta INTEGER NOT NULL,
  request_id TEXT,
  actor_user_id TEXT,
  reason TEXT,
  created_at_ms INTEGER NOT NULL
);
CREATE TABLE quota_reservations (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  request_id TEXT NOT NULL UNIQUE,
  resource_kind TEXT NOT NULL,
  amount INTEGER NOT NULL CHECK(amount > 0),
  state TEXT NOT NULL CHECK(state IN ('held','committed','released','unknown')),
  expires_at_ms INTEGER NOT NULL,
  created_at_ms INTEGER NOT NULL,
  settled_at_ms INTEGER
);
CREATE TABLE requests (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  api_key_id TEXT NOT NULL REFERENCES api_keys(id),
  protocol TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  state TEXT NOT NULL,
  result_status INTEGER,
  error_code TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
CREATE TABLE idempotency_keys (
  scope TEXT NOT NULL,
  client_key TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  request_id TEXT NOT NULL REFERENCES requests(id),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(scope, client_key)
);
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
"#;

pub(crate) const SCHEMA_V2: &str = r#"
CREATE TABLE requests_next (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  api_key_id TEXT NOT NULL REFERENCES api_keys(id),
  protocol TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('received','validating','reserved','queued','dispatched','completing','succeeded','failed','unknown','settled')),
  result_status INTEGER,
  error_code TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
INSERT INTO requests_next
  (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms)
  SELECT id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms
  FROM requests;
CREATE TABLE idempotency_keys_next (
  scope TEXT NOT NULL,
  client_key TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  request_id TEXT NOT NULL REFERENCES requests_next(id),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(scope, client_key)
);
INSERT INTO idempotency_keys_next (scope, client_key, request_hash, request_id, created_at_ms)
  SELECT scope, client_key, request_hash, request_id, created_at_ms FROM idempotency_keys;
DROP TABLE idempotency_keys;
DROP TABLE requests;
ALTER TABLE requests_next RENAME TO requests;
ALTER TABLE idempotency_keys_next RENAME TO idempotency_keys;
"#;

pub(crate) const SCHEMA_V3: &str = r#"
CREATE TABLE legacy_migration_records (
  migration_id TEXT NOT NULL,
  source_file TEXT NOT NULL,
  source_hash TEXT NOT NULL,
  actor_user_id TEXT NOT NULL REFERENCES users(id),
  reason TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(migration_id, source_file)
);
CREATE TABLE legacy_key_registry (
  legacy_key_id TEXT PRIMARY KEY,
  key_digest BLOB NOT NULL UNIQUE,
  migrated_user_id TEXT NOT NULL REFERENCES users(id),
  status TEXT NOT NULL CHECK(status IN ('migration_legacy','disabled')),
  migration_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL REFERENCES users(id),
  reason TEXT NOT NULL,
  disabled_at_ms INTEGER NOT NULL
);
CREATE TABLE legacy_assets (
  id TEXT PRIMARY KEY,
  owner_key_id TEXT NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id),
  filename TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  extension TEXT NOT NULL,
  size INTEGER NOT NULL CHECK(size >= 0),
  content_sha256 TEXT NOT NULL,
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL,
  migration_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL REFERENCES users(id),
  reason TEXT NOT NULL
);
CREATE TABLE legacy_jobs (
  id TEXT PRIMARY KEY,
  owner_key_id TEXT NOT NULL,
  user_id TEXT NOT NULL REFERENCES users(id),
  status TEXT NOT NULL CHECK(status IN ('queued','completed','failed','unknown')),
  reconcile_required INTEGER NOT NULL CHECK(reconcile_required IN (0,1)),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  migration_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL REFERENCES users(id),
  reason TEXT NOT NULL
);
CREATE TABLE legacy_observations (
  id TEXT PRIMARY KEY,
  account_ref TEXT NOT NULL,
  resource_kind TEXT NOT NULL,
  value_json TEXT NOT NULL,
  source TEXT NOT NULL CHECK(source = 'json_cache'),
  observed_at_ms INTEGER NOT NULL,
  migration_id TEXT NOT NULL,
  actor_user_id TEXT NOT NULL REFERENCES users(id),
  reason TEXT NOT NULL
);
"#;

pub(crate) const SCHEMA_V4: &str = r#"
ALTER TABLE legacy_assets ADD COLUMN storage_ref TEXT NOT NULL DEFAULT '';
ALTER TABLE legacy_assets ADD COLUMN migration_status TEXT NOT NULL DEFAULT 'legacy_unverified' CHECK(migration_status IN ('verified','legacy_unverified','reconcile_required'));
ALTER TABLE legacy_observations ADD COLUMN observed_value INTEGER;
ALTER TABLE legacy_observations ADD COLUMN summary_json TEXT NOT NULL DEFAULT '{}';
"#;

pub(crate) const SCHEMA_V5: &str = r#"
CREATE TRIGGER legacy_assets_storage_ref_guard_insert
BEFORE INSERT ON legacy_assets
WHEN (
  (NEW.storage_ref <> '' AND (
    NEW.storage_ref NOT GLOB 'assets/*'
    OR length(NEW.storage_ref) <= length('assets/')
    OR trim(NEW.storage_ref) <> NEW.storage_ref
    OR instr(NEW.storage_ref, '..') > 0
    OR instr(NEW.storage_ref, char(92)) > 0
    OR instr(NEW.storage_ref, '//') > 0
    OR instr(NEW.storage_ref, char(9)) > 0
    OR instr(NEW.storage_ref, char(10)) > 0
    OR instr(NEW.storage_ref, char(13)) > 0
  ))
  OR (NEW.migration_status = 'verified' AND (
    NEW.storage_ref = ''
    OR NEW.storage_ref NOT GLOB 'assets/*'
    OR length(NEW.storage_ref) <= length('assets/')
    OR trim(NEW.storage_ref) <> NEW.storage_ref
    OR instr(NEW.storage_ref, '..') > 0
    OR instr(NEW.storage_ref, char(92)) > 0
    OR instr(NEW.storage_ref, '//') > 0
    OR instr(NEW.storage_ref, char(9)) > 0
    OR instr(NEW.storage_ref, char(10)) > 0
    OR instr(NEW.storage_ref, char(13)) > 0
  ))
)
BEGIN
  SELECT RAISE(ABORT, 'invalid legacy asset storage_ref');
END;

CREATE TRIGGER legacy_assets_storage_ref_guard_update
BEFORE UPDATE OF storage_ref, migration_status ON legacy_assets
WHEN (
  (NEW.storage_ref <> '' AND (
    NEW.storage_ref NOT GLOB 'assets/*'
    OR length(NEW.storage_ref) <= length('assets/')
    OR trim(NEW.storage_ref) <> NEW.storage_ref
    OR instr(NEW.storage_ref, '..') > 0
    OR instr(NEW.storage_ref, char(92)) > 0
    OR instr(NEW.storage_ref, '//') > 0
    OR instr(NEW.storage_ref, char(9)) > 0
    OR instr(NEW.storage_ref, char(10)) > 0
    OR instr(NEW.storage_ref, char(13)) > 0
  ))
  OR (NEW.migration_status = 'verified' AND (
    NEW.storage_ref = ''
    OR NEW.storage_ref NOT GLOB 'assets/*'
    OR length(NEW.storage_ref) <= length('assets/')
    OR trim(NEW.storage_ref) <> NEW.storage_ref
    OR instr(NEW.storage_ref, '..') > 0
    OR instr(NEW.storage_ref, char(92)) > 0
    OR instr(NEW.storage_ref, '//') > 0
    OR instr(NEW.storage_ref, char(9)) > 0
    OR instr(NEW.storage_ref, char(10)) > 0
    OR instr(NEW.storage_ref, char(13)) > 0
  ))
)
BEGIN
  SELECT RAISE(ABORT, 'invalid legacy asset storage_ref');
END;
"#;

pub(crate) const SCHEMA_V6: &str = r#"
CREATE TABLE upstream_accounts (
  id TEXT PRIMARY KEY, provider TEXT NOT NULL, credentials_ref TEXT NOT NULL, region TEXT,
  capabilities_json TEXT NOT NULL CHECK(json_valid(capabilities_json)),
  enabled INTEGER NOT NULL CHECK(enabled IN (0,1)),
  max_concurrency INTEGER NOT NULL CHECK(max_concurrency > 0),
  state TEXT NOT NULL CHECK(state IN ('available','cooling','forbidden','disabled')),
  cooldown_until_ms INTEGER, cooldown_reason TEXT,
  consecutive_errors INTEGER NOT NULL CHECK(consecutive_errors >= 0),
  created_at_ms INTEGER NOT NULL, updated_at_ms INTEGER NOT NULL
);
CREATE TABLE upstream_observations_next (
  id TEXT PRIMARY KEY, account_ref TEXT NOT NULL REFERENCES upstream_accounts(id),
  resource_kind TEXT NOT NULL, observed_value INTEGER, value_scale INTEGER NOT NULL CHECK(value_scale > 0),
  source TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('fresh','stale','failed')),
  observed_at_ms INTEGER NOT NULL, stale_at_ms INTEGER NOT NULL,
  summary_json TEXT NOT NULL CHECK(json_valid(summary_json))
);
"#;

pub(crate) const SCHEMA_V6_FINISH: &str = r#"
CREATE TABLE upstream_leases (
  id TEXT PRIMARY KEY, request_id TEXT NOT NULL REFERENCES requests(id),
  account_ref TEXT NOT NULL REFERENCES upstream_accounts(id), resource_kind TEXT NOT NULL,
  predicted_units INTEGER NOT NULL CHECK(predicted_units > 0),
  observation_id TEXT REFERENCES upstream_observations(id),
  state TEXT NOT NULL CHECK(state IN ('held','active','succeeded','failed','unknown','released')),
  lease_expires_at_ms INTEGER NOT NULL, reconcile_until_ms INTEGER,
  upstream_request_ref TEXT, error_kind TEXT, created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL, settled_at_ms INTEGER,
  UNIQUE(request_id, resource_kind)
);
CREATE INDEX upstream_accounts_by_provider_state ON upstream_accounts(provider, state);
CREATE INDEX upstream_observations_by_account_resource_time ON upstream_observations(account_ref, resource_kind, observed_at_ms DESC);
CREATE INDEX upstream_leases_by_account_state ON upstream_leases(account_ref, state);
CREATE INDEX upstream_leases_by_request_resource ON upstream_leases(request_id, resource_kind);
CREATE INDEX upstream_leases_recoverable ON upstream_leases(state, lease_expires_at_ms) WHERE state IN ('held','active','unknown');
"#;

pub(crate) const SCHEMA_V7: &str = r#"
CREATE TABLE requests_next (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  api_key_id TEXT NOT NULL REFERENCES api_keys(id),
  protocol TEXT NOT NULL,
  endpoint TEXT NOT NULL,
  model TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  state TEXT NOT NULL CHECK(state IN ('received','validating','reserved','queued','dispatched','completing','cancel_requested','canceled','succeeded','failed','unknown','settled')),
  result_status INTEGER,
  error_code TEXT,
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL
);
INSERT INTO requests_next
  (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms)
  SELECT id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, result_status, error_code, created_at_ms, updated_at_ms
  FROM requests;
CREATE TABLE idempotency_keys_next (
  scope TEXT NOT NULL,
  client_key TEXT NOT NULL,
  request_hash BLOB NOT NULL,
  request_id TEXT NOT NULL REFERENCES requests_next(id),
  created_at_ms INTEGER NOT NULL,
  PRIMARY KEY(scope, client_key)
);
INSERT INTO idempotency_keys_next (scope, client_key, request_hash, request_id, created_at_ms)
  SELECT scope, client_key, request_hash, request_id, created_at_ms FROM idempotency_keys;
DROP TABLE idempotency_keys;
DROP TABLE requests;
ALTER TABLE requests_next RENAME TO requests;
ALTER TABLE idempotency_keys_next RENAME TO idempotency_keys;
"#;

pub(crate) const SCHEMA_V8: &str = r#"
CREATE TABLE assets (
  id TEXT PRIMARY KEY,
  user_id TEXT NOT NULL REFERENCES users(id),
  filename TEXT NOT NULL,
  mime_type TEXT NOT NULL,
  extension TEXT NOT NULL,
  size INTEGER NOT NULL CHECK(size > 0),
  sha256 TEXT NOT NULL,
  storage_ref TEXT NOT NULL CHECK(
    storage_ref GLOB 'assets/*'
    AND length(storage_ref) > length('assets/')
    AND trim(storage_ref) = storage_ref
    AND instr(storage_ref, '..') = 0
    AND instr(storage_ref, char(92)) = 0
    AND instr(storage_ref, '//') = 0
    AND instr(storage_ref, char(9)) = 0
    AND instr(storage_ref, char(10)) = 0
    AND instr(storage_ref, char(13)) = 0
  ),
  content_token_digest BLOB NOT NULL CHECK(length(content_token_digest) = 32),
  created_at_ms INTEGER NOT NULL,
  expires_at_ms INTEGER NOT NULL CHECK(expires_at_ms > created_at_ms),
  state TEXT NOT NULL CHECK(state IN ('active','expired')),
  UNIQUE(content_token_digest)
);
CREATE INDEX assets_by_user_state_created ON assets(user_id, state, created_at_ms DESC);
CREATE INDEX assets_by_expiry ON assets(state, expires_at_ms);
"#;

pub(crate) const SCHEMA_V9: &str = r#"
CREATE TABLE jobs (
  id TEXT PRIMARY KEY,
  request_id TEXT NOT NULL UNIQUE REFERENCES requests(id),
  user_id TEXT NOT NULL REFERENCES users(id),
  kind TEXT NOT NULL CHECK(kind = 'video'),
  model TEXT NOT NULL,
  input_hash BLOB NOT NULL CHECK(length(input_hash) = 32),
  state TEXT NOT NULL CHECK(state IN ('created','queued','running','cancel_requested','canceled','succeeded','failed','unknown')),
  output_ref TEXT,
  artifact_ref TEXT,
  error_code TEXT,
  reconcile_required INTEGER NOT NULL CHECK(reconcile_required IN (0,1)),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  last_heartbeat_ms INTEGER,
  cancel_requested_at_ms INTEGER
);
CREATE TABLE job_attempts (
  id TEXT PRIMARY KEY,
  job_id TEXT NOT NULL REFERENCES jobs(id),
  attempt_no INTEGER NOT NULL CHECK(attempt_no > 0),
  account_ref TEXT NOT NULL REFERENCES upstream_accounts(id),
  lease_id TEXT NOT NULL UNIQUE REFERENCES upstream_leases(id),
  upstream_request_ref TEXT,
  state TEXT NOT NULL CHECK(state IN ('queued','running','cancel_requested','canceled','succeeded','failed','unknown')),
  error_code TEXT,
  retryable INTEGER NOT NULL CHECK(retryable IN (0,1)),
  created_at_ms INTEGER NOT NULL,
  updated_at_ms INTEGER NOT NULL,
  last_heartbeat_ms INTEGER,
  finished_at_ms INTEGER,
  UNIQUE(job_id, attempt_no)
);
CREATE INDEX jobs_by_user_state_updated ON jobs(user_id, state, updated_at_ms DESC);
CREATE INDEX jobs_by_recovery ON jobs(state, reconcile_required, updated_at_ms);
CREATE INDEX job_attempts_by_job_attempt ON job_attempts(job_id, attempt_no DESC);
CREATE INDEX job_attempts_by_recovery ON job_attempts(state, updated_at_ms);
"#;

pub(crate) const SCHEMA_V10: &str = r#"
CREATE TABLE dispatch_queue_cursors (
  resource_kind TEXT PRIMARY KEY,
  last_user_id TEXT,
  updated_at_ms INTEGER NOT NULL
);
CREATE INDEX jobs_by_queue_claim ON jobs(kind, state, created_at_ms, id);
"#;

pub(crate) const SCHEMA_V11: &str = r#"
ALTER TABLE jobs ADD COLUMN queue_provider_hint TEXT;
ALTER TABLE jobs ADD COLUMN queue_required_capabilities_json TEXT;
ALTER TABLE jobs ADD COLUMN queue_region TEXT;
ALTER TABLE jobs ADD COLUMN queue_predicted_units INTEGER;
ALTER TABLE jobs ADD COLUMN queue_safety_margin_units INTEGER;
ALTER TABLE jobs ADD COLUMN queue_observation_max_age_ms INTEGER;
ALTER TABLE jobs ADD COLUMN queue_allowed_accounts_json TEXT;
ALTER TABLE jobs ADD COLUMN queue_dedicated_account TEXT;
ALTER TABLE jobs ADD COLUMN queue_selection_strategy TEXT;
ALTER TABLE jobs ADD COLUMN queue_lease_ttl_ms INTEGER;
ALTER TABLE jobs ADD COLUMN queue_reconcile_ttl_ms INTEGER;
ALTER TABLE jobs ADD COLUMN queue_claim_owner TEXT;
ALTER TABLE jobs ADD COLUMN queue_claim_expires_at_ms INTEGER;
CREATE INDEX jobs_by_queue_owner ON jobs(queue_claim_owner, queue_claim_expires_at_ms);
"#;
