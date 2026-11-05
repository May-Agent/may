use std::path::{Path, PathBuf};
use std::process::Command;

use super::test_user::{EMAIL, NAME, TestUser};
use super::{CommitSigning, SigningError};
use crate::repo_clone::test_sandbox::TestTerminal;

fn canonical_tempdir() -> tempfile::TempDir {
    tempfile::tempdir_in(dunce::canonicalize(std::env::temp_dir()).unwrap()).unwrap()
}

/// Host git in `dir` with no signing, for setting repositories up.
fn host_git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/dev/null",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `status|author|committer` of each commit of `range`, verified against the
/// user's allowed signers with `git verify`'s own machinery.
fn verified_log(dir: &Path, user: &TestUser, range: &str) -> Vec<String> {
    host_git(
        dir,
        &[
            "-c",
            "gpg.format=ssh",
            "-c",
            "gpg.ssh.program=ssh-keygen",
            "-c",
            &format!(
                "gpg.ssh.allowedSignersFile={}",
                user.allowed_signers().display()
            ),
            "log",
            "--format=%G?|%an <%ae>|%cn <%ce>",
            range,
        ],
    )
    .lines()
    .map(str::to_string)
    .collect()
}

struct Session {
    tmp: tempfile::TempDir,
    user: TestUser,
    signing: CommitSigning,
    repo: PathBuf,
    base: String,
    terminal: TestTerminal,
}

impl Session {
    async fn new() -> Self {
        let tmp = canonical_tempdir();
        let user = TestUser::new(&tmp.path().join("user"));
        let signing = user.signing(&tmp.path().join("may"), "s1");
        let root = tmp.path().join("session");
        let terminal = TestTerminal::signing(&root, signing.clone()).await;
        let repo = root.join("repo");
        host_git(
            tmp.path(),
            &["init", "-q", "-b", "main", &repo.to_string_lossy()],
        );
        host_git(&repo, &["config", "user.name", "Base"]);
        host_git(&repo, &["config", "user.email", "base@example.com"]);
        std::fs::write(repo.join("base.txt"), "base").unwrap();
        host_git(&repo, &["add", "."]);
        host_git(&repo, &["commit", "-q", "-m", "base"]);
        // The clone carries no configuration of its own: identity and
        // signing come from the sandbox's HOME.
        host_git(&repo, &["config", "--unset", "user.name"]);
        host_git(&repo, &["config", "--unset", "user.email"]);
        let base = host_git(&repo, &["rev-parse", "HEAD"]);
        Self {
            tmp,
            user,
            signing,
            repo,
            base,
            terminal,
        }
    }

    async fn run(&self, script: &str) -> Result<String, crate::repo_clone::RepoCloneError> {
        self.terminal.sandbox().run(&self.repo, script).await
    }

    fn log(&self, range: &str) -> Vec<String> {
        verified_log(&self.repo, &self.user, range)
    }
}

fn user_line() -> String {
    format!("G|{NAME} <{EMAIL}>|{NAME} <{EMAIL}>")
}

/// Commit, amend, a rebase that rewrites commits and a merge commit, all made
/// by the sandbox's git, carry the user's identity and a valid signature.
#[tokio::test(flavor = "multi_thread")]
async fn sandboxed_commits_are_the_users_and_signed() {
    let s = Session::new().await;

    s.run("echo one > one.txt && git add . && git commit -q -m one")
        .await
        .unwrap();
    assert_eq!(s.log(&format!("{}..HEAD", s.base)), [user_line()]);

    s.run("git commit -q --amend -m 'one, amended'")
        .await
        .unwrap();
    assert_eq!(s.log(&format!("{}..HEAD", s.base)), [user_line()]);

    // topic forks from main, main moves on, topic is rebased onto it:
    // every topic commit is re-created.
    s.run(
        "git checkout -q -b topic && echo t1 > t1.txt && git add . && git commit -q -m t1 \
         && echo t2 > t2.txt && git add . && git commit -q -m t2 \
         && git checkout -q main && echo m > m.txt && git add . && git commit -q -m m",
    )
    .await
    .unwrap();
    let topic_before = s.run("git rev-parse topic").await.unwrap();
    s.run("git checkout -q topic && git rebase -q main")
        .await
        .unwrap();
    assert_ne!(s.run("git rev-parse topic").await.unwrap(), topic_before);
    assert_eq!(
        s.log("main..topic"),
        [user_line(), user_line()],
        "rebased commits are signed"
    );

    s.run("git checkout -q main && git merge -q --no-ff -m merged topic")
        .await
        .unwrap();
    let parents = s.run("git rev-list --parents -n 1 HEAD").await.unwrap();
    assert_eq!(parents.split_whitespace().count(), 3, "a merge commit");
    let all = s.log(&format!("{}..main", s.base));
    assert_eq!(all.len(), 5, "{all:?}");
    assert!(all.iter().all(|line| *line == user_line()), "{all:?}");

    // Cherry-pick and revert create commits too.
    s.run(
        "git checkout -q -b side main~1 && echo c > c.txt && git add . && git commit -q -m c \
         && git checkout -q main && git cherry-pick side >/dev/null && git revert --no-edit HEAD >/dev/null",
    )
    .await
    .unwrap();
    let all = s.log(&format!("{}..main", s.base));
    assert!(all.iter().all(|line| *line == user_line()), "{all:?}");

    // Every signing is in the audit log, attributed to the main agent.
    let log = std::fs::read_to_string(s.signing.audit_log()).unwrap();
    let records: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(records.len() >= all.len(), "{} records", records.len());
    for record in &records {
        assert_eq!(record["session"], "s1");
        // The test terminal runs no agent loop, so calls carry no identity.
        assert_eq!(record["agent"], "unknown");
        assert_eq!(record["identity"], format!("{NAME} <{EMAIL}>"));
        assert_eq!(record["tree"].as_str().unwrap().len(), 40);
        assert!(record["parents"].is_array());
        assert!(record["time"].as_str().unwrap().ends_with('Z'));
    }
}

/// What `git config --file` reads for `key`.
fn config_value(file: &Path, key: &str) -> String {
    let output = Command::new("git")
        .args(["config", "--file"])
        .arg(file)
        .args(["--get", key])
        .output()
        .unwrap();
    assert!(output.status.success(), "{key} does not parse");
    String::from_utf8_lossy(&output.stdout)
        .trim_end_matches('\n')
        .to_string()
}

/// A name with every character git config treats specially reads back from
/// the sandbox's configuration exactly as the user's is.
#[tokio::test]
async fn the_sandbox_config_quotes_what_git_config_treats_specially() {
    let tmp = canonical_tempdir();
    let name = r#"Q "The \ User" # note ; end"#;
    let user = TestUser::named(&tmp.path().join("user"), name);
    let root = tmp.path().join("session");
    std::fs::create_dir_all(&root).unwrap();

    user.signing(tmp.path(), "s1")
        .install_sandbox_git(&root, None)
        .await
        .unwrap();

    let config = root.join(".gitconfig");
    assert_eq!(config_value(&config, "user.name"), name);
    assert_eq!(config_value(&config, "user.email"), EMAIL);
    assert_eq!(config_value(&config, "gpg.format"), "ssh");
    assert_eq!(config_value(&config, "commit.gpgsign"), "true");
    assert!(config_value(&config, "user.signingkey").starts_with("key::ssh-ed25519 "));
    assert_eq!(
        config_value(&config, "gpg.ssh.program"),
        root.join(".config/may/ssh-sign").to_string_lossy()
    );
}

/// A container reads the same files through its mount of the session
/// directory, and finds the signer at its in-container path.
#[tokio::test]
async fn a_container_is_given_the_signer_at_its_in_container_path() {
    let tmp = canonical_tempdir();
    let user = TestUser::new(&tmp.path().join("user"));
    let root = tmp.path().join("session");
    std::fs::create_dir_all(&root).unwrap();
    let mount = crate::container::lifecycle::workspace_mount(root.clone());

    user.signing(tmp.path(), "s1")
        .install_sandbox_git(&root, Some(&mount))
        .await
        .unwrap();

    assert_eq!(
        config_value(&root.join(".gitconfig"), "gpg.ssh.program"),
        "/workspace/.config/may/ssh-sign"
    );
    let script = std::fs::read_to_string(root.join(".config/may/ssh-sign")).unwrap();
    assert!(script.contains(super::SIGN_COMMAND), "{script}");
    // A directory outside the mount is an error, not a guessed path.
    let elsewhere = tmp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    assert!(
        user.signing(tmp.path(), "s1")
            .install_sandbox_git(&elsewhere, Some(&mount))
            .await
            .is_err()
    );
}

/// `user.signingkey` is a file or a literal key, as git reads it. A `~` path
/// needs a home directory and is an error without one (not testable here:
/// the test host has one).
#[test]
fn a_signing_key_is_a_file_or_a_literal() {
    assert!(matches!(
        super::SigningKey::parse("/abs/key").unwrap(),
        super::SigningKey::File(_)
    ));
    assert!(matches!(
        super::SigningKey::parse("key::ssh-ed25519 AAAA").unwrap(),
        super::SigningKey::Literal(_)
    ));
}

/// The audit log names the agent behind a request; a subagent's commits are
/// signed through the same command as the main agent's.
#[test]
fn the_requester_is_the_main_agent_or_a_named_subagent() {
    use aither_core::llm::tool::{AgentIdentity, CallIdentity, SubagentIdentity, ToolCallId};
    let call = |agent| CallIdentity::new(ToolCallId::new("c1"), agent);
    assert_eq!(super::requester(Some(&call(AgentIdentity::Main))), "main");
    let subagent = AgentIdentity::Subagent(SubagentIdentity::new("explore", None));
    assert_eq!(super::requester(Some(&call(subagent))), "subagent explore");
}

/// A commit the host cannot sign fails; no unsigned commit is made.
#[tokio::test(flavor = "multi_thread")]
async fn a_host_that_cannot_sign_fails_the_commit() {
    let s = Session::new().await;
    std::fs::remove_file(s.tmp.path().join("user/id_ed25519")).unwrap();

    let error = s
        .run("echo one > one.txt && git add . && git commit -q -m one")
        .await
        .unwrap_err();

    assert!(error.to_string().contains("could not sign"), "{error}");
    assert_eq!(
        s.run("git rev-list --count HEAD").await.unwrap().trim(),
        "1"
    );
}

fn commit_buffer(author: &str, committer: &str, extra: &str) -> Vec<u8> {
    format!(
        "tree 4b825dc642cb6eb9a060e54bf8d69288fbee4904\nparent {}\nauthor {author} 1700000000 +0000\ncommitter {committer} 1700000000 +0000\n{extra}\nmessage\n",
        "a".repeat(40)
    )
    .into_bytes()
}

/// The harness signs a commit object of the user's and nothing else.
#[tokio::test(flavor = "multi_thread")]
async fn the_harness_signs_only_the_users_commit_objects() {
    let tmp = canonical_tempdir();
    let user = TestUser::new(&tmp.path().join("user"));
    let signing = user.signing(tmp.path(), "s1");
    let me = format!("{NAME} <{EMAIL}>");

    let signature = signing
        .sign(commit_buffer(&me, &me, ""), "main")
        .await
        .unwrap();
    assert!(signature.starts_with("-----BEGIN SSH SIGNATURE-----"));

    let refused = |result: Result<String, SigningError>| match result {
        Err(SigningError::Refused(reason)) => reason,
        other => panic!("expected a refusal, got {other:?}"),
    };
    let other = "Someone Else <else@example.com>";
    for buffer in [
        commit_buffer(other, &me, ""),
        commit_buffer(&me, other, ""),
        commit_buffer(&me, &me, "gpgsig -----BEGIN SSH SIGNATURE-----\n"),
        b"arbitrary data".to_vec(),
        // A tag object.
        format!(
            "object {}\ntype commit\ntag v1\ntagger {me} 1700000000 +0000\n\nrelease\n",
            "a".repeat(40)
        )
        .into_bytes(),
        // A commit whose tree is not an object id.
        b"tree nothex\nauthor x <x> 1 +0000\ncommitter x <x> 1 +0000\n\nm\n".to_vec(),
        vec![0xff, 0xfe],
    ] {
        refused(signing.sign(buffer, "main").await);
    }
    // Only the first signing is on record.
    let log = std::fs::read_to_string(signing.audit_log()).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
}
