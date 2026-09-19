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
  state TEXT NOT NULL CHECK(state IN ('received','validating','reserved','queued','dispatched','completing','succeeded','failed','unknown','settled')),
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
