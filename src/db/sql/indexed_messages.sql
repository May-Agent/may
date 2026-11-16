SELECT m.id AS message_id, m.session_id, m.role, m.content, m.timestamp,
    s.title, s.model, s.profile_id, s.archived, s.preview, s.updated_at, s.message_count
FROM messages m JOIN sessions s ON s.id = m.session_id
WHERE m.id IN (SELECT value FROM json_each(?))
