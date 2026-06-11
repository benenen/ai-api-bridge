-- Key-value settings editable from the admin page (e.g. fallback_route).
-- Seeded from bridge.toml on first run; a present key is then authoritative
-- (an absent key falls back to the config file value).
CREATE TABLE IF NOT EXISTS settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
