CREATE TRIGGER IF NOT EXISTS session_intervals_ask_user_cleared AFTER DELETE ON session_pending_ask_user_requests_v2
WHEN OLD.blocking = 1
BEGIN
  SELECT RAISE(ABORT, 'session_pending_ask_user_requests_v2.created_at is not a timestamp')
  WHERE julianday(OLD.created_at) IS NULL;
  INSERT INTO session_intervals (session_id, kind, start_ms, end_ms)
  SELECT OLD.session_id, 'user_wait', w.start_ms, MAX(w.start_ms, w.end_ms)
  FROM (SELECT CAST(ROUND((julianday(OLD.created_at) - 2440587.5) * 86400000.0) AS INTEGER) AS start_ms,
               CAST(ROUND((julianday('now') - 2440587.5) * 86400000.0) AS INTEGER) AS end_ms) AS w;
END

CREATE TRIGGER IF NOT EXISTS session_intervals_permission_cleared AFTER DELETE ON session_pending_permissions
BEGIN
  SELECT RAISE(ABORT, 'session_pending_permissions.created_at is not a timestamp')
  WHERE julianday(OLD.created_at) IS NULL;
  INSERT INTO session_intervals (session_id, kind, start_ms, end_ms)
  SELECT OLD.session_id, 'user_wait', w.start_ms, MAX(w.start_ms, w.end_ms)
  FROM (SELECT CAST(ROUND((julianday(OLD.created_at) - 2440587.5) * 86400000.0) AS INTEGER) AS start_ms,
               CAST(ROUND((julianday('now') - 2440587.5) * 86400000.0) AS INTEGER) AS end_ms) AS w;
END

CREATE TRIGGER IF NOT EXISTS session_intervals_workspace_cleared AFTER DELETE ON session_pending_workspace
BEGIN
  SELECT RAISE(ABORT, 'session_pending_workspace.created_at is not a timestamp')
  WHERE julianday(OLD.created_at) IS NULL;
  INSERT INTO session_intervals (session_id, kind, start_ms, end_ms)
  SELECT OLD.session_id, 'user_wait', w.start_ms, MAX(w.start_ms, w.end_ms)
  FROM (SELECT CAST(ROUND((julianday(OLD.created_at) - 2440587.5) * 86400000.0) AS INTEGER) AS start_ms,
               CAST(ROUND((julianday('now') - 2440587.5) * 86400000.0) AS INTEGER) AS end_ms) AS w;
END
