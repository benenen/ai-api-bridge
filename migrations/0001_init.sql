-- Providers and routes are stored in SQLite; bridge.toml seeds them on first run.
CREATE TABLE IF NOT EXISTS providers (
    name             TEXT PRIMARY KEY,
    wire             TEXT NOT NULL,
    base_url         TEXT NOT NULL,
    api_key          TEXT,
    model_prefix     TEXT,
    max_tokens_field TEXT NOT NULL DEFAULT 'max_tokens',
    extra_headers    TEXT NOT NULL DEFAULT '{}'
);

CREATE TABLE IF NOT EXISTS routes (
    alias    TEXT PRIMARY KEY,
    provider TEXT NOT NULL REFERENCES providers(name) ON DELETE CASCADE,
    model    TEXT NOT NULL
);
