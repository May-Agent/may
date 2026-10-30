INSERT INTO search_outbox (collection, doc_id, message_id)
SELECT 'may-sessions', session_id || ':' || id, id FROM messages
WHERE role NOT IN ('hidden_context', 'permission_request', 'permission_response', 'ask_user_request', 'ask_user_response')
ORDER BY id
