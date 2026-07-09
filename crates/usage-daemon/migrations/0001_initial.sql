CREATE TABLE IF NOT EXISTS accounts (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  external_account_id TEXT NOT NULL,
  profile_id TEXT NOT NULL DEFAULT '',
  display_name TEXT,
  hidden INTEGER NOT NULL DEFAULT 0,
  collection_enabled INTEGER NOT NULL DEFAULT 1,
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(provider_id, profile_id)
);

CREATE INDEX IF NOT EXISTS accounts_provider_external_account
ON accounts(provider_id, external_account_id);

CREATE TABLE IF NOT EXISTS usage_snapshots (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  normalized_json TEXT NOT NULL,
  metadata_json TEXT,
  FOREIGN KEY(account_id) REFERENCES accounts(id)
);

CREATE INDEX IF NOT EXISTS usage_snapshots_provider_account_time
ON usage_snapshots(provider_id, account_id, collected_at DESC);

CREATE TABLE IF NOT EXISTS raw_payloads (
  id TEXT PRIMARY KEY,
  snapshot_id TEXT NOT NULL,
  provider_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  payload_json TEXT NOT NULL,
  FOREIGN KEY(snapshot_id) REFERENCES usage_snapshots(id)
);

CREATE TABLE IF NOT EXISTS provider_health (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  status TEXT NOT NULL,
  collection_mode TEXT,
  last_success_at TEXT,
  last_failure_at TEXT,
  last_error_code TEXT,
  last_error_message TEXT,
  updated_at TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id)
);
