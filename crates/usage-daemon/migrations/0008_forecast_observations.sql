BEGIN;

CREATE TABLE IF NOT EXISTS usage_schema_migrations (
  version INTEGER PRIMARY KEY,
  applied_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage_window_observations (
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

CREATE INDEX IF NOT EXISTS usage_window_observations_lookup
ON usage_window_observations(
  provider_id,
  account_id,
  window_id,
  collected_at DESC,
  snapshot_sequence DESC,
  percent_used,
  reset_at
);

INSERT OR IGNORE INTO usage_window_observations (
  snapshot_id,
  snapshot_sequence,
  provider_id,
  account_id,
  window_id,
  collected_at,
  percent_used,
  reset_at
)
SELECT snapshot.id,
       snapshot.rowid,
       snapshot.provider_id,
       snapshot.account_id,
       json_extract(item.value, '$.window_id'),
       snapshot.collected_at,
       CAST(json_extract(item.value, '$.percent_used') AS REAL),
       json_extract(item.value, '$.reset_at')
FROM usage_snapshots AS snapshot,
     json_each(snapshot.normalized_json, '$.windows') AS item
WHERE NOT EXISTS (
        SELECT 1 FROM usage_schema_migrations WHERE version = 7
      )
  AND json_type(item.value, '$.window_id') = 'text'
  AND json_type(item.value, '$.percent_used') IN ('integer', 'real');

INSERT OR IGNORE INTO usage_schema_migrations(version, applied_at)
VALUES (7, CURRENT_TIMESTAMP);

COMMIT;
