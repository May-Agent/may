INSERT INTO prompt_cache_routes (session_id, provider_id, model, observed_at_ms,
    cache_read_tokens, cache_write_tokens, prompt_tokens, chain, cache_block_tokens)
VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
ON CONFLICT (session_id, provider_id, model) DO UPDATE SET
    observed_at_ms = excluded.observed_at_ms,
    cache_read_tokens = excluded.cache_read_tokens,
    cache_write_tokens = excluded.cache_write_tokens,
    prompt_tokens = excluded.prompt_tokens, chain = excluded.chain,
    cache_block_tokens = excluded.cache_block_tokens
