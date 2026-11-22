#!/usr/bin/env python3
"""Compute what May's CI gate must compile and run for a diff.

Takes the `lexoliu/affected-crates` report of the merge-base diff (produced
with `.github/affected-rules.toml`; it supplies `merge_base`, `workspace`,
`affected`, `members` and `owners`) from `--report-file` or the
`AFFECTED_REPORT` environment variable and writes the `GITHUB_OUTPUT` keys
the `scope` job forwards:

- `rust` — `true` when any workspace package is in scope. `fmt`, `clippy`
  and `test` carry the same value: prose-only, web-ui-only and
  outside-workspace diffs compile nothing.
- `packages` — space-separated affected package names, the literal
  `workspace`, or empty.
- `package-args` — `packages` as cargo flags: `-p <name>` per package,
  `--workspace` for `workspace`, empty for empty.
- `linux-package-args` — `package-args` minus the members the Linux legs
  never compile (`LINUX_EXCLUDED`; none today).
- `cargo-flags` / `linux-cargo-flags` — the target selection the test jobs
  build and run: the affected packages as `-p` flags plus `--all-targets`
  when the e2e gates run, `--lib --bins` when they do not, so a
  client-only diff never compiles `tests/terminal_first_e2e.rs`. When the
  e2e gates run, the root package — owner of the `may` binary and the
  `tests/` harness — joins the set so the e2e and `may` binaries exist to
  upload; `--workspace` for a workspace scope.
- `ui-clippy` — `true` when the `--features ui` clippy leg has work: a
  workspace scope, a `web-ui/` change, or an in-scope member carrying the
  `ui` feature that embeds the frontend bundle.
- `ui-args` — the packages carrying `ui` as `-p` flags, so the leg checks
  only what the feature can enable on.
- `e2e` — `true` when the diff can reach the daemon, agent runtime,
  sandbox, tools or CLI entrypoints: a workspace scope, or a changed file
  whose direct owner is not a client-only leaf (`CLIENT_ONLY_PACKAGES`,
  e.g. `may-tui` once extracted — its reverse-dependency edge still lands
  the thin `may` binary in `affected`, which is why `e2e` reads the
  files' direct owners and not the closure). `acceptance` carries the
  same value for the container jobs.
- `web_ui` — `true` when `web-ui/` changed (or the runtime bridge its
  tests load), or the scope is the whole workspace.

The determinator's rules can name only crates that exist today, so for
the asset directories embedded into a crate's build (`subagents/`,
`skills/`, `templates/`, `container/`) the affected set grows by every
workspace member compiling them in (`EMBEDDED_OWNERS`) plus that member's
workspace dependents — the split crates' suites run for their assets
before the rules file can spell their names.

Any uncertainty — an unreadable report, a failed `git` or `cargo` probe —
widens the gate to `workspace`; it never narrows on a guess.

Usage:

    affected.py --base <rev> --report-file <report.json>

An empty `--base` means no comparison base exists (the first push to a
branch), which selects the whole workspace.
"""

import argparse
import fnmatch
import json
import os
from pathlib import Path
import re
import subprocess
import sys

# Members whose changes can never reach the daemon, agent runtime, sandbox,
# tools or the CLI's non-TUI entrypoints. The thin `may` binary links them,
# so the determinator's reverse-dependency closure still lands the root
# package in `affected` for a pure client diff — `e2e` must therefore key on
# the changed files' direct owners (the report's `owners` map), not on the
# affected set.
CLIENT_ONLY_PACKAGES = frozenset({"may-tui"})

# Fallbacks for the `cargo metadata` probe: the workspace's root package —
# it owns the `may` binary, `tests/` and `build.rs` — and the members that
# carry the `ui` feature embedding the frontend bundle. Names not in the
# workspace are dropped by the `members` intersection, so future crates can
# be listed before they land.
ROOT_PACKAGE = "may-agent"
UI_FEATURE_PACKAGES = frozenset({"may-agent", "may-daemon"})

# Members the Linux legs never compile (none today: every member builds on
# Linux). An Apple-only member would join this set so the Ubuntu `-p` lists
# drop it — `-p` cannot be combined with `--exclude`.
LINUX_EXCLUDED = frozenset()

# The frontend the `ui` feature embeds, and the in-package bridge file the
# web-ui tests load outside that tree (both pre- and post-daemon extraction).
WEB_UI_PREFIX = "web-ui/"
WEB_UI_EXTRA = frozenset(
    {
        "src/texts/artifact_runtime_bridge.js",
        "daemon/src/texts/artifact_runtime_bridge.js",
    }
)

# `may-hashline` is a separate workspace; nothing in this graph reaches it.
HASHLINE_PREFIX = "hashline/"

# Directories compiled into a crate's build from outside every package
# directory: `include_dir!` embeds subagents/ and skills/, askama and
# `include_str!` read templates/, and build.rs hashes container/ into the
# baked image references. A path-rule can name only members that already
# exist (determinator resolves mark-changed against the workspace and
# errors on an unknown name), so the rules file marks just the root
# package and each prefix lists every other crate that embeds it — today
# and once the workspace splits. `compute` unions in `root_pkg`, keeps
# the candidates that exist, and adds their workspace dependents: a crate
# built on an embedder observes the assets through it.
EMBEDDED_OWNERS = {
    # include_dir!("../subagents") in may-config (config/src/subagent_file.rs).
    "subagents/": frozenset({"may-config"}),
    # include_dir!("../skills") in may-client (client/src/bundle/mod.rs).
    "skills/": frozenset({"may-client", "may-config"}),
    # askama/include_str! reach root templates/ from the TUI, the harness,
    # the agent core and the daemon.
    "templates/": frozenset(
        {"may-tui", "may-harness", "may-agent-core", "may-daemon"}
    ),
    # build.rs hashes the container/ contexts; may-container-image's tests
    # read them, and may-daemon owns the build once it moves.
    "container/": frozenset({"may-container-image", "may-daemon"}),
}
EMBEDDED_PREFIXES = tuple(EMBEDDED_OWNERS)

# Metadata file names the determinator's default rules consume before a
# package sees them; none can change what compiles.
METADATA_FILES = frozenset(
    {".gitignore", ".gitattributes", ".dockerignore", ".hgignore", ".svnignore", ".ignore"}
)

# Basename patterns for prose no compile can read — the same file kinds the
# rules and the determinator defaults map to nothing. Embedded directories
# are exempted by prefix first: their `.md` files are `include_dir!`'d in.
PROSE_NAMES = ("*.md", "README*", "LICENSE*", "CONTRIBUTING*", "CODE_OF_CONDUCT*", "SECURITY*")

# A release-plz bump touches only these, and only their `version =` lines;
# the bump was already green where it was authored.
RELEASE_ONLY_PATHS = frozenset({"CHANGELOG.md", "Cargo.toml", "Cargo.lock"})


def is_prose(path):
    """Whether `path` is documentation or repository prose no compile can
    read. Top-level prose, a crate's own README/LICENSE/markdown, the docs
    tree and issue templates qualify; the embedded asset directories do
    not — their files are compiled into the root package regardless of
    extension."""
    if path.startswith(EMBEDDED_PREFIXES):
        return False
    name = path.rsplit("/", 1)[-1]
    if any(fnmatch.fnmatch(name, pattern) for pattern in PROSE_NAMES):
        return True
    return path.startswith("docs/") or path.startswith(".github/ISSUE_TEMPLATE/")


def reaches_runtime(path, owner):
    """Whether a changed path can reach the daemon, agent runtime, sandbox,
    tools or CLI entrypoints — read from the file's direct owner, or, for a
    path a path-rule consumed (`owner` is None), from the mapping the rules
    file encodes. Ownerless runtime paths are the embedded asset
    directories and Cargo.lock; ownerless non-runtime paths are prose,
    web-ui sources, the outside-workspace crate and metadata files."""
    if owner is not None:
        return owner not in CLIENT_ONLY_PACKAGES and not is_prose(path)
    if path.startswith(EMBEDDED_PREFIXES):
        return True
    if path.startswith((WEB_UI_PREFIX, HASHLINE_PREFIX)):
        return False
    if is_prose(path):
        return False
    name = path.rsplit("/", 1)[-1]
    if name in METADATA_FILES:
        return False
    # Anything else a rule consumed is build machinery the gate runs:
    # Cargo.lock (its bump can move the pinned sandbox and image tags),
    # or a default-consumed file a narrower reading missed.
    return True


def release_only_bump(paths, diff_lines):
    """Whether the diff is a pure version bump: every changed path is one a
    release commit touches and every `+`/`-` line in the Cargo manifests
    edits `version =`. The release-plz bump was already green where it was
    authored, so the gate runs nothing for it."""
    if not paths or not set(paths) <= RELEASE_ONLY_PATHS:
        return False
    interesting = [
        line
        for line in diff_lines
        if line[:1] in {"+", "-"} and not line.startswith(("+++", "---"))
    ]
    return all(re.search(r"version *=", line) for line in interesting)


def package_args(names):
    """`names` rendered as cargo `-p` flags, sorted for stable diffs."""
    return " ".join(f"-p {name}" for name in sorted(names))


def reverse_closure(roots, dependents):
    """`roots` plus every member that transitively depends on one.
    `dependents` maps a member name to the members directly depending on
    it; dev-dependency cycles terminate through `seen`."""
    seen = set(roots)
    stack = list(seen)
    while stack:
        for dependent in dependents.get(stack.pop(), ()):
            if dependent not in seen:
                seen.add(dependent)
                stack.append(dependent)
    return seen


def workspace_outputs(ui_args, linux_excluded=LINUX_EXCLUDED):
    """Every gate on: the diff can reach anywhere."""
    # `--exclude` composes with `--workspace` where `-p` cannot.
    excludes = " ".join(f"--exclude {name}" for name in sorted(linux_excluded))
    linux_scope = f"--workspace {excludes}".rstrip()
    return {
        "rust": "true",
        "fmt": "true",
        "clippy": "true",
        "ui-clippy": "true",
        "test": "true",
        "e2e": "true",
        "acceptance": "true",
        "web_ui": "true",
        "packages": "workspace",
        "package-args": "--workspace",
        "linux-package-args": linux_scope,
        "cargo-flags": "--workspace --all-targets",
        "linux-cargo-flags": f"{linux_scope} --all-targets",
        "ui-args": ui_args,
    }


def empty_outputs(ui_args):
    """Nothing compiles: a prose-only, web-ui-agnostic or zero diff."""
    return {
        "rust": "false",
        "fmt": "false",
        "clippy": "false",
        "ui-clippy": "false",
        "test": "false",
        "e2e": "false",
        "acceptance": "false",
        "web_ui": "false",
        "packages": "",
        "package-args": "",
        "linux-package-args": "",
        "cargo-flags": "--lib --bins",
        "linux-cargo-flags": "--lib --bins",
        "ui-args": ui_args,
    }


def compute(report, *, root_pkg, ui_crates, rev_deps=None, linux_excluded=LINUX_EXCLUDED):
    """The output map for a parsed report. `ui_crates` are the members that
    declare a `ui` feature; `rev_deps` maps each member to the members that
    directly depend on it; `linux_excluded` the members no Linux leg
    compiles."""
    members = set(report["members"])
    owners = report["owners"]
    paths = list(owners)
    ui_args = package_args(set(ui_crates) & members)

    if report["workspace"]:
        return workspace_outputs(ui_args, linux_excluded)

    affected = set(report["affected"])
    # The rules file marks the root package for the embedded directories —
    # it can spell no name that does not exist yet — so every other member
    # compiling an asset in joins here, with its dependents: a crate built
    # on an embedder observes the assets through it.
    dependents = rev_deps or {}
    for path in paths:
        for prefix, embedders in EMBEDDED_OWNERS.items():
            if path.startswith(prefix):
                affected |= reverse_closure(
                    (embedders | {root_pkg}) & members, dependents
                )
    affected = sorted(affected)
    if not paths:
        return empty_outputs(ui_args)

    # web-ui changes reach the frontend job and the `--features ui` leg
    # without adding a Rust package to scope; the bridge file is the one
    # frontend-adjacent source living inside a package.
    web_ui_changed = any(path.startswith(WEB_UI_PREFIX) for path in paths)
    web_ui = web_ui_changed or any(path in WEB_UI_EXTRA for path in paths)

    # e2e keys on the changed files' direct owners: a client-only leaf
    # (e.g. may-tui) pulls the root package into `affected` through the
    # binary's dependency edge, but its diff still cannot reach the daemon.
    # It also needs a Rust crate in scope — the e2e and `may` binaries the
    # container job downloads are built by the test jobs, so `e2e` can never
    # be on while `rust` is off.
    e2e = bool(affected) and any(reaches_runtime(path, owners[path]) for path in paths)

    ui_clippy = web_ui_changed or bool(set(affected) & set(ui_crates))

    # When the e2e gates run, the root package must be in the build set —
    # it owns the `may` binary and the `tests/` harness the artifacts pack.
    test_names = set(affected)
    if e2e and root_pkg in members:
        test_names.add(root_pkg)
    target_kinds = "--all-targets" if e2e else "--lib --bins"
    cargo_flags = f"{package_args(test_names)} {target_kinds}".strip()
    linux_cargo_flags = f"{package_args(test_names - linux_excluded)} {target_kinds}".strip()

    rust = "true" if affected else "false"
    return {
        "rust": rust,
        "fmt": rust,
        "clippy": rust,
        "ui-clippy": "true" if ui_clippy else "false",
        "test": rust,
        "e2e": "true" if e2e else "false",
        "acceptance": "true" if e2e else "false",
        "web_ui": "true" if web_ui else "false",
        "packages": " ".join(affected),
        "package-args": package_args(affected),
        "linux-package-args": package_args(n for n in affected if n not in linux_excluded),
        "cargo-flags": cargo_flags,
        "linux-cargo-flags": linux_cargo_flags,
        "ui-args": ui_args,
    }


def workspace_facts():
    """`(root package, members carrying the `ui` feature, workspace
    dependents)` from `cargo metadata --no-deps`; the constants answer on
    any failure. `dependents` maps a member to the members directly
    depending on it — every dependency kind counts, since a dev- or
    build-dependency's change can still break the dependent's tests."""
    try:
        raw = subprocess.run(
            ["cargo", "metadata", "--no-deps", "--format-version", "1"],
            capture_output=True,
            text=True,
            check=True,
        ).stdout
        metadata = json.loads(raw)
        workspace_root = Path(metadata["workspace_root"])
        members = set(metadata["workspace_members"])
        member_names = set()
        root = ROOT_PACKAGE
        ui_crates = set()
        packages = []
        for package in metadata["packages"]:
            if package["id"] not in members:
                continue
            member_names.add(package["name"])
            packages.append(package)
            if Path(package["manifest_path"]).parent == workspace_root:
                root = package["name"]
            if "ui" in package.get("features", {}):
                ui_crates.add(package["name"])
        dependents = {}
        for package in packages:
            for dep in package.get("dependencies", []):
                target = dep.get("package") or dep["name"]
                if (
                    target in member_names
                    and target != package["name"]
                    and dep.get("source") is None
                ):
                    dependents.setdefault(target, set()).add(package["name"])
        return root, ui_crates or UI_FEATURE_PACKAGES, dependents
    except Exception:
        return ROOT_PACKAGE, UI_FEATURE_PACKAGES, {}


def git(*args):
    """One git call's stdout, raising on failure."""
    return subprocess.run(
        ["git", *args], capture_output=True, text=True, check=True
    ).stdout


def release_diff_lines(merge_base, base):
    """The `+`/`-` lines the manifests carry between base and HEAD."""
    if not merge_base:
        merge_base = git("merge-base", base, "HEAD").strip()
    diff = git("diff", "-U0", merge_base, "HEAD", "--", "Cargo.toml", "Cargo.lock")
    return diff.splitlines()


def load_report(report_file):
    """The parsed affected-crates report from `--report-file` or
    `AFFECTED_REPORT`."""
    raw = (
        Path(report_file).read_text(encoding="utf-8")
        if report_file is not None
        else os.environ["AFFECTED_REPORT"]
    )
    return json.loads(raw)


def emit(outputs):
    print(json.dumps(outputs))
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with open(output_file, "a", encoding="utf-8") as handle:
            for key, value in outputs.items():
                handle.write(f"{key}={value}\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--base",
        required=True,
        help="base revision or sha; empty selects the whole workspace",
    )
    parser.add_argument(
        "--report-file",
        help="path to the affected-crates JSON report (defaults to AFFECTED_REPORT)",
    )
    args = parser.parse_args()

    # Fail open: an unreadable report or a failed probe widens the gate.
    # `--workspace` is the safe `ui` scope here — it names no package, and
    # cargo enables the feature only on the members that carry it.
    outputs = workspace_outputs("--workspace")
    try:
        if not args.base:
            emit(outputs)
            return
        report = load_report(args.report_file)
        owners = report["owners"]
        if not isinstance(owners, dict):
            raise TypeError("report owners is not a map")
        paths = list(owners)
        if paths and set(paths) <= RELEASE_ONLY_PATHS:
            lines = release_diff_lines(report.get("merge_base"), args.base)
            if release_only_bump(paths, lines):
                emit(empty_outputs(""))
                return
        root_pkg, ui_crates, rev_deps = workspace_facts()
        emit(
            compute(
                report, root_pkg=root_pkg, ui_crates=ui_crates, rev_deps=rev_deps
            )
        )
    except Exception as error:  # widen on any failure — never scope on a guess
        print(
            f"::warning::affected.py fell back to the whole workspace: {error}",
            file=sys.stderr,
        )
        emit(outputs)


if __name__ == "__main__":
    main()
