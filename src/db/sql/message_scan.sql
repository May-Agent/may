SELECT m.id AS id, m.session_id AS session_id, s.title AS title, m.role AS role,
    m.content AS content, m.timestamp AS timestamp
FROM messages m JOIN sessions s ON s.id = m.session_id
WHERE m.id < ?
AND (? IS NULL OR m.session_id = ?)
AND m.role IN (SELECT value FROM json_each(?))
AND (? IS NULL OR m.timestamp >= ?)
AND (? IS NULL OR m.timestamp <= ?)
AND (? IS NULL OR instr(lower(COALESCE(s.title, '')), lower(?)) > 0)
ORDER BY m.id DESC LIMIT ?
