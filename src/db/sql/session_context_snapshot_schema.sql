CREATE TABLE IF NOT EXISTS session_context_snapshots (session_id TEXT PRIMARY KEY, checkpoint_blob BLOB NOT NULL, history_message_count INTEGER NOT NULL, updated_at TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS idx_session_context_snapshots_updated_at ON session_context_snapshots(updated_at);
