CREATE INDEX IF NOT EXISTS raw_payloads_snapshot_id
ON raw_payloads(snapshot_id);

CREATE INDEX IF NOT EXISTS raw_payloads_provider_time
ON raw_payloads(provider_id, collected_at DESC);
