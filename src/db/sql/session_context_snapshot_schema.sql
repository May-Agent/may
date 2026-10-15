CREATE TABLE IF NOT EXISTS session_context_snapshots (session_id TEXT PRIMARY KEY, checkpoint_blob BLOB NOT NULL, last_message_db_id INTEGER, updated_at TEXT NOT NULL, format_version INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS idx_session_context_snapshots_updated_at ON session_context_snapshots(updated_at);
