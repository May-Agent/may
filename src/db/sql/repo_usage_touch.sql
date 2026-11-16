INSERT INTO repo_usage (worktree, last_used_at, use_count, owner_repo)
VALUES (?, ?, 1, ?)
ON CONFLICT(worktree) DO UPDATE SET
    last_used_at = MAX(last_used_at, excluded.last_used_at),
    use_count = use_count + 1,
    owner_repo = COALESCE(excluded.owner_repo, owner_repo)
