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
