# CI for a Rust project

Save as `.github/workflows/ci.yml`. These jobs are the minimum and all exist in every Rust repository: `build`, `test`, `lint`, `format`, `deny`, and the `result` job (check name `CI result`) that closes the workflow. Do not remove, merge or rename them, and do not add `continue-on-error`. The `deny` job reads `deny.toml` at the repository root; its starter is below the workflow, and its `allow` and `allow-git` lists grow as the project's dependencies need them, never shrink to pass a check.

Edit only the places marked `# PROJECT`:

- Platforms: one `build` and one `test` job per platform the project really supports. A project with one platform keeps the plain names `build` and `test`. With several, copy the pair per platform, name them `build (linux)`, `test (linux)`, `build (macos)`, `test (macos)`, `build (windows)`, `test (windows)` and set `runs-on` to `ubuntu-latest`, `macos-latest`, `windows-latest`. Never a `matrix:`, so `result`'s `needs:` lists each check job by one literal name. Add Windows only when the project claims Windows support.
- The `rust` path filter: the globs a Rust change touches. A workspace adds member directories only when they are outside the globs below.

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

env:
  CARGO_TERM_COLOR: always

jobs:
  changes:
    runs-on: ubuntu-latest
    timeout-minutes: 5
    permissions:
      contents: read
      pull-requests: read
    outputs:
      rust: ${{ steps.filter.outputs.rust }}
    steps:
      - uses: actions/checkout@v4
      - uses: dorny/paths-filter@v3
        id: filter
        with:
          base: ${{ github.event_name == 'merge_group' && github.event.merge_group.base_sha || '' }}
          ref: ${{ github.event_name == 'merge_group' && github.event.merge_group.head_sha || '' }}
          filters: |
            rust:
              - '**/*.rs'
              - '**/Cargo.toml'
              - 'Cargo.lock'
              - 'deny.toml'
              - 'rust-toolchain*'
              - 'rustfmt.toml'
              - '.rustfmt.toml'
              - 'clippy.toml'
              - '.cargo/**'
              - '.github/workflows/ci.yml'

  format:
    needs: changes
    if: needs.changes.outputs.rust == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --all --check

  lint:
    needs: changes
    if: needs.changes.outputs.rust == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy --workspace --all-targets --all-features -- -D warnings

  deny:
    needs: changes
    if: needs.changes.outputs.rust == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 10
    steps:
      - uses: actions/checkout@v4
      - uses: EmbarkStudios/cargo-deny-action@v2

  # PROJECT: copy the build and test jobs per platform (see above).
  build:
    needs: changes
    if: needs.changes.outputs.rust == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 20
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --workspace --all-targets --all-features

  test:
    needs: changes
    if: needs.changes.outputs.rust == 'true'
    runs-on: ubuntu-latest
    timeout-minutes: 30
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --workspace --all-features

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
      - deny
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

`CI result` runs under `if: always()`, so it reports even when a needed job failed or the path filter skipped every check job; a needed job that was skipped does not fail it, only `failure` and `cancelled` do. A pull request that touches no Rust file still reports a green `CI result` — the single context branch protection requires — so skipped jobs never stall the merge queue. Its `needs:` must list every check job in the workflow; a job missing from `needs:` is a check whose failure nothing waits on.

Save as `deny.toml`, next to `Cargo.toml`. The `allow` list is the fixed starting set; a dependency under another license earns an `allow` entry and a git dependency an `allow-git` prefix — the project amends the file, never the job.

```toml
[licenses]
version = 2
allow = [
  "0BSD",
  "Apache-2.0",
  "Apache-2.0 WITH LLVM-exception",
  "BSD-2-Clause",
  "BSD-3-Clause",
  "BSL-1.0",
  "CC0-1.0",
  "ISC",
  "MIT",
  "Unicode-3.0",
  "Unicode-DFS-2016",
  "Zlib",
]

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-git = []
```

The action runs `cargo deny --all-features check`, which covers advisories, bans, licenses and sources; `advisories` and `bans` keep their defaults (deny a matching advisory or yanked release, warn on multiple versions and wildcards).

## Releases

A Rust repository's first commit also carries the release-plz setup. The default is git-only releases — tags and a release pull request, never crates.io: `cargo publish` is irreversible, so anything that reaches crates.io is configured only when the user explicitly asks for it.

- `Cargo.toml`: `publish = false` in `[package]`, unless the user explicitly asked to publish to crates.io.
- `release-plz.toml` at the repository root:

```toml
[workspace]
git_only = true
git_tag_name = "v{{ version }}"
```

  For a workspace whose member crates release independently, give each released crate a `[[package]]` entry instead — `name = "<crate>"`, `git_only = true`, `git_tag_name = "<dir>/v{{ version }}"` where `<dir>` is the crate's directory (for example `parse/v0.2.0` tags a `parse` member), so every member's tags stay distinct.
- `.github/workflows/release-plz.yml`, which opens or refreshes the release pull request on every push to `main`:

```yaml
name: Release PR

on:
  push:
    branches: [main]

permissions:
  contents: write
  pull-requests: write

jobs:
  release-plz:
    name: Release
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      - uses: MarcoIeni/release-plz-action@v0.5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
```

  No `CARGO_REGISTRY_TOKEN`, no publish step: `git_only = true` reads the current version from git tags and skips `cargo publish` entirely. crates.io publishing (trusted publishing or a stored token) is wired only on the user's explicit request.
- The workflow opens pull requests with `GITHUB_TOKEN`, which needs the repository to allow Actions-created pull requests:

```bash
gh api -X PUT repos/OWNER/NAME/actions/permissions/workflow -f default_workflow_permissions=read -F can_approve_pull_request_reviews=true
```

A release is cut by merging release-plz's release PR into `main`, and only when the user instructs it.
