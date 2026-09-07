CREATE TABLE provider_usage_events (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  event_id TEXT NOT NULL,
  occurred_at TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  normalized_json TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id, event_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE INDEX provider_usage_events_account_time
ON provider_usage_events(account_id, occurred_at DESC, event_id DESC);
