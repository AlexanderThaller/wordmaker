-- Unpublished changes to a page: set together, or not at all.
ALTER TABLE pages ADD COLUMN draft_title TEXT;
ALTER TABLE pages ADD COLUMN draft_body TEXT;
ALTER TABLE pages ADD COLUMN draft_updated_at TEXT;
