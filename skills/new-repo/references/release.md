# Release for a Rust project

Save as `.github/workflows/release.yml`. Edit only the `# PROJECT` comment.

```yaml
name: Release

on:
  push:
    branches: [main] # PROJECT: the repository's default branch

permissions:
  contents: write
  pull-requests: write
  id-token: write

jobs:
  release-plz:
    name: Release
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
        with:
          fetch-depth: 0
      - uses: dtolnay/rust-toolchain@stable
      # Mint a short-lived crates.io token from the GitHub OIDC identity
      # instead of a stored CARGO_REGISTRY_TOKEN. Trusted publishing is
      # configured per crate on crates.io; a crate that has never been
      # published cannot be configured, so the step stays non-blocking
      # until the first `cargo publish` bootstrap lands it.
      - uses: rust-lang/crates-io-auth-action@v1
        id: crates-io-auth
        continue-on-error: true
      - uses: MarcoIeni/release-plz-action@v0.5
        env:
          GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}
          CARGO_REGISTRY_TOKEN: ${{ steps.crates-io-auth.outputs.token }}
```

Save as `release-plz.toml`, next to `Cargo.toml`:

```toml
[workspace]
publish = false
```

`publish = false` keeps a release to a git tag and a GitHub release. Publishing to crates.io needs a one-time bootstrap, because trusted publishing can only be configured for a crate that already exists:

1. `cargo publish` the crate once by hand, with a classic crates.io API token — a placeholder version is enough; the crate only has to exist.
2. On crates.io, under the crate's Trusted Publishing settings, allow this repository's `release.yml` workflow.
3. Flip `publish` to `true` in `release-plz.toml`. The workflow already mints the token; nothing else changes.

Every push to the default branch runs the action, which maintains one open release PR carrying the version bump and a changelog generated from conventional commits. Merging that PR — through the merge queue like any other — tags the version and cuts the GitHub release, and publishes the crate when `publish` is on.
