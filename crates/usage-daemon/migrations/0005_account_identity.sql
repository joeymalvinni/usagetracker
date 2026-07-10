ALTER TABLE accounts ADD COLUMN display_name_source TEXT NOT NULL DEFAULT 'provider';
ALTER TABLE accounts ADD COLUMN email TEXT;
