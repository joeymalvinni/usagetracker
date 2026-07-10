CREATE TABLE IF NOT EXISTS notification_window_state (
  account_id TEXT NOT NULL,
  window_id TEXT NOT NULL,
  last_percent REAL NOT NULL,
  reset_at TEXT,
  notified_mask INTEGER NOT NULL DEFAULT 0,
  last_attempt_at TEXT,
  PRIMARY KEY(account_id, window_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

