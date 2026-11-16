SELECT worktree, last_used_at, use_count, owner_repo FROM repo_usage
WHERE last_used_at >= ?
