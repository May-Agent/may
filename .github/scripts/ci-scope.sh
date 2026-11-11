#!/usr/bin/env bash
# Reads the changed paths on stdin, one per line, and prints the CI jobs they
# select as `job=true|false` lines. A path no rule names selects every Rust job,
# so a new kind of file is covered until a rule says what reads it.
#
#   fmt      rustfmt reads Rust sources, the manifests (edition) and its config.
#   clippy   reads everything Rust builds, and the web UI through the `ui` feature.
#   test     reads everything Rust builds and runs, fixtures included; the web UI
#            only through the `ui` feature, which the tests do not enable.
#   web_ui   reads web-ui/ and src/texts/artifact_runtime_bridge.js, which its
#            tests load.
#   acceptance  reads what the `verify-tools` and `container-acceptance` jobs
#            run through `may exec`: the tool distribution, the shell and IPC
#            bridge, the container sandbox and image, the agent that wires
#            them, the manifests, and the acceptance tests and their script.
#
# A change under .github/ (workflows, actions, this script) selects every job.
# The repository documents nothing builds or reads select none.
set -euo pipefail

fmt=false
clippy=false
test=false
web_ui=false
acceptance=false

while IFS= read -r path; do
  # Independent of the job rules below, which stop at their first match.
  case "${path}" in
    .github/* | Cargo.toml | Cargo.lock | build.rs | rust-toolchain | rust-toolchain.toml | \
      src/tools/* | src/shell/* | src/container/* | src/ipc/* | src/command_policy/* | \
      src/agent/* | src/bin/may/* | src/config.rs | src/terminal_*.rs | \
      container/* | templates/* | tasks/* | \
      tests/terminal_first_e2e.rs | tests/fixtures/*)
      acceptance=true
      ;;
  esac
  case "${path}" in
    AGENTS.md | CLAUDE.md | README.md) ;;
    .github/*)
      fmt=true clippy=true test=true web_ui=true
      ;;
    web-ui/*)
      clippy=true web_ui=true
      ;;
    src/texts/artifact_runtime_bridge.js)
      clippy=true test=true web_ui=true
      ;;
    *.rs | Cargo.toml | rustfmt.toml | .rustfmt.toml | rust-toolchain | rust-toolchain.toml)
      fmt=true clippy=true test=true
      ;;
    *)
      clippy=true test=true
      ;;
  esac
done

echo "fmt=${fmt}"
echo "clippy=${clippy}"
echo "test=${test}"
echo "web_ui=${web_ui}"
echo "acceptance=${acceptance}"
