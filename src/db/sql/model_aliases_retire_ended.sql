UPDATE model_aliases SET live = 0 WHERE live = 1 AND kind = 'agent' AND target NOT IN (SELECT id FROM sessions WHERE archived = 0) RETURNING kind, target;
UPDATE model_aliases SET live = 0 WHERE live = 1 AND kind = 'question' AND target NOT IN (SELECT request_id FROM session_pending_ask_user_requests_v2) RETURNING kind, target;
UPDATE model_aliases SET live = 0 WHERE live = 1 AND kind = 'task' AND target NOT IN (SELECT id FROM scheduled_tasks) RETURNING kind, target;
