CREATE TABLE IF NOT EXISTS provider_daily_usage_summary (
  provider_id TEXT NOT NULL,
  account_id TEXT NOT NULL,
  bucket_count INTEGER NOT NULL CHECK(bucket_count >= 0),
  total_tokens INTEGER NOT NULL CHECK(total_tokens >= 0),
  PRIMARY KEY(provider_id, account_id),
  FOREIGN KEY(account_id) REFERENCES accounts(id) ON DELETE CASCADE
);

INSERT INTO provider_daily_usage_summary
  (provider_id, account_id, bucket_count, total_tokens)
SELECT provider_id, account_id, COUNT(*), SUM(tokens)
FROM provider_daily_usage
WHERE NOT EXISTS (SELECT 1 FROM provider_daily_usage_summary)
GROUP BY provider_id, account_id;

CREATE TRIGGER IF NOT EXISTS provider_daily_usage_summary_insert
AFTER INSERT ON provider_daily_usage
BEGIN
  INSERT INTO provider_daily_usage_summary
    (provider_id, account_id, bucket_count, total_tokens)
  VALUES (NEW.provider_id, NEW.account_id, 1, NEW.tokens)
  ON CONFLICT(provider_id, account_id) DO UPDATE SET
    bucket_count = bucket_count + 1,
    total_tokens = total_tokens + NEW.tokens;
END;

CREATE TRIGGER IF NOT EXISTS provider_daily_usage_summary_update
AFTER UPDATE OF tokens ON provider_daily_usage
WHEN OLD.tokens != NEW.tokens
BEGIN
  UPDATE provider_daily_usage_summary
  SET total_tokens = total_tokens - OLD.tokens + NEW.tokens
  WHERE provider_id = NEW.provider_id AND account_id = NEW.account_id;
END;

CREATE TRIGGER IF NOT EXISTS provider_daily_usage_summary_delete
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
