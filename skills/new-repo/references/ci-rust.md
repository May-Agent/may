# CI for a Rust project

Save as `.github/workflows/ci.yml`. These jobs are the minimum and all exist in every Rust repository: `build`, `test`, `lint`, `format`, `deny`. Do not remove, merge or rename them, and do not add `continue-on-error`. The `deny` job reads `deny.toml` at the repository root; its starter is below the workflow, and its `allow` and `allow-git` lists grow as the project's dependencies need them, never shrink to pass a check.

Edit only the places marked `# PROJECT`:

- Platforms: one `build` and one `test` job per platform the project really supports. A project with one platform keeps the plain names `build` and `test`. With several, copy the pair per platform, name them `build (linux)`, `test (linux)`, `build (macos)`, `test (macos)`, `build (windows)`, `test (windows)` and set `runs-on` to `ubuntu-latest`, `macos-latest`, `windows-latest`. Never a `matrix:`, so required-check names stay literal. Add Windows only when the project claims Windows support.
- The `rust` path filter: the globs a Rust change touches. A workspace adds member directories only when they are outside the globs below.

```yaml
name: CI

on:
  pull_request:
    branches: [dev, main]
  merge_group:
  push:
    branches: [dev]

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
```

A job skipped by its `if` reports success under its own name, so a pull request that touches no Rust file still passes the required checks.

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
