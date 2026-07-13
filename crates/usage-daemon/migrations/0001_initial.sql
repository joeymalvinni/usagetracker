CREATE TABLE schema_migrations (
  version INTEGER PRIMARY KEY,
  name TEXT NOT NULL UNIQUE,
  applied_at TEXT NOT NULL
);

CREATE TABLE accounts (
  id TEXT PRIMARY KEY,
  provider_id TEXT NOT NULL,
  external_account_id TEXT NOT NULL,
  profile_id TEXT NOT NULL,
  display_name TEXT,
  display_name_source TEXT NOT NULL CHECK(display_name_source IN ('provider', 'generated', 'user')),
  email TEXT,
  hidden INTEGER NOT NULL DEFAULT 0 CHECK(hidden IN (0, 1)),
  collection_enabled INTEGER NOT NULL DEFAULT 1 CHECK(collection_enabled IN (0, 1)),
  created_at TEXT NOT NULL,
  updated_at TEXT NOT NULL,
  UNIQUE(provider_id, profile_id)
);

CREATE INDEX accounts_provider_external_account
ON accounts(provider_id, external_account_id);

CREATE TABLE usage_snapshots (
  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  normalized_json TEXT NOT NULL,
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE INDEX usage_snapshots_provider_account_time
ON usage_snapshots(provider_id, account_id, collected_at DESC, sequence DESC);

CREATE TABLE usage_window_observations (
  snapshot_id TEXT NOT NULL,
  snapshot_sequence INTEGER NOT NULL,
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  window_id TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  percent_used REAL NOT NULL,
  reset_at TEXT,
  PRIMARY KEY(snapshot_id, window_id),
  FOREIGN KEY(snapshot_id) REFERENCES usage_snapshots(id) ON DELETE CASCADE,
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
) WITHOUT ROWID;

CREATE INDEX usage_window_observations_lookup
ON usage_window_observations(
  provider_id, account_id, window_id, collected_at DESC,
  snapshot_sequence DESC, percent_used, reset_at
);

CREATE TABLE provider_daily_usage (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  usage_date TEXT NOT NULL,
  tokens INTEGER NOT NULL CHECK(tokens >= 0),
  cost_usd REAL CHECK(cost_usd IS NULL OR cost_usd >= 0),
  source TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id, usage_date),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE INDEX provider_daily_usage_account_date
ON provider_daily_usage(account_id, usage_date);

CREATE TABLE provider_daily_usage_summary (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  bucket_count INTEGER NOT NULL CHECK(bucket_count >= 0),
  total_tokens INTEGER NOT NULL CHECK(total_tokens >= 0),
  PRIMARY KEY(provider_id, account_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE TRIGGER provider_daily_usage_summary_insert
AFTER INSERT ON provider_daily_usage
BEGIN
  INSERT INTO provider_daily_usage_summary
    (provider_id, account_id, bucket_count, total_tokens)
  VALUES (NEW.provider_id, NEW.account_id, 1, NEW.tokens)
  ON CONFLICT(provider_id, account_id) DO UPDATE SET
    bucket_count = bucket_count + 1,
    total_tokens = total_tokens + NEW.tokens;
END;

CREATE TRIGGER provider_daily_usage_summary_update
AFTER UPDATE OF tokens ON provider_daily_usage
WHEN OLD.tokens != NEW.tokens
BEGIN
  UPDATE provider_daily_usage_summary
  SET total_tokens = total_tokens - OLD.tokens + NEW.tokens
  WHERE provider_id = NEW.provider_id AND account_id = NEW.account_id;
END;

CREATE TRIGGER provider_daily_usage_summary_delete
AFTER DELETE ON provider_daily_usage
BEGIN
  UPDATE provider_daily_usage_summary
  SET bucket_count = bucket_count - 1,
      total_tokens = total_tokens - OLD.tokens
  WHERE provider_id = OLD.provider_id AND account_id = OLD.account_id;
  DELETE FROM provider_daily_usage_summary
  WHERE provider_id = OLD.provider_id
    AND account_id = OLD.account_id
    AND bucket_count = 0;
END;

CREATE TABLE provider_health (
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

CREATE TABLE provider_backoff (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  consecutive_failures INTEGER NOT NULL CHECK(consecutive_failures > 0),
  retry_at TEXT NOT NULL,
  last_failure_at TEXT NOT NULL,
  error_message TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE INDEX provider_backoff_retry_at ON provider_backoff(retry_at);

CREATE TABLE notification_window_state (
  account_id TEXT NOT NULL,
  window_id TEXT NOT NULL,
  reset_at TEXT,
  notified_mask INTEGER NOT NULL DEFAULT 0,
  last_attempt_at TEXT,
  PRIMARY KEY(account_id, window_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE TABLE pending_notifications (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  title TEXT NOT NULL,
  body TEXT NOT NULL,
  created_at TEXT NOT NULL
);
