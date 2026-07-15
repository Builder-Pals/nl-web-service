CREATE TABLE IF NOT EXISTS game_workflows (
    source_place_id INTEGER PRIMARY KEY NOT NULL,
    source_revision TEXT NOT NULL,
    source_name TEXT NOT NULL,
    sandboxed_asset_id INTEGER,
    operation_id TEXT,
    state TEXT NOT NULL CHECK(state IN ('uploading','moderating','approved','failed')),
    failure_code TEXT,
    failure_message TEXT,
    validated_at INTEGER NOT NULL,
    attempted_at INTEGER NOT NULL,
    completed_at INTEGER
);
