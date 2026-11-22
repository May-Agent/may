INSERT INTO request_logs (session_id, timestamp, model, provider_id,
    prompt_tokens, completion_tokens, total_tokens, cache_read_tokens,
    cache_write_tokens, context_window, elapsed_ms, ok, error, keep_warm)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
