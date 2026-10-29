CREATE TABLE IF NOT EXISTS session_run_leases (session_id TEXT PRIMARY KEY, owner TEXT, generation INTEGER NOT NULL, heartbeat_at INTEGER NOT NULL, cancellation_requested BOOLEAN NOT NULL);
