-- Per-provider quota/availability watcher: probe config on providers, an
-- optional per-route fallback chain, and a runtime status table.
ALTER TABLE providers ADD COLUMN probe_script TEXT;
ALTER TABLE providers ADD COLUMN probe_enabled INTEGER;
ALTER TABLE providers ADD COLUMN probe_interval_secs INTEGER;
ALTER TABLE providers ADD COLUMN quota_min REAL;

ALTER TABLE routes ADD COLUMN fallback TEXT NOT NULL DEFAULT '[]';

CREATE TABLE IF NOT EXISTS provider_status (
    provider        TEXT PRIMARY KEY REFERENCES providers(name) ON DELETE CASCADE,
    available       INTEGER NOT NULL DEFAULT 0,
    quota_remaining REAL,
    quota_used      REAL,
    quota_limit     REAL,
    last_checked    INTEGER,
    last_ok         INTEGER NOT NULL DEFAULT 0,
    error           TEXT,
    note            TEXT
);
