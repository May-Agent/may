CREATE TABLE IF NOT EXISTS search_outbox (id INTEGER PRIMARY KEY AUTOINCREMENT, collection TEXT NOT NULL, doc_id TEXT NOT NULL, message_id INTEGER NOT NULL)

CREATE INDEX IF NOT EXISTS idx_search_outbox_doc ON search_outbox(collection, doc_id)

CREATE TRIGGER IF NOT EXISTS search_outbox_message_insert AFTER INSERT ON messages
WHEN NEW.role NOT IN ('hidden_context', 'permission_request', 'permission_response', 'ask_user_request', 'ask_user_response')
BEGIN
  INSERT INTO search_outbox (collection, doc_id, message_id)
  VALUES ('may-sessions', NEW.session_id || ':' || NEW.id, NEW.id);
END

CREATE TRIGGER IF NOT EXISTS search_outbox_message_delete AFTER DELETE ON messages
WHEN OLD.role NOT IN ('hidden_context', 'permission_request', 'permission_response', 'ask_user_request', 'ask_user_response')
BEGIN
  INSERT INTO search_outbox (collection, doc_id, message_id)
  VALUES ('may-sessions', OLD.session_id || ':' || OLD.id, OLD.id);
END
