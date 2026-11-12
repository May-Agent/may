//! What a sandboxed terminal command finds on `PATH`: the managed copies of
//! the base tools first, the host's own toolchain after them.

use std::fmt::Write as _;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::path::PathBuf;

use aither_sandbox::SubagentGrant;

use crate::repo_clone::test_sandbox::TestTerminal;

/// Commands the managed tools directory provides, coreutils included.
const MANAGED: &[&str] = &[
    "bash", "python3", "uv", "rg", "ast-grep", "jq", "fd", "xargs", "date", "stat", "timeout",
    "ls", "cat",
];

/// Commands only the host provides.
const HOST: &[&str] = &["cargo", "rustc"];

async fn resolve(names: &[&str]) -> Vec<(String, String)> {
    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let script = names
        .iter()
        .map(|name| format!("printf '%s=%s\\n' {name} \"$(command -v {name})\""))
        .collect::<Vec<_>>()
        .join("\n");
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the resolution script runs");
    output
        .lines()
        .map(|line| {
            let (name, path) = line.split_once('=').expect("name=path line");
            (name.to_string(), path.to_string())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn base_commands_resolve_to_the_managed_directory_and_the_toolchain_to_the_host() {
    let resolved = resolve(&[MANAGED, HOST].concat()).await;
    for (name, path) in &resolved {
        // python3 and uv resolve through the shims beside the managed tools.
        let directory = if matches!(name.as_str(), "python3" | "uv") {
            "shims"
        } else {
            "bin"
        };
        let managed =
            path.contains("/runtimes/") && path.ends_with(&format!("/{directory}/{name}"));
        if MANAGED.contains(&name.as_str()) {
            assert!(managed, "{name} resolved to {path:?}, not a managed copy");
        } else {
            assert!(
                !path.is_empty() && !managed,
                "{name} resolved to {path:?}, not the host's"
            );
        }
    }
}

/// A credential store aither's read policy denies.
const CREDENTIAL: &str = ".ssh";

/// Reads are allowed everywhere but the credential stores: a file outside
/// the session and its grants reads, and the user's `~/.ssh` does not.
#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_reads_ordinary_paths_and_not_credential_stores() {
    let session = tempfile::tempdir().expect("session dir");
    let outside = tempfile::tempdir().expect("a directory outside the session");
    let ordinary = outside.path().join("ordinary.txt");
    std::fs::write(&ordinary, "readable").expect("ordinary file");
    // A CI runner's home has no `~/.ssh`; the policy denies the path whether
    // or not it holds keys, so an empty one is enough.
    let ssh = std::env::home_dir().expect("home").join(CREDENTIAL);
    std::fs::create_dir_all(&ssh).expect("the credential directory exists");

    let terminal = TestTerminal::new(session.path()).await;
    let script = format!(
        "cat '{}'; echo\nls '{}' >/dev/null 2>&1 && echo ssh=readable || echo ssh=denied",
        ordinary.display(),
        ssh.display()
    );
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the script runs");
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        ["readable", "ssh=denied"]
    );
}

/// The host Rust toolchain works end to end in the sandbox: cargo runs, and
/// `cargo test` executes the binary it just built under the session.
#[tokio::test(flavor = "multi_thread")]
async fn the_host_rust_toolchain_builds_and_tests_a_crate() {
    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let sandbox = terminal.sandbox();
    sandbox
        .run(session.path(), "cargo --version")
        .await
        .expect("cargo is reachable inside the sandbox");

    let krate = session.path().join("probe");
    std::fs::create_dir_all(krate.join("src")).expect("the crate's source directory");
    std::fs::write(
        krate.join("Cargo.toml"),
        "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .expect("the manifest");
    std::fs::write(
        krate.join("src/lib.rs"),
        "#[test]\nfn it_runs() { assert_eq!(1 + 1, 2); }\n",
    )
    .expect("the test source");
    sandbox
        .run(
            &krate,
            &format!(
                "CARGO_TARGET_DIR='{}' cargo test",
                krate.join("target").display()
            ),
        )
        .await
        .expect("cargo test executes the binary it built under the session");
}

/// Removes a credentials file this test created when the host had none.
#[cfg(target_os = "macos")]
struct CreatedFile(Option<PathBuf>);

#[cfg(target_os = "macos")]
impl Drop for CreatedFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Credential files inside the granted toolchain homes stay unreadable, while
/// the home itself is readable.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn cargo_credentials_inside_the_granted_home_stay_unreadable() {
    let cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|home| home.join(".cargo")))
        .expect("a cargo home");
    std::fs::create_dir_all(&cargo_home).expect("the cargo home exists");
    let credentials = cargo_home.join("credentials.toml");
    let _created = CreatedFile(if credentials.exists() {
        None
    } else {
        std::fs::write(&credentials, "").expect("a credentials file");
        Some(credentials)
    });

    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let output = terminal
        .sandbox()
        .run(
            session.path(),
            "ls \"$CARGO_HOME\" >/dev/null && echo home=readable\n\
             cat \"$CARGO_HOME/credentials.toml\" >/dev/null 2>&1 \
             && echo credentials=readable || echo credentials=denied",
        )
        .await
        .expect("the script runs");
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        ["home=readable", "credentials=denied"]
    );
}

/// Host git in `dir`, for setting repositories up.
fn host_git(dir: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "user.name=May Test",
            "-c",
            "user.email=test@may.invalid",
        ])
        .args(args)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A script that writes `body` to `path` and makes it executable.
fn write_executable(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, body).expect("the program");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .expect("the program is executable");
}

/// The main agent's `git` in a committing worktree resolves the worktree's
/// host-owned repository entry through the shim: no program the subagent's
/// repository configuration names, and no `.git` file it replaced, decides
/// what runs.
#[tokio::test(flavor = "multi_thread")]
async fn the_main_agents_git_in_a_committing_worktree_runs_nothing_the_subagent_chose() {
    let tmp = tempfile::tempdir_in(dunce::canonicalize(std::env::temp_dir()).unwrap())
        .expect("a temporary directory");
    let session = tmp.path().join("session");
    let repository = session.join("repository");
    std::fs::create_dir_all(&repository).expect("the repository directory");
    host_git(&repository, &["init", "-q", "--initial-branch=main"]);
    host_git(
        &repository,
        &["commit", "-q", "--allow-empty", "-m", "initial"],
    );
    let worktree = session.join("guarded");
    host_git(
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "subagent/guarded/work",
            worktree.to_str().unwrap(),
        ],
    );
    let worktree = dunce::canonicalize(worktree).expect("the worktree");
    let entry = dunce::canonicalize(repository.join(".git"))
        .expect("the common dir")
        .join("worktrees/guarded");

    let terminal = TestTerminal::new(&session).await;
    let _child = terminal
        .shell()
        .subagent(
            SubagentGrant::new().with_committing_worktree(&worktree),
            None,
            "child",
        )
        .await
        .expect("the worktree can be granted with commit");
    let subagent = entry.join("subagent");
    assert!(subagent.is_dir(), "the subagent's repository exists");

    // A program only subagent-chosen configuration can reach.
    let marker = tmp.path().join("subagent-ran");
    let program = tmp.path().join("ran-by-git.sh");
    write_executable(
        &program,
        &format!("#!/bin/sh\nprintf ran >> '{}'\n", marker.display()),
    );
    let hooks = tmp.path().join("evil-hooks");
    std::fs::create_dir(&hooks).expect("the hooks directory");
    write_executable(
        &hooks.join("pre-commit"),
        &format!("#!/bin/sh\nprintf hook >> '{}'\n", marker.display()),
    );

    // Its repository's configuration names the program everywhere a program
    // can be named; the worktree's attributes bind the filter.
    let mut config = std::fs::read_to_string(subagent.join("config")).expect("its config");
    write!(
        config,
        "\n[core]\n\tfsmonitor = {program}\n\thooksPath = {hooks}\n\
         [filter \"pwn\"]\n\tclean = {program}\n\trequired = true\n",
        program = program.display(),
        hooks = hooks.display(),
    )
    .expect("a string takes the text");
    std::fs::write(subagent.join("config"), config).expect("its config is rewritten");
    std::fs::write(worktree.join(".gitattributes"), "* filter=pwn\n").expect("the attributes");

    let sandbox = terminal.sandbox();
    let tree = worktree.display();
    for script in [
        format!("git -C '{tree}' status"),
        format!("cd '{tree}' && git status"),
        format!(
            "git -C '{tree}' add -A && git -C '{tree}' -c user.name=a -c user.email=a@a \
             -c commit.gpgSign=false commit --allow-empty -m x"
        ),
    ] {
        sandbox
            .run(&session, &script)
            .await
            .unwrap_or_else(|error| panic!("the ordinary git command keeps working: {error}"));
    }
    assert!(
        !marker.exists(),
        "no program of the subagent's choosing ran through the worktree's repository"
    );

    // A replaced `.git` file resolves nothing either: the shim acts on the
    // worktree's path, not on what its `.git` says.
    std::fs::write(
        worktree.join(".git"),
        format!("gitdir: {}\n", subagent.display()),
    )
    .expect("the .git file is replaced");
    sandbox
        .run(&session, &format!("git -C '{tree}' status"))
        .await
        .expect("git still works");
    assert!(!marker.exists(), "a replaced .git file chose no program");

    // Git anywhere else is untouched.
    sandbox
        .run(
            &session,
            &format!("git -C '{}' log --oneline -1", repository.display()),
        )
        .await
        .expect("git in unrelated repositories still reaches them");
}
