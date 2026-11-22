//! What a sandboxed terminal command finds on `PATH`: the managed copies of
//! the base tools first, the host's own toolchain after them.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use aither_sandbox::SubagentGrant;

use crate::client_tests::test_sandbox::TestTerminal;

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

/// Credential directories inside the user's home that aither's read policy denies.
const CREDENTIALS: &[&str] = &[".ssh", ".config/gh", ".config/glab-cli"];

/// Removes a directory entry created by a test when the host had none.
struct CreatedDir(Option<PathBuf>);

impl Drop for CreatedDir {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_dir(path);
        }
    }
}

/// Ensures `path` exists for a negative-read test and removes newly created
/// empty directories on drop without touching pre-existing host state.
fn ensure_test_dir(path: &Path) -> Vec<CreatedDir> {
    let mut created = Vec::new();
    let mut missing = Vec::new();
    let mut current = Some(path);
    while let Some(dir) = current {
        if dir.exists() {
            break;
        }
        missing.push(dir.to_path_buf());
        current = dir.parent();
    }
    for dir in missing.iter().rev() {
        std::fs::create_dir(dir).expect("test credential directory creates");
        created.push(CreatedDir(Some(dir.clone())));
    }
    // Drop leaves deepest first when reversed at the caller or popped; keep
    // children ahead of parents so `remove_dir` removes the empty leaf first.
    created.reverse();
    created
}

/// Reads are allowed everywhere but the credential stores: a file outside
/// the session and its grants reads, while the user's `~/.ssh`,
/// `~/.config/gh` and `~/.config/glab-cli` stay unreadable.
#[tokio::test(flavor = "multi_thread")]
async fn the_sandbox_reads_ordinary_paths_and_not_credential_stores() {
    let session = tempfile::tempdir().expect("session dir");
    let outside = tempfile::tempdir().expect("a directory outside the session");
    let ordinary = outside.path().join("ordinary.txt");
    std::fs::write(&ordinary, "readable").expect("ordinary file");
    let home = std::env::home_dir().expect("home");
    // A CI runner's home may lack `~/.ssh`, `~/.config/gh` or
    // `~/.config/glab-cli`; the policy denies each path whether or not it
    // holds files.
    let created_dirs: Vec<CreatedDir> = CREDENTIALS
        .iter()
        .flat_map(|rel| ensure_test_dir(&home.join(rel)))
        .collect();
    let mut script = format!("cat '{}'; echo\n", ordinary.display());
    let mut expected = vec!["readable".to_string()];
    for rel in CREDENTIALS {
        let cred = home.join(rel);
        let _ = writeln!(
            script,
            "ls '{}' >/dev/null 2>&1 && echo {rel}=readable || echo {rel}=denied",
            cred.display()
        );
        expected.push(format!("{rel}=denied"));
    }

    let terminal = TestTerminal::new(session.path()).await;
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the script runs");
    drop(created_dirs);
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
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
struct CreatedFile(Option<PathBuf>);

impl Drop for CreatedFile {
    fn drop(&mut self) {
        if let Some(path) = &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Credential files inside the granted toolchain homes stay unreadable, while
/// the home itself is readable.
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

/// Keychain directories and macOS keychain security services stay
/// unreadable from inside the sandbox.
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn macos_keychains_stay_unreadable() {
    let home = std::env::home_dir().expect("home");
    let user_keychains = home.join("Library").join("Keychains");
    let _created = ensure_test_dir(&user_keychains);
    let system_keychains = Path::new("/Library/Keychains");
    assert!(
        system_keychains.exists(),
        "macOS system keychains directory exists"
    );

    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let script = format!(
        "ls '{}' >/dev/null 2>&1 && echo user_kc=readable || echo user_kc=denied\n\
         ls '{}' >/dev/null 2>&1 && echo sys_kc=readable || echo sys_kc=denied\n\
         cat /Library/Keychains/System.keychain >/dev/null 2>&1 && echo sys_file=readable || echo sys_file=denied\n\
         security list-keychains >/dev/null 2>&1 && echo list_svc=readable || echo list_svc=denied\n\
         security default-keychain >/dev/null 2>&1 && echo default_svc=readable || echo default_svc=denied",
        user_keychains.display(),
        system_keychains.display()
    );
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the script runs");
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        [
            "user_kc=denied",
            "sys_kc=denied",
            "sys_file=denied",
            "list_svc=denied",
            "default_svc=denied",
        ],
        "macOS keychain stores and SecurityServer lookups must stay denied: {output}"
    );
}

/// The managed base commands (GNU coreutils, rg, ast-grep, jq, fd, xargs, uv)
/// and managed Python execute inside the sandbox on both macOS and Linux.
#[tokio::test(flavor = "multi_thread")]
async fn base_commands_and_python_run_inside_the_sandbox() {
    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let script = "\
        bash --version >/dev/null\n\
        date --version >/dev/null\n\
        stat --version >/dev/null\n\
        timeout --version >/dev/null\n\
        rg --version >/dev/null\n\
        ast-grep --version >/dev/null\n\
        jq --version >/dev/null\n\
        fd --version >/dev/null\n\
        xargs --version >/dev/null\n\
        uv --version >/dev/null\n\
        python3 -c 'import sys; assert sys.version_info >= (3, 10); print(\"python-ok\")'\n\
    ";
    let output = terminal
        .sandbox()
        .run(session.path(), script)
        .await
        .expect("managed base commands and python run");
    assert_eq!(output.trim(), "python-ok");
}

/// Writes succeed inside the granted session directory and are denied outside
/// it (including the managed tools directory).
#[tokio::test(flavor = "multi_thread")]
async fn writes_outside_granted_paths_are_denied() {
    let session = tempfile::tempdir().expect("session dir");
    let outside = tempfile::tempdir().expect("outside dir");
    let terminal = TestTerminal::new(session.path()).await;
    let denied_outside = outside.path().join("denied.txt");
    let script = format!(
        "echo ok > '{}/ok.txt' && echo session=writable || echo session=denied\n\
         (echo no > '{}' ) 2>/dev/null && echo outside=writable || echo outside=denied\n\
         bin_dir=$(dirname \"$(command -v rg)\")\n\
         (echo no > \"$bin_dir/denied.txt\" ) 2>/dev/null && echo bin=writable || echo bin=denied",
        session.path().display(),
        denied_outside.display(),
    );
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the write probe script runs");
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        ["session=writable", "outside=denied", "bin=denied"],
        "unexpected write permissions: {output}"
    );
    assert!(!denied_outside.exists(), "outside file must not exist");
}

/// Linux credential and keyring stores stay unreadable from inside the sandbox.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn linux_credential_stores_stay_unreadable() {
    const LINUX_CREDENTIALS: &[&str] =
        &[".gnupg", ".local/share/keyrings", ".local/share/kwalletd"];
    let home = std::env::home_dir().expect("home");
    let created_dirs: Vec<CreatedDir> = LINUX_CREDENTIALS
        .iter()
        .flat_map(|rel| ensure_test_dir(&home.join(rel)))
        .collect();
    let mut script = String::new();
    let mut expected = Vec::with_capacity(LINUX_CREDENTIALS.len());
    for rel in LINUX_CREDENTIALS {
        let cred = home.join(rel);
        let _ = writeln!(
            script,
            "ls '{}' >/dev/null 2>&1 && echo {rel}=readable || echo {rel}=denied",
            cred.display()
        );
        expected.push(format!("{rel}=denied"));
    }

    let session = tempfile::tempdir().expect("session dir");
    let terminal = TestTerminal::new(session.path()).await;
    let output = terminal
        .sandbox()
        .run(session.path(), &script)
        .await
        .expect("the script runs");
    drop(created_dirs);
    assert_eq!(
        output.lines().collect::<Vec<_>>(),
        expected.iter().map(String::as_str).collect::<Vec<_>>()
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
