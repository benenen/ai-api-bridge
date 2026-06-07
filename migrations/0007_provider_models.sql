-- Provider-supported model list (used by admin UI for upstream model autocomplete).
ALTER TABLE providers ADD COLUMN models TEXT NOT NULL DEFAULT '[]';
