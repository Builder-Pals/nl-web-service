ALTER TABLE game_workflows ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'roblox';
ALTER TABLE game_workflows ADD COLUMN archive_record_id TEXT;
ALTER TABLE game_workflows ADD COLUMN archive_sha256 TEXT;
ALTER TABLE game_workflows ADD COLUMN archive_path TEXT;
ALTER TABLE game_workflows ADD COLUMN archive_size INTEGER;

CREATE TABLE archive_catalog_cache (
    id INTEGER PRIMARY KEY NOT NULL CHECK(id = 1),
    body BLOB NOT NULL,
    etag TEXT,
    updated_at INTEGER NOT NULL
);
