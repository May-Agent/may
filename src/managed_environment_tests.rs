//! What a sandboxed terminal command finds on `PATH`: the managed copies of
//! the base tools first, the host's own toolchain after them.

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
        let managed = path.contains("/runtimes/") && path.ends_with(&format!("/bin/{name}"));
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
    let ssh = std::env::home_dir().expect("home").join(CREDENTIAL);
    assert!(ssh.is_dir(), "{} must exist for this test", ssh.display());

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
