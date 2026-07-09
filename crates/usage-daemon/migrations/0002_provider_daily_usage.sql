CREATE TABLE IF NOT EXISTS provider_daily_usage (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  usage_date TEXT NOT NULL,
  tokens INTEGER NOT NULL CHECK(tokens >= 0),
  cost_usd REAL,
  source TEXT NOT NULL,
  collected_at TEXT NOT NULL,
  PRIMARY KEY(provider_id, account_id, usage_date),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS provider_daily_usage_account_date
ON provider_daily_usage(account_id, usage_date);
