-- Support inline probe script text as an alternative to a file path.
-- probe_source: 'path' (use probe_script file) or 'text' (use probe_script_text inline).
ALTER TABLE providers ADD COLUMN probe_script_text TEXT;
ALTER TABLE providers ADD COLUMN probe_source TEXT NOT NULL DEFAULT 'path';
