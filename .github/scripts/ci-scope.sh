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
#
# A change under .github/ (workflows, actions, this script) selects every job.
# The repository documents nothing builds or reads select none.
set -euo pipefail

fmt=false
clippy=false
test=false
web_ui=false

while IFS= read -r path; do
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
