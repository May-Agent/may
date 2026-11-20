# CI for a TypeScript project using bun

Save as `.github/workflows/ci.yml`. These jobs are the minimum and all exist in every TypeScript repository: `build`, `test`, `lint`, `format`, and the `result` job (check name `CI result`) that closes the workflow. Do not remove, merge or rename them, and do not add `continue-on-error`.

The project's `package.json` defines the scripts the jobs call: `build` (type-check and bundle, for example `tsc --noEmit && bun build ./src/index.ts --outdir dist`), `test` (`bun test`), `lint` (for example `biome lint .`), `format:check` (for example `biome format .`). A missing script is added to `package.json` in the first commit; the job is not changed to skip it.

Edit only the places marked `# PROJECT`: the platforms (one literal-named `build` and `test` pair per real target, named as in `ci-rust.md`, never a `matrix:`) and the `ts` path filter when sources live outside the listed globs.

```yaml
name: CI

on:
  pull_request:
    branches: [main]
  merge_group:
  push:
    branches: [main]

concurrency:
  group: ci-${{ github.ref }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

permissions:
  contents: read

jobs:
  changes:
    runs-on: ubuntu-latest
    timeout-minutes: 5
    permissions:
      contents: read
      pull-requests: read
    outputs:
      ts: ${{ steps.filter.outputs.ts }}
    steps:
      - uses: actions/checkout@v4
      - uses: dorny/paths-filter@v3
        id: filter
        with:
          base: ${{ github.event_name == 'merge_group' && github.event.merge_group.base_sha || '' }}
          ref: ${{ github.event_name == 'merge_group' && github.event.merge_group.head_sha || '' }}
          filters: |
            ts:
              - '**/*.ts'
              - '**/*.tsx'
              - '**/*.js'
              - '**/package.json'
              - 'bun.lock'
              - 'bunfig.toml'
              - 'tsconfig*.json'
              - 'biome.json*'
              - '.prettierrc*'
              - 'eslint.config.*'
              - '.github/workflows/ci.yml'

  format:
    needs: changes
    if: needs.changes.outputs.ts == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: oven-sh/setup-bun@v2
      - run: bun install --frozen-lockfile
      - run: bun run format:check

  lint:
    needs: changes
    if: needs.changes.outputs.ts == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: oven-sh/setup-bun@v2
      - run: bun install --frozen-lockfile
      - run: bun run lint

  # PROJECT: copy the build and test jobs per platform.
  build:
    needs: changes
    if: needs.changes.outputs.ts == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 15
    steps:
      - uses: actions/checkout@v4
      - uses: oven-sh/setup-bun@v2
      - run: bun install --frozen-lockfile
      - run: bun run build

  test:
    needs: changes
    if: needs.changes.outputs.ts == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 15
    steps:
      - uses: actions/checkout@v4
      - uses: oven-sh/setup-bun@v2
      - run: bun install --frozen-lockfile
      - run: bun test

  result:
    # The one required check on `main`: green when every job it needs
    # succeeded or was skipped by the path filter.
    name: CI result
    if: always()
    # PROJECT: list every check job above; per-platform build/test pairs
    # replace `build` and `test` with their literal names.
    needs:
      - format
      - lint
      - build
      - test
    runs-on: ubuntu-latest
    timeout-minutes: 5
    steps:
      - name: Check job results
        env:
          RESULTS: ${{ join(needs.*.result, ' ') }}
        run: |
          for result in $RESULTS; do
            if [ "$result" = failure ] || [ "$result" = cancelled ]; then
              echo "::error::a needed job ended $result"
              exit 1
            fi
          done
```

`CI result` runs under `if: always()`, so it reports even when a needed job failed or the path filter skipped every check job; a needed job that was skipped does not fail it, only `failure` and `cancelled` do. A pull request that touches no TypeScript file still reports a green `CI result` — the single context branch protection requires — so skipped jobs never stall the merge queue. Its `needs:` must list every check job in the workflow; a job missing from `needs:` is a check whose failure nothing waits on.
