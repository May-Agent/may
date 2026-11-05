//! Signing of the commits an agent makes in its session sandbox.
//!
//! The sandbox's `HOME` is the session directory, so git there sees none of
//! the user's configuration. May writes the git configuration the sandbox
//! reads ([`CommitSigning::install_sandbox_git`]): the user's identity,
//! `commit.gpgsign`, and a `gpg.ssh.program` that hands every buffer git wants
//! signed to the [`SignCommitTool`] IPC command. The harness signs it on the
//! host with the user's own signing configuration, so the key never enters the
//! sandbox and nothing is rewritten after the fact.
//!
//! The harness signs a buffer only when it is a well-formed git commit object
//! whose author and committer are the user; it never runs git in the clone
//! and never runs anything the clone configures.
//!
//! [`CommitSigning::verify_range`] is the other half: `pr` refuses a range
//! with a commit that carries no valid signature.

use std::borrow::Cow;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use aither_core::llm::{Tool, ToolContext, ToolResult, tool::CallIdentity};
use aither_sandbox::MountRoot;
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::host_git::{HostGit, HostGitError};

#[cfg(test)]
#[path = "commit_signing_test_user.rs"]
pub(crate) mod test_user;
#[cfg(test)]
#[path = "commit_signing_tests.rs"]
mod tests;

/// Directory of the signer program under the sandbox's `HOME`.
const SIGNER_DIR: &str = ".config/may";

/// File name of the signer program.
const SIGNER_FILE: &str = "ssh-sign";

/// The IPC command the sandbox's signer program calls.
pub const SIGN_COMMAND: &str = "sign-commit";

/// Directory under `~/.may` of the signing audit log.
pub const AUDIT_LOG_SUBDIR: &str = "logs/signing";

/// File of the audit log: one JSON record per line.
pub const AUDIT_LOG_FILE: &str = "commits.jsonl";

/// How long the user's signing program may take, a prompt for a passphrase
/// or a touch included.
const SIGN_TIMEOUT: Duration = Duration::from_secs(120);

/// Why a buffer was not signed, or a range not verified.
#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    /// The host's git configuration lacks what signing as the user needs.
    #[error("the host's git configuration has no `{0}`, so a commit cannot be made as the user")]
    MissingConfig(&'static str),
    /// The harness signs commits authored as the user and nothing else.
    #[error("refusing to sign: {0}")]
    Refused(String),
    /// The user's signing program failed; nothing was signed.
    #[error("the host could not sign: {0}")]
    Host(String),
    /// The signing could not be recorded, so it is not returned.
    #[error("the signing audit log {path} could not be written: {reason}")]
    Audit {
        /// The log.
        path: PathBuf,
        /// What failed.
        reason: String,
    },
    /// A commit in a range carries no valid signature.
    #[error(
        "commit {commit} has {status}: every commit of a PR must be signed by the user's key. \
         Make commits with the sandbox's git, which signs them, and re-create ones made \
         elsewhere with `git rebase --exec true <base>` or `git commit --amend --no-edit`"
    )]
    Unsigned {
        /// The commit.
        commit: String,
        /// What the verification found, in words.
        status: &'static str,
    },
    /// A git invocation on the host failed.
    #[error("git {args} failed: {stderr}")]
    Git {
        /// The command line.
        args: String,
        /// Captured stderr.
        stderr: String,
    },
    /// Filesystem or process failure.
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// The sandbox git configuration could not be rendered.
    #[error("rendering the sandbox git configuration: {0}")]
    Render(#[from] askama::Error),
    /// The sandbox directory is not under the container's mount.
    #[error("the container does not mount the sandbox directory: {0}")]
    Mount(#[from] aither_sandbox::MountRootError),
}

impl From<HostGitError> for SigningError {
    fn from(error: HostGitError) -> Self {
        match error {
            HostGitError::Io(error) => Self::Io(error),
            HostGitError::Failed { args, stderr } => Self::Git { args, stderr },
        }
    }
}

/// `key` from the host's configuration, which signing as the user needs.
fn required(host: &HostGit, key: &'static str) -> Result<String, SigningError> {
    host.config_value(key)?
        .filter(|value| !value.is_empty())
        .ok_or(SigningError::MissingConfig(key))
}

/// Who the user is to git.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Identity {
    name: String,
    email: String,
}

/// The key the user's signing configuration names.
#[derive(Debug, Clone)]
enum SigningKey {
    /// `user.signingkey` is a public key itself (`key::` or `ssh-…`): the
    /// ssh-agent holds the private half.
    Literal(String),
    /// `user.signingkey` is a key file.
    File(PathBuf),
}

impl SigningKey {
    /// Reads `user.signingkey` the way git does.
    fn parse(value: &str) -> Result<Self, SigningError> {
        if let Some(key) = value.strip_prefix("key::") {
            return Ok(Self::Literal(key.to_string()));
        }
        if value.starts_with("ssh-") {
            return Ok(Self::Literal(value.to_string()));
        }
        let path = match value.strip_prefix("~/") {
            Some(rest) => dirs::home_dir()
                .ok_or_else(|| {
                    SigningError::Host(format!(
                        "user.signingkey {value} is under `~`, and the host has no home directory"
                    ))
                })?
                .join(rest),
            None => PathBuf::from(value),
        };
        Ok(Self::File(path))
    }

    /// The public half as `<type> <base64>`.
    fn public(&self) -> Result<String, SigningError> {
        let text = match self {
            Self::Literal(key) => key.clone(),
            Self::File(path) => {
                let sibling = {
                    let mut name = path.clone().into_os_string();
                    name.push(".pub");
                    PathBuf::from(name)
                };
                let content =
                    std::fs::read_to_string(&sibling).or_else(|_| std::fs::read_to_string(path))?;
                if content.starts_with("-----BEGIN") {
                    let output = Command::new("ssh-keygen")
                        .args(["-y", "-f"])
                        .arg(path)
                        .stdin(Stdio::null())
                        .output()?;
                    if !output.status.success() {
                        return Err(SigningError::Host(format!(
                            "the public key of {} could not be derived: {}",
                            path.display(),
                            String::from_utf8_lossy(&output.stderr).trim()
                        )));
                    }
                    String::from_utf8_lossy(&output.stdout).into_owned()
                } else {
                    content
                }
            }
        };
        let mut fields = text.split_whitespace();
        match (fields.next(), fields.next()) {
            (Some(kind), Some(data)) => Ok(format!("{kind} {data}")),
            _ => Err(SigningError::Host(
                "user.signingkey is not an ssh public key".to_string(),
            )),
        }
    }
}

/// The user's signing configuration on the host.
#[derive(Debug, Clone)]
struct HostSigner {
    identity: Identity,
    /// `gpg.ssh.program`, `ssh-keygen` by default.
    program: String,
    key: SigningKey,
}

impl HostSigner {
    fn read(host: &HostGit) -> Result<Self, SigningError> {
        Ok(Self {
            identity: Identity {
                name: required(host, "user.name")?,
                email: required(host, "user.email")?,
            },
            program: host
                .config_value("gpg.ssh.program")?
                .filter(|program| !program.is_empty())
                .unwrap_or_else(|| "ssh-keygen".to_string()),
            key: SigningKey::parse(&required(host, "user.signingkey")?)?,
        })
    }

    /// Signs `buffer` exactly as git asks `gpg.ssh.program` to:
    /// `<program> -Y sign -n git -f <key> [-U] <file>`, the signature in
    /// `<file>.sig`.
    fn sign(&self, buffer: &[u8]) -> Result<String, SigningError> {
        let dir = tempfile::Builder::new().prefix("may-sign-").tempdir()?;
        let file = dir.path().join("buffer");
        std::fs::write(&file, buffer)?;
        let mut command = Command::new(&self.program);
        command
            .current_dir(dir.path())
            .args(["-Y", "sign", "-n", "git", "-f"]);
        match &self.key {
            SigningKey::File(path) => {
                command.arg(path);
            }
            SigningKey::Literal(key) => {
                let key_file = dir.path().join("key.pub");
                std::fs::write(&key_file, key)?;
                command.arg(&key_file).arg("-U");
            }
        }
        command.arg(&file);
        let stderr = run_bounded(command, SIGN_TIMEOUT).map_err(SigningError::Host)?;
        let mut signature = file.into_os_string();
        signature.push(".sig");
        let signature = std::fs::read_to_string(&signature).map_err(|error| {
            SigningError::Host(format!(
                "`{}` wrote no signature ({error}): {stderr}",
                self.program
            ))
        })?;
        if signature.starts_with("-----BEGIN SSH SIGNATURE-----") {
            Ok(signature)
        } else {
            Err(SigningError::Host(format!(
                "`{}` wrote something other than an ssh signature",
                self.program
            )))
        }
    }
}

/// Runs `command` to completion within `deadline`, stdin closed. Returns its
/// stderr, or the reason it failed.
fn run_bounded(mut command: Command, deadline: Duration) -> Result<String, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            format!(
                "`{}` did not start: {error}",
                command.get_program().to_string_lossy()
            )
        })?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            break status;
        }
        if started.elapsed() > deadline {
            child.kill().ok();
            child.wait().ok();
            return Err(format!("it did not finish within {deadline:?}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        std::io::Read::read_to_string(&mut pipe, &mut stderr).ok();
    }
    if status.success() {
        Ok(stderr.trim().to_string())
    } else {
        Err(format!(
            "`{}` failed: {}",
            command.get_program().to_string_lossy(),
            stderr.trim()
        ))
    }
}

/// A git commit object, parsed far enough to know whose it is.
#[derive(Debug, PartialEq, Eq)]
struct CommitObject<'a> {
    tree: &'a str,
    parents: Vec<&'a str>,
    author: Identity,
    committer: Identity,
}

fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `Name <email> <seconds> <±hhmm>`.
fn parse_identity(line: &str) -> Option<Identity> {
    let mut parts = line.rsplitn(3, ' ');
    let zone = parts.next()?;
    let seconds = parts.next()?;
    let who = parts.next()?;
    let zone_ok = zone.len() == 5
        && matches!(zone.as_bytes()[0], b'+' | b'-')
        && zone[1..].bytes().all(|b| b.is_ascii_digit());
    if !zone_ok || seconds.is_empty() || !seconds.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let who = who.strip_suffix('>')?;
    let (name, email) = who.rsplit_once('<')?;
    let name = if name.is_empty() {
        name
    } else {
        name.strip_suffix(' ')?
    };
    Some(Identity {
        name: name.to_string(),
        email: email.to_string(),
    })
}

impl<'a> CommitObject<'a> {
    /// Parses the buffer git signs: a commit object without its signature.
    fn parse(buffer: &'a str) -> Result<Self, SigningError> {
        let refuse = |reason: &str| SigningError::Refused(format!("not a commit object: {reason}"));
        let (headers, _message) = buffer
            .split_once("\n\n")
            .ok_or_else(|| refuse("no blank line ends its headers"))?;
        let mut lines = headers.split('\n').peekable();
        let tree = lines
            .next()
            .and_then(|line| line.strip_prefix("tree "))
            .filter(|id| is_object_id(id))
            .ok_or_else(|| refuse("it does not begin with `tree <id>`"))?;
        let mut parents = Vec::new();
        while let Some(id) = lines.peek().and_then(|line| line.strip_prefix("parent ")) {
            if !is_object_id(id) {
                return Err(refuse("a parent is not an object id"));
            }
            parents.push(id);
            lines.next();
        }
        let mut identity = |role: &str| {
            lines
                .next()
                .and_then(|line| line.strip_prefix(role))
                .and_then(parse_identity)
                .ok_or_else(|| refuse(&format!("`{}` is missing or malformed", role.trim())))
        };
        let author = identity("author ")?;
        let committer = identity("committer ")?;
        // What follows is `encoding` and `mergetag` (whose continuation
        // lines begin with a space). A `gpgsig` means the buffer is already
        // a signed commit, which is not what git asks to have signed.
        let mut in_mergetag = false;
        for line in lines {
            if line.starts_with(' ') && in_mergetag {
                continue;
            }
            in_mergetag = line.starts_with("mergetag ");
            if !in_mergetag && !line.starts_with("encoding ") {
                return Err(refuse("it has a header a commit to sign does not"));
            }
        }
        Ok(Self {
            tree,
            parents,
            author,
            committer,
        })
    }
}

/// One signing, as the audit log records it.
#[derive(Debug, Serialize)]
struct AuditRecord<'a> {
    time: String,
    session: &'a str,
    agent: &'a str,
    tree: &'a str,
    parents: &'a [&'a str],
    identity: String,
    /// SHA-256 of the signed buffer, which is the commit object without its
    /// signature: its git hash is only known once the signature is in.
    buffer_sha256: String,
}

/// Renders the sandbox's git configuration.
#[derive(Template)]
#[template(path = "sandbox_gitconfig", escape = "none")]
struct SandboxGitConfig {
    /// The user's identity, when the host has one.
    identity: Option<Identity>,
    /// `key::<public key>`, the key the harness signs with.
    signing_key: String,
    /// The signer program, as the sandbox spells its path.
    program: String,
}

/// Template filters.
#[allow(
    clippy::inline_always,
    clippy::unused_self,
    clippy::unnecessary_wraps,
    clippy::missing_docs_in_private_items
)]
mod filters {
    /// A git config value in double quotes, `\` and `"` escaped, so it parses
    /// back as written whatever it holds (`#`, `;` included).
    #[askama::filter_fn]
    pub fn git_quoted(
        value: impl std::fmt::Display,
        _: &dyn askama::Values,
    ) -> askama::Result<String> {
        let value = value.to_string();
        Ok(format!(
            "\"{}\"",
            value.replace('\\', "\\\\").replace('"', "\\\"")
        ))
    }
}

/// Renders the signer program git calls in the sandbox.
#[derive(Template)]
#[template(path = "git_ssh_sign.sh", escape = "none")]
struct SignerScript {
    command: &'static str,
}

/// The harness: signs the user's commits for one session's sandbox, records
/// each signing, and verifies the signatures of a range.
#[derive(Debug, Clone)]
pub struct CommitSigning {
    host: HostGit,
    audit_log: PathBuf,
    session_id: String,
}

impl CommitSigning {
    /// The harness of `session_id`, signing with the user's configuration in
    /// `host` and appending to the audit log under `may_dir`.
    #[must_use]
    pub fn new(host: HostGit, may_dir: &Path, session_id: impl Into<String>) -> Self {
        Self {
            host,
            audit_log: may_dir.join(AUDIT_LOG_SUBDIR).join(AUDIT_LOG_FILE),
            session_id: session_id.into(),
        }
    }

    /// The host git the user's configuration is read through: every host git
    /// `pr` runs goes through it.
    #[must_use]
    pub const fn host(&self) -> &HostGit {
        &self.host
    }

    /// The file the audit log is appended to.
    #[must_use]
    pub fn audit_log(&self) -> &Path {
        &self.audit_log
    }

    /// Signs `buffer` for `requester` when it is a commit object authored and
    /// committed by the user, and returns the ssh signature git stores in the
    /// commit.
    ///
    /// # Errors
    /// Fails when the buffer is anything else, when the host cannot sign, or
    /// when the signing cannot be recorded.
    pub async fn sign(&self, buffer: Vec<u8>, requester: &str) -> Result<String, SigningError> {
        let this = self.clone();
        let requester = requester.to_string();
        blocking::unblock(move || this.sign_blocking(&buffer, &requester)).await
    }

    fn sign_blocking(&self, buffer: &[u8], requester: &str) -> Result<String, SigningError> {
        let text = std::str::from_utf8(buffer)
            .map_err(|_| SigningError::Refused("not a commit object: it is not UTF-8".into()))?;
        let commit = CommitObject::parse(text)?;
        let signer = HostSigner::read(&self.host)?;
        for (role, who) in [("author", &commit.author), ("committer", &commit.committer)] {
            if *who != signer.identity {
                return Err(SigningError::Refused(format!(
                    "the commit's {role} is {} <{}>, not the user ({} <{}>)",
                    who.name, who.email, signer.identity.name, signer.identity.email
                )));
            }
        }
        let signature = signer.sign(buffer)?;
        self.record(&AuditRecord {
            time: jiff::Timestamp::now().to_string(),
            session: &self.session_id,
            agent: requester,
            tree: commit.tree,
            parents: &commit.parents,
            identity: format!("{} <{}>", signer.identity.name, signer.identity.email),
            buffer_sha256: hex::encode(Sha256::digest(buffer)),
        })?;
        Ok(signature)
    }

    fn record(&self, record: &AuditRecord<'_>) -> Result<(), SigningError> {
        let audit = |reason: String| SigningError::Audit {
            path: self.audit_log.clone(),
            reason,
        };
        if let Some(parent) = self.audit_log.parent() {
            std::fs::create_dir_all(parent).map_err(|e| audit(e.to_string()))?;
        }
        let mut line = serde_json::to_string(record).map_err(|e| audit(e.to_string()))?;
        line.push('\n');
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_log)
            .and_then(|mut file| file.write_all(line.as_bytes()))
            .map_err(|e| audit(e.to_string()))
    }

    /// Writes the git configuration and signer program the sandbox rooted at
    /// `sandbox_dir` (its `HOME`) reads, from the user's current host
    /// configuration. Without a usable host identity or key the configuration
    /// still demands signing, so a commit fails with the harness's reason
    /// instead of going out unsigned.
    ///
    /// # Errors
    /// Fails when the files cannot be written, or when a container's mount
    /// does not hold `sandbox_dir`.
    ///
    /// `container` is the mount of `sandbox_dir` into the session's
    /// container, whose `HOME` is that mount: the configuration then names
    /// the signer by its path inside the container.
    pub async fn install_sandbox_git(
        &self,
        sandbox_dir: &Path,
        container: Option<&MountRoot>,
    ) -> Result<(), SigningError> {
        async_fs::create_dir_all(sandbox_dir).await?;
        let program_dir = sandbox_dir.join(SIGNER_DIR);
        let program = program_dir.join(SIGNER_FILE);
        let seen_by_git = match container {
            Some(mount) => mount.container_path(&program)?.as_str().to_string(),
            None => async_fs::canonicalize(sandbox_dir)
                .await?
                .join(SIGNER_DIR)
                .join(SIGNER_FILE)
                .to_string_lossy()
                .into_owned(),
        };
        let config = self.sandbox_git_config(&seen_by_git).await?;
        let script = SignerScript {
            command: SIGN_COMMAND,
        }
        .render()?;
        async_fs::create_dir_all(&program_dir).await?;
        async_fs::write(&program, script).await?;
        {
            use std::os::unix::fs::PermissionsExt as _;
            async_fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).await?;
        }
        async_fs::write(sandbox_dir.join(".gitconfig"), config).await?;
        Ok(())
    }

    /// The sandbox's git configuration for the user now on the host, naming
    /// the signer at `program`.
    async fn sandbox_git_config(&self, program: &str) -> Result<String, SigningError> {
        let host = self.host.clone();
        let (identity, signing_key) = blocking::unblock(move || {
            let signer = HostSigner::read(&host);
            match signer.and_then(|s| Ok((s.key.public()?, s.identity))) {
                Ok((public, identity)) => (Some(identity), format!("key::{public}")),
                Err(error) => {
                    tracing::warn!(%error, "no usable git signing configuration on the host");
                    (None, "key::".to_string())
                }
            }
        })
        .await;
        Ok(SandboxGitConfig {
            identity,
            signing_key,
            program: program.to_string(),
        }
        .render()?)
    }

    /// Checks that every commit of `base..head` in `repo`, a repository the
    /// host trusts, carries a signature valid under the user's key.
    ///
    /// # Errors
    /// Fails on the first commit without one, or when the user's key cannot
    /// be read.
    pub async fn verify_range(
        &self,
        repo: PathBuf,
        base: String,
        head: String,
    ) -> Result<(), SigningError> {
        let this = self.clone();
        blocking::unblock(move || this.verify_range_blocking(&repo, &base, &head)).await
    }

    /// [`Self::verify_range`] for a caller already off the event loop.
    ///
    /// # Errors
    /// As [`Self::verify_range`].
    pub fn verify_range_blocking(
        &self,
        repo: &Path,
        base: &str,
        head: &str,
    ) -> Result<(), SigningError> {
        let signer = HostSigner::read(&self.host)?;
        let dir = tempfile::Builder::new().prefix("may-signers-").tempdir()?;
        let signers = dir.path().join("allowed_signers");
        std::fs::write(
            &signers,
            format!(
                "{} namespaces=\"git\" {}\n",
                signer.identity.email,
                signer.key.public()?
            ),
        )?;
        let output = self
            .host
            .command()
            .arg("-C")
            .arg(repo)
            .args([
                "-c",
                "gpg.format=ssh",
                "-c",
                "gpg.ssh.program=ssh-keygen",
                "-c",
            ])
            .arg(format!("gpg.ssh.allowedSignersFile={}", signers.display()))
            .args(["log", "--format=%H %G?", &format!("{base}..{head}")])
            .output()?;
        if !output.status.success() {
            return Err(SigningError::Git {
                args: format!("log {base}..{head}"),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
            });
        }
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            let (commit, status) = line.split_once(' ').unwrap_or((line, ""));
            let status = match status {
                "G" => continue,
                "N" => "no signature",
                "B" => "a bad signature",
                "U" | "X" | "Y" | "R" | "E" => "a signature that is not the user's key",
                _ => "an unreadable signature",
            };
            return Err(SigningError::Unsigned {
                commit: commit.to_string(),
                status,
            });
        }
        Ok(())
    }
}

/// `sign-commit` — the IPC command behind the sandbox's `gpg.ssh.program`.
#[derive(Debug, Clone)]
pub struct SignCommitTool {
    signing: CommitSigning,
}

impl SignCommitTool {
    /// The command of one session's harness.
    #[must_use]
    pub const fn new(signing: CommitSigning) -> Self {
        Self { signing }
    }
}

/// Arguments of `sign-commit`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SignCommitArgs {
    /// The commit object to sign, on stdin. git's signer program feeds it;
    /// the signature is printed.
    #[serde(default)]
    input: Option<String>,
}

/// Who asked, for the audit log.
fn requester(identity: Option<&CallIdentity>) -> String {
    use aither_core::llm::tool::AgentIdentity;
    match identity.map(CallIdentity::agent) {
        Some(AgentIdentity::Main) => "main".to_string(),
        Some(AgentIdentity::Subagent(subagent)) => format!("subagent {}", subagent.name()),
        None => "unknown".to_string(),
    }
}

impl Tool for SignCommitTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed(SIGN_COMMAND)
    }

    type Arguments = SignCommitArgs;
    type Res = ToolResult;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> aither_core::Result<Self::Res> {
        let buffer = args
            .input
            .ok_or_else(|| anyhow::anyhow!("no commit object on stdin"))?;
        let signature = self
            .signing
            .sign(buffer.into_bytes(), &requester(cx.identity()))
            .await?;
        Ok(ToolResult::text(signature))
    }
}
