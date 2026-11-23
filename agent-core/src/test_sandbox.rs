#![allow(clippy::disallowed_methods, reason = "test fixture sandbox setup")]
//! A session shell as May builds one, for `#[cfg(test)]` code: a
//! sandboxed terminal with the baseline tool distribution, plus helpers
//! for hostile repository configurations.

use std::path::{Path, PathBuf};

use std::sync::Arc;

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure};
use aither_sandbox::{DomainApprover, DomainRequest, Grants, Interpreter};

use crate::agent::command_env::CommandEnv;
use crate::repo_clone::CloneSandbox;
use may_harness::ipc::{CommandRegistryBuilder, SessionShell};

/// Runs sandboxed commands and refuses every unsandboxed one and every
/// connection.
#[derive(Debug, Clone, Copy)]
pub struct SandboxedOnly;

impl Approver for SandboxedOnly {
    fn approve(
        &self,
        _request: ApprovalRequest,
    ) -> impl Future<Output = Result<Approval, ApproverFailure>> + Send {
        std::future::ready(Ok(Approval::Denied {
            reason: "the test terminal runs sandboxed scripts only".to_string(),
        }))
    }
}

impl DomainApprover for SandboxedOnly {
    fn approve(&self, _request: &DomainRequest) -> impl Future<Output = bool> + Send {
        std::future::ready(false)
    }
}

/// A session shell working in `session_root`, as May builds one: the
/// root is its only writable place. Dropping it ends its sandbox.
pub struct TestTerminal {
    shell: Arc<SessionShell>,
    timeout: std::time::Duration,
    _audit: tempfile::TempDir,
}

impl std::fmt::Debug for TestTerminal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TestTerminal").finish_non_exhaustive()
    }
}

impl TestTerminal {
    /// A terminal in `session_root` serving the baseline commands.
    pub async fn new(session_root: &Path) -> Self {
        Self::configured(session_root, CommandRegistryBuilder::new(), None).await
    }

    /// A terminal as the daemon builds one for a session that signs: the
    /// sandbox's git configuration is installed and `sign-commit` is
    /// served by `signing`.
    ///
    /// # Panics
    /// Panics when the directory or git configuration cannot be written.
    pub async fn signing(
        session_root: &Path,
        signing: may_config::commit_signing::CommitSigning,
    ) -> Self {
        std::fs::create_dir_all(session_root).unwrap();
        signing
            .install_sandbox_git(session_root, None)
            .await
            .unwrap();
        let mut commands = CommandRegistryBuilder::new();
        commands.configure_tool(may_config::commit_signing::SignCommitTool::new(signing));
        Self::configured(session_root, commands, None).await
    }

    /// A terminal whose scripts can call the commands `commands` holds.
    pub async fn with_commands(session_root: &Path, commands: CommandRegistryBuilder) -> Self {
        Self::configured(session_root, commands, None).await
    }

    /// A terminal whose commands see the environment `env` supplies.
    pub async fn with_env(session_root: &Path, env: CommandEnv) -> Self {
        Self::configured(session_root, CommandRegistryBuilder::new(), Some(env)).await
    }

    async fn configured(
        session_root: &Path,
        commands: CommandRegistryBuilder,
        env_source: Option<CommandEnv>,
    ) -> Self {
        // A shell spawns its job registry on the global executor, which
        // the `may` binary installs at startup.
        may_protocol::test_support::install_global_executor();
        let distribution =
            may_tool_dist::embedded_distribution().expect("embedded distribution validates");
        let cache = may_tool_dist::test_tool_cache();
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::create_dir_all(session_root).unwrap();
        let tools = may_tool_dist::ToolEnvironments::prepare_host(&distribution, &cache, false)
            .await
            .expect("the baseline tools prepare");
        let audit = tempfile::tempdir().expect("an audit directory");
        let audit_log = aither_sandbox::open_network_audit_log(audit.path(), 1)
            .await
            .expect("the audit log opens");
        let grants = Grants::new();
        grants.add_writable(session_root);
        let settings = may_config::config::ShellSettings::default();
        let shell = crate::shell::ShellAssembly {
            tools,
            container: None,
            working_dir: session_root.to_path_buf(),
            grants,
            priority: heel::ProcessPriority::default(),
            audit_log,
            permissions: SandboxedOnly,
            env_source,
            budgets: crate::shell::ShellBudgets::of(&settings),
            // As a session's shell, whose tool definition a fork must
            // present unchanged.
            interpreters: vec![Interpreter::Shell, Interpreter::Python],
            policies: may_command_policy::PolicySet {
                managed_bash: may_command_policy::runs_managed_bash(false),
                refused: may_command_policy::RefusedReads::default(),
                checkout_reads: None,
            },
            slot: may_harness::ipc::ShellSlot::new(),
            ssh_servers: Vec::new(),
            ids: None,
            spaces: crate::agent::subagent_spaces::SessionSubagentSpaces::without_clones(),
            pool: None,
            connectivity: None,
        }
        .build(commands.build())
        .await
        .expect("the shell builds");
        Self {
            shell,
            timeout: settings.timeout_cap,
            _audit: audit,
        }
    }

    /// The session shell, as a command that launches work from it gets it.
    #[must_use]
    pub fn shell(&self) -> &SessionShell {
        &self.shell
    }

    /// Starts a background job that sleeps for a minute, and returns it
    /// with its pid.
    ///
    /// # Panics
    /// Panics when the job does not start.
    pub async fn sleeping_job(&self) -> (aither_sandbox::Job, u32) {
        let job = self
            .shell
            .run_background(aither_sandbox::ShellArgs {
                description: "sleeps".to_string(),
                script: "sleep 60".to_string(),
                interpreter: aither_sandbox::Interpreter::default(),
                sandbox: aither_sandbox::SandboxMode::Sandboxed,
                location: None,
                resolution: aither_sandbox::MediaResolution::default(),
                max_lines: None,
                raw: false,
                timeout: 120,
                background: true,
            })
            .await
            .expect("the job starts");
        let pid = job.info().await.pid;
        (job, pid)
    }

    /// The terminal's sandbox, as a shell-bound command gets it.
    #[must_use]
    pub fn sandbox(&self) -> CloneSandbox {
        CloneSandbox::new(SessionShell::clone(&self.shell), self.timeout)
    }
}

/// Arm `clone` with every executable setting a repository can carry.
///
/// Each setting is set the way an agent writing it could: each runs a script
/// that touches `host/<name>` (outside the sandbox) and `ran/<name>` inside
/// the session, so a marker in `host/` means it ran on the host. Returns the
/// `host/` directory.
///
/// # Panics
/// Panics when a directory or file cannot be written.
#[must_use]
pub fn arm_hostile_config(session_root: &Path, clone: &Path, outside: &Path) -> PathBuf {
    let host = outside.join("host");
    let ran = session_root.join("ran");
    let scripts = session_root.join("hostile");
    let hooks = scripts.join("hooks");
    for dir in [&host, &ran, &hooks] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let script = |path: &Path, name: &str, tail: &str| {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\ntouch '{}/{name}' 2>/dev/null\ntouch '{}/{name}' 2>/dev/null\n{tail}\n",
                host.display(),
                ran.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    };
    // Repository-local hooks, both where git looks by default and under
    // `core.hooksPath`.
    for hook in [
        "pre-commit",
        "prepare-commit-msg",
        "commit-msg",
        "post-commit",
        "post-checkout",
        "post-merge",
        "pre-push",
        "pre-receive",
        "update",
        "post-update",
        "push-to-checkout",
        "reference-transaction",
        "pre-auto-gc",
        "post-index-change",
    ] {
        script(&hooks.join(hook), hook, "exit 0");
        script(&clone.join(".git/hooks").join(hook), hook, "exit 0");
    }
    let mut settings = vec![(
        "core.hooksPath".to_string(),
        hooks.to_string_lossy().into_owned(),
    )];
    for (key, tail) in [
        // Answers "no fsmonitor data", so git falls back to a scan.
        ("core.fsmonitor", "exit 1"),
        ("filter.hostile.clean", "cat"),
        ("filter.hostile.smudge", "cat"),
        ("uploadpack.packObjectsHook", "exec \"$@\""),
        ("core.sshCommand", "exit 1"),
        ("core.alternateRefsCommand", "exit 0"),
        ("credential.helper", "exit 0"),
        ("core.pager", "cat"),
        ("core.editor", "exit 0"),
        ("diff.external", "exit 0"),
        ("gpg.program", "exit 1"),
    ] {
        let path = scripts.join(key);
        script(&path, key, tail);
        settings.push((key.to_string(), path.to_string_lossy().into_owned()));
    }
    settings.push(("filter.hostile.required".to_string(), "true".to_string()));
    for (key, value) in settings {
        may_config::host_git::HostGit::from_file("/dev/null")
            .output(Some(clone), &["config", &key, &value])
            .unwrap();
    }
    // `.git/info/attributes` applies the filter too, and leaves the
    // worktree clean so a reuse check still runs against the armed
    // configuration.
    std::fs::write(clone.join(".git/info/attributes"), "* filter=hostile\n").unwrap();
    host
}

/// Names of the markers in `dir`.
///
/// # Panics
/// Panics when `dir` cannot be read.
#[must_use]
pub fn markers(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}
