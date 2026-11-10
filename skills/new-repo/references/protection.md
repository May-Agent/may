# Branches and protection

## Branches

After the first push of `dev`:

```bash
git push origin dev:main
gh api -X PATCH repos/OWNER/NAME -f default_branch=dev
gh api repos/OWNER/NAME --jq .default_branch   # must print: dev
```

## Ruleset

One ruleset covers both branches. Write the payload to `ruleset.json` (outside the repository checkout) and post it:

```json
{
  "name": "protect-dev-main",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": { "include": ["refs/heads/dev", "refs/heads/main"], "exclude": [] }
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
        "check_response_timeout_minutes": 60,
        "grouping_strategy": "ALLGREEN",
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

Merge queue is available on organization-owned repositories and on user-owned repositories of plans that include it. If the POST answers 422 naming `merge_queue`, report that to the user and stop; do not drop the rule and continue.

## Read back

```bash
id=$(gh api repos/OWNER/NAME/rulesets --jq '.[] | select(.name=="protect-dev-main") | .id')
gh api repos/OWNER/NAME/rulesets/$id --jq '{enforcement, include: .conditions.ref_name.include, bypass: .bypass_actors, rules: [.rules[].type]}'
```

Expected: `enforcement` is `active`, `include` is `["refs/heads/dev","refs/heads/main"]`, `bypass` is `[]`, and `rules` contains exactly `deletion`, `non_fast_forward`, `required_signatures`, `pull_request`, `merge_queue` (any order). Anything else is a mismatch.

## Required checks

After the first green CI run on `dev`, add a `required_status_checks` rule to the same ruleset: fetch it, append this rule to `rules` with the `name` of each CI job (exactly as GitHub shows it), and send it back with `gh api -X PUT repos/OWNER/NAME/rulesets/$id --input ruleset.json`.

```json
{
  "type": "required_status_checks",
  "parameters": {
    "strict_required_status_checks_policy": false,
    "required_status_checks": [
      { "context": "build" },
      { "context": "test" },
      { "context": "lint" },
      { "context": "format" },
      { "context": "deny" }
    ]
  }
}
```

The contexts are the literal name of every job in the generated workflow — a Rust repository's set is `build`, `test`, `lint`, `format`, `deny`, with the per-platform pairs replacing `build`/`test` where the project copied them; a TypeScript repository's set is the same minus `deny`.

Required job names must be literal: a job with a `matrix:` that is skipped by its `if` reports an unexpanded name and stays pending forever. The CI templates therefore name each job literally and use no matrix. Run the read-back again afterwards; `rules` now also lists `required_status_checks`.
