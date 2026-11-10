# CI for a TypeScript project using bun

Save as `.github/workflows/ci.yml`. These jobs are the minimum and all exist in every TypeScript repository: `build`, `test`, `lint`, `format`. Do not remove, merge or rename them, and do not add `continue-on-error`.

The project's `package.json` defines the scripts the jobs call: `build` (type-check and bundle, for example `tsc --noEmit && bun build ./src/index.ts --outdir dist`), `test` (`bun test`), `lint` (for example `biome lint .`), `format:check` (for example `biome format .`). A missing script is added to `package.json` in the first commit; the job is not changed to skip it.

Edit only the places marked `# PROJECT`: the platforms (one literal-named `build` and `test` pair per real target, named as in `ci-rust.md`, never a `matrix:`) and the `ts` path filter when sources live outside the listed globs.

```yaml
name: CI

on:
  pull_request:
  merge_group:
  push:
    branches: [main] # PROJECT: the repository's default branch

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
```
