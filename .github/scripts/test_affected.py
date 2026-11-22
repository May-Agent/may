"""Unit tests for affected.py — the CI gate's scope computation.

Feeds synthetic `lexoliu/affected-crates` reports through `compute` and the
`main` entry, so no git history or cargo run is needed.
"""

import io
import json
import os
import subprocess
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))

import affected

# The workspace as it stands: the root package plus the member crates (`sed/`
# is its own standalone workspace, like `hashline/`).
MEMBERS = ["may-agent", "may-container-image", "may-patch", "may-tasks"]

# …and the near-future shape once the subsystem crates land.
FUTURE_MEMBERS = MEMBERS + [
    "may-client",
    "may-config",
    "may-daemon",
    "may-harness",
    "may-tui",
]

# Direct workspace dependents in that future shape — an entry X -> {Y, Z}
# means Y and Z build on X, so a change to X reaches them. Mirrors the
# split's Cargo.toml edges: may-config feeds the client, harness, daemon,
# tui and the root binary; may-client feeds the tui, daemon and binary.
FUTURE_DEPS = {
    "may-config": {"may-agent", "may-client", "may-daemon", "may-harness", "may-tui"},
    "may-client": {"may-agent", "may-daemon", "may-tui"},
    "may-harness": {"may-agent", "may-client", "may-daemon", "may-tui"},
    "may-tui": {"may-agent"},
    "may-daemon": {"may-agent"},
    "may-container-image": {"may-agent"},
    "may-tasks": {"may-agent", "may-client", "may-config", "may-harness", "may-tui"},
    "may-patch": {"may-agent", "may-harness"},
}

ROOT = "may-agent"
UI = {"may-agent"}


def report(*, workspace=False, affected=(), owners=None, members=MEMBERS):
    """A synthetic affected-crates report."""
    return {
        "merge_base": "0" * 40,
        "workspace": workspace,
        "affected": list(affected),
        "members": list(members),
        "owners": owners or {},
    }


def compute(r, **kwargs):
    kwargs.setdefault("root_pkg", ROOT)
    kwargs.setdefault("ui_crates", UI)
    return affected.compute(r, **kwargs)


class HelperTests(unittest.TestCase):
    def test_package_args_sorts(self):
        self.assertEqual(affected.package_args({"may-sed", "may-agent"}), "-p may-agent -p may-sed")
        self.assertEqual(affected.package_args([]), "")

    def test_reverse_closure(self):
        # b <-> a exercises the dev-dependency cycles cargo permits.
        dependents = {"a": {"b", "c"}, "b": {"a"}, "c": {"d"}}
        self.assertEqual(
            affected.reverse_closure({"a"}, dependents), {"a", "b", "c", "d"}
        )
        self.assertEqual(affected.reverse_closure({"d"}, dependents), {"d"})
        self.assertEqual(affected.reverse_closure(set(), dependents), set())

    def test_workspace_facts_reads_dependents(self):
        metadata = {
            "workspace_root": "/repo",
            "workspace_members": [
                "may-agent 0.1.0 (path+file:///repo)",
                "may-config 0.1.0 (path+file:///repo/config)",
            ],
            "packages": [
                {
                    "name": "may-agent",
                    "id": "may-agent 0.1.0 (path+file:///repo)",
                    "manifest_path": "/repo/Cargo.toml",
                    "features": {"ui": []},
                    "dependencies": [
                        {"name": "may-config", "source": None, "kind": None},
                        {
                            "name": "serde",
                            "source": "registry+https://github.com/rust-lang/crates.io-index",
                            "kind": None,
                        },
                    ],
                },
                {
                    "name": "may-config",
                    "id": "may-config 0.1.0 (path+file:///repo/config)",
                    "manifest_path": "/repo/config/Cargo.toml",
                    "features": {},
                    # A dev-dependency edge counts as much as a normal one.
                    "dependencies": [
                        {"name": "may-agent", "source": None, "kind": "dev"},
                    ],
                },
            ],
        }
        done = subprocess.CompletedProcess(
            args=[], returncode=0, stdout=json.dumps(metadata), stderr=""
        )
        with mock.patch.object(affected.subprocess, "run", return_value=done):
            root, ui_crates, dependents = affected.workspace_facts()
        self.assertEqual(root, "may-agent")
        self.assertEqual(ui_crates, {"may-agent"})
        self.assertEqual(dependents["may-config"], {"may-agent"})
        self.assertEqual(dependents["may-agent"], {"may-config"})
        # Registry dependencies never name a member, so none leak in.
        self.assertNotIn("serde", dependents)

    def test_workspace_facts_falls_back_on_failure(self):
        with mock.patch.object(
            affected.subprocess, "run", side_effect=OSError("no cargo")
        ):
            self.assertEqual(
                affected.workspace_facts(),
                (affected.ROOT_PACKAGE, affected.UI_FEATURE_PACKAGES, {}),
            )

    def test_is_prose(self):
        for prose in [
            "README.md",
            "CHANGELOG.md",
            "LICENSE",
            "docs/guide.md",
            ".github/ISSUE_TEMPLATE/bug.md",
            "patch/README.md",  # a crate's own prose
            "sed/LICENSE",
            "tasks/notes.md",
        ]:
            self.assertTrue(affected.is_prose(prose), prose)
        for not_prose in [
            "src/config.rs",
            "patch/src/lib.rs",
            "templates/prompt/layout.txt",  # embedded: askama/included
            "skills/may/SKILL.md",  # embedded: include_dir!
            "subagents/audit.toml",
            "container/workspace/Dockerfile",
            "web-ui/src/app.tsx",
        ]:
            self.assertFalse(affected.is_prose(not_prose), not_prose)

    def test_reaches_runtime(self):
        # Direct owners: every member is runtime except the client-only set.
        self.assertTrue(affected.reaches_runtime("src/daemon/mod.rs", "may-agent"))
        self.assertTrue(affected.reaches_runtime("tasks/src/main.rs", "may-tasks"))
        self.assertTrue(affected.reaches_runtime("patch/src/lib.rs", "may-patch"))
        self.assertFalse(affected.reaches_runtime("tui/src/app.rs", "may-tui"))
        # A crate's own prose reaches no runtime even though its owner is real.
        self.assertFalse(affected.reaches_runtime("patch/README.md", "may-patch"))
        # Rule-consumed paths (owner None): the embedded dirs and Cargo.lock
        # reach the build; prose, web-ui, standalone workspaces and metadata do not.
        self.assertTrue(affected.reaches_runtime("skills/may/SKILL.md", None))
        self.assertTrue(affected.reaches_runtime("subagents/audit.toml", None))
        self.assertTrue(affected.reaches_runtime("templates/prompt/layout.txt", None))
        self.assertTrue(affected.reaches_runtime("container/base/Dockerfile", None))
        self.assertTrue(affected.reaches_runtime("Cargo.lock", None))
        self.assertFalse(affected.reaches_runtime("README.md", None))
        self.assertFalse(affected.reaches_runtime("web-ui/src/app.tsx", None))
        self.assertFalse(affected.reaches_runtime("hashline/src/lib.rs", None))
        self.assertFalse(affected.reaches_runtime("sed/src/main.rs", None))
        self.assertFalse(affected.reaches_runtime("patch/.gitignore", None))

    def test_release_only_bump(self):
        version_diff = [
            "--- a/Cargo.toml",
            "+++ b/Cargo.toml",
            "-version = \"0.1.1\"",
            "+version = \"0.1.2\"",
            "--- a/Cargo.lock",
            "+++ b/Cargo.lock",
            "-version = \"0.1.1\"",
            "+version = \"0.1.2\"",
        ]
        self.assertTrue(
            affected.release_only_bump({"CHANGELOG.md", "Cargo.toml", "Cargo.lock"}, version_diff)
        )
        # A changelog-only diff counts: no manifest lines at all.
        self.assertTrue(affected.release_only_bump({"CHANGELOG.md"}, []))
        # A non-version manifest line disqualifies.
        self.assertFalse(
            affected.release_only_bump(
                {"Cargo.toml", "Cargo.lock"}, version_diff + ["+[dependencies]"]
            )
        )
        # So does any path outside the release set.
        self.assertFalse(affected.release_only_bump({"Cargo.toml", "src/lib.rs"}, version_diff))
        self.assertFalse(affected.release_only_bump({}, version_diff))


class ComputeTests(unittest.TestCase):
    def test_workspace_scope_runs_everything(self):
        out = compute(report(workspace=True, affected=MEMBERS, owners={"x": None}))
        self.assertEqual(out["rust"], "true")
        # A main-workspace gate change does not touch the standalone sed/ workspace.
        self.assertEqual(out["sed"], "false")
        self.assertEqual(out["packages"], "workspace")
        self.assertEqual(out["package-args"], "--workspace")
        self.assertEqual(out["linux-package-args"], "--workspace")
        self.assertEqual(out["cargo-flags"], "--workspace --all-targets")
        for key in ["fmt", "clippy", "ui-clippy", "test", "e2e", "acceptance", "web_ui"]:
            self.assertEqual(out[key], "true", key)
        self.assertEqual(out["ui-args"], "-p may-agent")

    def test_workspace_scope_with_linux_excluded(self):
        out = compute(
            report(workspace=True, affected=FUTURE_MEMBERS, owners={"x": None},
                   members=FUTURE_MEMBERS),
            linux_excluded={"may-tui"},
        )
        self.assertEqual(out["package-args"], "--workspace")
        self.assertEqual(out["linux-package-args"], "--workspace --exclude may-tui")
        self.assertEqual(
            out["linux-cargo-flags"], "--workspace --exclude may-tui --all-targets"
        )
        self.assertEqual(out["cargo-flags"], "--workspace --all-targets")

    def test_empty_diff_runs_nothing(self):
        out = compute(report(owners={}))
        self.assertEqual(out["rust"], "false")
        self.assertEqual(out["packages"], "")
        for key in ["fmt", "clippy", "ui-clippy", "test", "e2e", "acceptance", "web_ui"]:
            self.assertEqual(out[key], "false", key)

    def test_prose_only_runs_nothing(self):
        out = compute(
            report(owners={"README.md": None, "docs/guide.md": None, "CHANGELOG.md": None})
        )
        self.assertEqual(out["rust"], "false")
        self.assertEqual(out["e2e"], "false")
        self.assertEqual(out["web_ui"], "false")
        self.assertEqual(out["packages"], "")

    def test_in_package_prose_scopes_to_its_crate(self):
        # patch/README.md compiles into no binary, but its owner is real:
        # the crate lints and unit-tests, while the e2e gates stay off.
        out = compute(report(affected=["may-patch"], owners={"patch/README.md": "may-patch"}))
        self.assertEqual(out["rust"], "true")
        self.assertEqual(out["packages"], "may-patch")
        self.assertEqual(out["package-args"], "-p may-patch")
        self.assertEqual(out["e2e"], "false")
        self.assertEqual(out["acceptance"], "false")
        self.assertEqual(out["cargo-flags"], "-p may-patch --lib --bins")
        self.assertEqual(out["ui-clippy"], "false")

    def test_standalone_sed_workspace_runs_sed_only(self):
        # `sed/` is a standalone workspace: editing it runs `sed`'s own fmt,
        # clippy, and test steps without building or testing `may-agent`.
        out = compute(report(owners={"sed/src/main.rs": None}))
        self.assertEqual(out["rust"], "false")
        self.assertEqual(out["sed"], "true")
        self.assertEqual(out["fmt"], "true")
        self.assertEqual(out["clippy"], "true")
        self.assertEqual(out["test"], "true")
        self.assertEqual(out["packages"], "")
        self.assertEqual(out["package-args"], "")
        self.assertEqual(out["e2e"], "false")
        self.assertEqual(out["acceptance"], "false")
        self.assertEqual(out["ui-clippy"], "false")
        self.assertEqual(out["web_ui"], "false")

    def test_leaf_tool_crate_keeps_e2e(self):
        # may-tasks ships inside the workspace and container image: its sources
        # reach the runtime, so the e2e and container gates stay on and the
        # root package joins the build set for the `may` and e2e binaries.
        out = compute(report(affected=["may-tasks"], owners={"tasks/src/main.rs": "may-tasks"}))
        self.assertEqual(out["sed"], "false")
        self.assertEqual(out["packages"], "may-tasks")
        self.assertEqual(out["package-args"], "-p may-tasks")
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(out["acceptance"], "true")
        self.assertEqual(out["cargo-flags"], "-p may-agent -p may-tasks --all-targets")
        self.assertEqual(out["ui-clippy"], "false")
        self.assertEqual(out["web_ui"], "false")

    def test_tool_crate_reverse_dependency_runs_full(self):
        # may-patch feeds the patch parser the daemon reads; its closure
        # already includes the root package.
        out = compute(
            report(
                affected=["may-agent", "may-patch"],
                owners={"patch/src/parser.rs": "may-patch"},
            )
        )
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(
            out["cargo-flags"], "-p may-agent -p may-patch --all-targets"
        )
        self.assertEqual(out["ui-clippy"], "true")

    def test_daemon_source_runs_full(self):
        out = compute(report(affected=["may-agent"], owners={"src/daemon/mod.rs": "may-agent"}))
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(out["cargo-flags"], "-p may-agent --all-targets")
        self.assertEqual(out["ui-clippy"], "true")

    def test_client_only_diff_skips_e2e(self):
        # The key scenario: may-tui's reverse edge lands may-agent in
        # `affected`, but the diff reaches no daemon/sandbox/tools — e2e,
        # baseline-tools and container-acceptance stay off and the test
        # jobs compile no integration targets.
        out = compute(
            report(
                affected=["may-agent", "may-tui"],
                owners={"tui/src/app.rs": "may-tui"},
                members=FUTURE_MEMBERS,
            )
        )
        self.assertEqual(out["rust"], "true")
        self.assertEqual(out["packages"], "may-agent may-tui")
        self.assertEqual(out["package-args"], "-p may-agent -p may-tui")
        self.assertEqual(out["e2e"], "false")
        self.assertEqual(out["acceptance"], "false")
        self.assertEqual(out["cargo-flags"], "-p may-agent -p may-tui --lib --bins")
        # The ui-carrying crate is in scope, so the embed still gets checked.
        self.assertEqual(out["ui-clippy"], "true")

    def test_client_only_plus_runtime_diff_runs_e2e(self):
        out = compute(
            report(
                affected=["may-agent", "may-tui"],
                owners={"tui/src/app.rs": "may-tui", "src/daemon/mod.rs": "may-agent"},
                members=FUTURE_MEMBERS,
            )
        )
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(out["cargo-flags"], "-p may-agent -p may-tui --all-targets")

    def test_client_only_mixed_with_tool_crate_runs_e2e(self):
        out = compute(
            report(
                affected=["may-agent", "may-tasks", "may-tui"],
                owners={"tui/src/app.rs": "may-tui", "tasks/src/main.rs": "may-tasks"},
                members=FUTURE_MEMBERS,
            )
        )
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(
            out["cargo-flags"], "-p may-agent -p may-tasks -p may-tui --all-targets"
        )

    def test_web_ui_only_runs_frontend_and_ui_clippy(self):
        out = compute(report(owners={"web-ui/src/app.tsx": None, "web-ui/package.json": None}))
        self.assertEqual(out["rust"], "false")
        self.assertEqual(out["packages"], "")
        self.assertEqual(out["web_ui"], "true")
        self.assertEqual(out["ui-clippy"], "true")
        self.assertEqual(out["ui-args"], "-p may-agent")
        self.assertEqual(out["e2e"], "false")

    def test_web_ui_plus_rust_source(self):
        out = compute(
            report(
                affected=["may-agent"],
                owners={"web-ui/src/app.tsx": None, "src/gui.rs": "may-agent"},
            )
        )
        self.assertEqual(out["rust"], "true")
        self.assertEqual(out["web_ui"], "true")
        self.assertEqual(out["ui-clippy"], "true")
        self.assertEqual(out["e2e"], "true")

    def test_embedded_assets_reach_runtime(self):
        # Today only the root package embeds skills/, subagents/ and
        # templates/, so the candidates∩members add nothing to the rule's
        # mark. container/ also lands may-container-image, whose tests read
        # the contexts — a member that exists already.
        for path in [
            "skills/may/SKILL.md",
            "subagents/audit.toml",
            "templates/prompt/layout.txt",
        ]:
            out = compute(report(affected=["may-agent"], owners={path: None}))
            self.assertEqual(out["e2e"], "true", path)
            self.assertEqual(out["packages"], "may-agent", path)
        out = compute(
            report(affected=["may-agent"], owners={"container/workspace/Dockerfile": None})
        )
        self.assertEqual(out["e2e"], "true")
        self.assertEqual(out["packages"], "may-agent may-container-image")

    def test_embedded_assets_after_the_split(self):
        # Once the split crates are members, an asset diff runs their
        # suites plus every workspace dependent's — the rules file still
        # marks only the root package it can name.
        for path, expected in [
            # may-config embeds subagents/; its dependents follow.
            (
                "subagents/auditor.md",
                "may-agent may-client may-config may-daemon may-harness may-tui",
            ),
            # may-client embeds skills/; may-config is a candidate too.
            (
                "skills/may/SKILL.md",
                "may-agent may-client may-config may-daemon may-harness may-tui",
            ),
            # may-tui, may-harness and may-daemon compile templates/;
            # may-agent-core is filtered out, not being a member yet.
            (
                "templates/prompt/layout.txt",
                "may-agent may-client may-daemon may-harness may-tui",
            ),
            # The daemon joins the container hash owner and the image crate.
            (
                "container/workspace/Dockerfile",
                "may-agent may-container-image may-daemon",
            ),
        ]:
            out = compute(
                report(
                    affected=["may-agent"],
                    owners={path: None},
                    members=FUTURE_MEMBERS,
                ),
                rev_deps=FUTURE_DEPS,
            )
            self.assertEqual(out["packages"], expected, path)
            self.assertEqual(out["e2e"], "true", path)
            # The test jobs compile the embedders' suites with the e2e
            # binaries alongside, so the whole set takes --all-targets.
            self.assertEqual(
                out["cargo-flags"],
                "-p " + expected.replace(" ", " -p ") + " --all-targets",
                path,
            )

    def test_lockfile_reaches_runtime(self):
        out = compute(report(affected=["may-agent"], owners={"Cargo.lock": None}))
        self.assertEqual(out["e2e"], "true")

    def test_outside_workspace_crate_runs_nothing(self):
        out = compute(report(owners={"hashline/src/lib.rs": None, "hashline/Cargo.toml": None}))
        self.assertEqual(out["rust"], "false")
        self.assertEqual(out["e2e"], "false")
        self.assertEqual(out["web_ui"], "false")

    def test_bridge_file_feeds_the_frontend_job(self):
        out = compute(
            report(
                affected=["may-agent"],
                owners={"src/texts/artifact_runtime_bridge.js": "may-agent"},
            )
        )
        self.assertEqual(out["web_ui"], "true")
        self.assertEqual(out["rust"], "true")

    def test_linux_excluded_filtered(self):
        out = compute(
            report(
                affected=["may-agent", "may-tui"],
                owners={"tui/src/app.rs": "may-tui", "src/daemon/mod.rs": "may-agent"},
                members=FUTURE_MEMBERS,
            ),
            linux_excluded={"may-tui"},
        )
        self.assertEqual(out["package-args"], "-p may-agent -p may-tui")
        self.assertEqual(out["linux-package-args"], "-p may-agent")
        self.assertEqual(out["cargo-flags"], "-p may-agent -p may-tui --all-targets")
        self.assertEqual(out["linux-cargo-flags"], "-p may-agent --all-targets")

    def test_malformed_report_raises(self):
        with self.assertRaises(KeyError):
            compute({"workspace": False})
        with self.assertRaises(KeyError):
            compute({"workspace": False, "affected": [], "owners": {}})
        with self.assertRaises(TypeError):
            compute({"workspace": False, "affected": [], "members": [], "owners": None})


class MainTests(unittest.TestCase):
    def run_main(self, argv, env=None):
        argv = ["affected.py", *argv]
        env = env or {}
        with (
            tempfile.NamedTemporaryFile("r+", suffix=".out") as output_file,
            mock.patch.object(sys, "argv", argv),
            mock.patch.dict(os.environ, {"GITHUB_OUTPUT": output_file.name, **env}),
            redirect_stdout(io.StringIO()) as stdout,
            redirect_stderr(io.StringIO()) as stderr,
        ):
            affected.main()
            output_file.seek(0)
            written = output_file.read()
        outputs = dict(
            line.split("=", 1) for line in written.strip().splitlines() if "=" in line
        )
        return outputs, stdout.getvalue(), stderr.getvalue()

    def report_file(self, contents):
        handle = tempfile.NamedTemporaryFile("w", suffix=".json", delete=False)
        handle.write(contents)
        handle.close()
        self.addCleanup(os.unlink, handle.name)
        return handle.name

    def test_empty_base_selects_workspace_without_report(self):
        outputs, _, _ = self.run_main(["--base", "", "--report-file", "/nonexistent"])
        self.assertEqual(outputs["packages"], "workspace")
        self.assertEqual(outputs["e2e"], "true")

    def test_missing_report_widens_to_workspace(self):
        outputs, _, stderr = self.run_main(
            ["--base", "abc123", "--report-file", "/nonexistent/report.json"]
        )
        self.assertEqual(outputs["packages"], "workspace")
        self.assertIn("fell back", stderr)

    def test_malformed_report_widens_to_workspace(self):
        path = self.report_file("{not json")
        outputs, _, stderr = self.run_main(["--base", "abc123", "--report-file", path])
        self.assertEqual(outputs["packages"], "workspace")
        self.assertIn("fell back", stderr)

    def test_report_missing_keys_widens_to_workspace(self):
        path = self.report_file(json.dumps({"workspace": False}))
        outputs, _, stderr = self.run_main(["--base", "abc123", "--report-file", path])
        self.assertEqual(outputs["packages"], "workspace")
        self.assertIn("fell back", stderr)

    def test_scoped_report(self):
        r = report(affected=["may-tasks"], owners={"tasks/src/main.rs": "may-tasks"})
        path = self.report_file(json.dumps(r))
        with mock.patch.object(affected, "workspace_facts", return_value=(ROOT, UI, {})):
            outputs, _, _ = self.run_main(["--base", "abc123", "--report-file", path])
        self.assertEqual(outputs["packages"], "may-tasks")
        self.assertEqual(outputs["sed"], "false")
        self.assertEqual(outputs["cargo-flags"], "-p may-agent -p may-tasks --all-targets")

    def test_report_from_environment(self):
        r = report(affected=["may-patch"], owners={"patch/README.md": "may-patch"})
        path = self.report_file(json.dumps(r))
        with mock.patch.object(affected, "workspace_facts", return_value=(ROOT, UI, {})):
            outputs, _, _ = self.run_main(
                ["--base", "abc123"], env={"AFFECTED_REPORT": Path(path).read_text()}
            )
        self.assertEqual(outputs["e2e"], "false")
        self.assertEqual(outputs["cargo-flags"], "-p may-patch --lib --bins")

    def test_release_bump_runs_nothing(self):
        r = report(
            owners={"CHANGELOG.md": None, "Cargo.toml": None, "Cargo.lock": None}
        )
        path = self.report_file(json.dumps(r))
        version_lines = [
            "-version = \"0.1.1\"",
            "+version = \"0.1.2\"",
        ]
        with mock.patch.object(
            affected, "release_diff_lines", return_value=version_lines
        ), mock.patch.object(affected, "workspace_facts", return_value=(ROOT, UI, {})):
            outputs, _, _ = self.run_main(["--base", "abc123", "--report-file", path])
        self.assertEqual(outputs["rust"], "false")
        self.assertEqual(outputs["packages"], "")
        self.assertEqual(outputs["e2e"], "false")

    def test_release_set_with_real_edit_is_not_a_bump(self):
        r = report(
            affected=["may-patch"],
            owners={"CHANGELOG.md": None, "Cargo.lock": None, "patch/src/lib.rs": "may-patch"},
        )
        path = self.report_file(json.dumps(r))
        with mock.patch.object(affected, "workspace_facts", return_value=(ROOT, UI, {})):
            outputs, _, _ = self.run_main(["--base", "abc123", "--report-file", path])
        # paths ⊄ RELEASE_ONLY_PATHS, so no diff probe is needed.
        self.assertEqual(outputs["rust"], "true")
        self.assertEqual(outputs["e2e"], "true")


if __name__ == "__main__":
    unittest.main()
