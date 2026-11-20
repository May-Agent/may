# Branches and protection

## Branches

After the first push of `main`, make it the default branch:

```bash
gh api -X PATCH repos/OWNER/NAME -f default_branch=main
gh api repos/OWNER/NAME --jq .default_branch   # must print: main
```

## Ruleset

One ruleset covers `main`. Write the payload to `ruleset.json` (outside the repository checkout) and post it:

```json
{
  "name": "protect-main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": { "include": ["refs/heads/main"], "exclude": [] }
  },
  "bypass_actors": [],
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "required_signatures" },
    {
      "type": "pull_request",
      "parameters": {
        "required_approving_review_count": 0,
        "dismiss_stale_reviews_on_push": false,
        "require_code_owner_review": false,
        "require_last_push_approval": false,
        "required_review_thread_resolution": false
      }
    },
    {
      "type": "merge_queue",
      "parameters": {
        "merge_method": "MERGE",
        "grouping_strategy": "ALLGREEN",
        "check_response_timeout_minutes": 60,
        "max_entries_to_build": 5,
        "max_entries_to_merge": 5,
        "min_entries_to_merge": 1,
        "min_entries_to_merge_wait_minutes": 5
      }
    }
  ]
}
```

```bash
gh api -X POST repos/OWNER/NAME/rulesets --input ruleset.json
```

`required_approving_review_count` is 0 because a single maintainer cannot approve their own pull request; raise it only when the user asks.

Merge queue needs a plan that includes it — organization repositories have it, while personal user-owned repositories and lower plans may not. If the POST answers HTTP 422 with an error naming `merge_queue`, do not stop and do not leave `main` unprotected: remove the `merge_queue` block from `rules` in `ruleset.json` and POST the rest again unchanged, then continue with the read-back and the required checks below as usual. Tell the user `merge_queue` was omitted because the repository's owner or plan does not include it.

## Read back

```bash
id=$(gh api repos/OWNER/NAME/rulesets --jq '.[] | select(.name=="protect-main") | .id')
gh api repos/OWNER/NAME/rulesets/$id --jq '{enforcement, include: .conditions.ref_name.include, bypass: .bypass_actors, rules: [.rules[].type]}'
```

Expected: `enforcement` is `active`, `include` is `["refs/heads/main"]`, `bypass` is `[]`, and `rules` contains `deletion`, `non_fast_forward`, `required_signatures`, `pull_request` and `merge_queue` (any order) — `merge_queue` absent when the fallback above ran. Anything else is a mismatch.

## Required checks

After the first green CI run on `main`, add a `required_status_checks` rule to the same ruleset: fetch it, append this rule to `rules`, and send it back with `gh api -X PUT repos/OWNER/NAME/rulesets/$id --input ruleset.json`.

```json
{
  "type": "required_status_checks",
  "parameters": {
    "strict_required_status_checks_policy": false,
    "required_status_checks": [
      { "context": "CI result" }
    ]
  }
}
```

`CI result` is the only required context — never the path-filtered job names. It is the always-running job every CI template ends with: `if: always()` keeps it running when a needed job fails or the path filter skipped every check job, and it fails only when a needed job ended `failure` or `cancelled`. A pull request that touches no source file still reports a green `CI result`, so required checks and the merge queue never stall on a skipped job.

`CI result`'s `needs:` must list every check job the workflow has: a job missing from `needs:` is a check whose failure nothing waits on, so a job added to the workflow is added to `needs:` in the same change. That is why the templates keep one literally named job per check and no `matrix:` — `needs:` then enumerates every required check plainly.

Run the read-back again afterwards; `rules` now also lists `required_status_checks`.
