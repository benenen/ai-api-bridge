-- Cost accumulator: per-provider rolling cost windows + a per-request cost event log.
ALTER TABLE providers ADD COLUMN cost_windows TEXT NOT NULL DEFAULT '[]';

CREATE TABLE IF NOT EXISTS usage_events (
    provider TEXT    NOT NULL,
    ts       INTEGER NOT NULL,  -- unix seconds
    cost     REAL    NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_events_provider_ts ON usage_events(provider, ts);
