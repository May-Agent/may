INSERT INTO cache_misses (session_id, timestamp, model, provider_id, cause,
    missed_tokens, read_tokens, gap_ms, diverged_kind, diverged_index, compacted,
    reassembled, agent_rebuilt, model_switched)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
