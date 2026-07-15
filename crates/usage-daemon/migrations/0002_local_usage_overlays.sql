CREATE TABLE IF NOT EXISTS local_usage_overlays (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  source TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  dataset_json TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id, source),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);
