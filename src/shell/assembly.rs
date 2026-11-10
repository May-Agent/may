//! The one place a session's shell is assembled: the sandbox it runs in, the
//! policies it checks scripts with and the commands its scripts call back.

use std::path::PathBuf;
use std::sync::Arc;

use aither_connectivity::Connectivity;
use aither_core::AnyIdAllocator;
use aither_core::llm::Approver;
use aither_sandbox::{
    Backend, ContainerShellRuntime, DomainApprover, Grants, Interpreter, Sandbox,
    Shell,
    ShellSessionRegistry, SshServer, SshServerError, TerminalError,
};
use heel::HeelBinary;

use super::{
    HostSession, MayShellWording, SessionEnvironmentError, ShellBudgets, approval::ServerApproval,
    shell_tool_settings,
};
use crate::agent::command_env::CommandEnv;
use crate::command_policy::PolicySet;
use crate::agent::subagent_spaces::SessionSubagentSpaces;
use crate::ipc::{CommandBridge, CommandRegistry, SessionShell, ShellSlot};
use crate::jobserver::JobConnection;
use crate::tools::{ToolEnvironments, ToolRuntimeError};

/// Why a session's shell could not be assembled.
#[derive(Debug, thiserror::Error)]
pub enum ShellAssemblyError {
    /// The session's environment could not be prepared.
    #[error(transparent)]
    Environment(#[from] SessionEnvironmentError),
    /// The home environment's tools are not what the backend needs.
    #[error(transparent)]
    Tools(#[from] ToolRuntimeError),
    /// The `heel` of the host is missing or not the linked version.
    #[error("the host's heel cannot be used: {0}")]
    Heel(#[from] heel::Error),
    /// The home environment's `heel` was not prepared.
    #[error("the home environment's tools carry no heel")]
    NoHeel,
    /// An SSH server of the session is not valid.
    #[error(transparent)]
    Server(#[from] SshServerError),
    /// The shell could not be built.
    #[error("building the shell failed: {0}")]
    Build(#[from] TerminalError),
}

/// Everything one session's shell is built from.
///
/// The daemon builds one per agent; a one-shot run, a scheduled task and a
/// repository clone build theirs from the same fields.
#[derive(Debug)]
pub struct ShellAssembly<P> {
    /// The home environment's prepared tools: where the shell runs, and what
    /// prepares an SSH server when its first command names it.
    pub tools: ToolEnvironments,
    /// The container the session works in; none for the host.
    pub container: Option<ContainerShellRuntime>,
    /// The directory scripts start in and the session's outputs live in.
    pub working_dir: PathBuf,
    /// The session's live grants, which the workspace moves while it runs.
    pub grants: Grants,
    /// The scheduling priority of every command.
    pub priority: heel::ProcessPriority,
    /// Where every network decision of a host command is recorded.
    pub audit_log: heel::NetworkAuditLog,
    /// What approves unsandboxed scripts, SSH servers and connections.
    pub permissions: P,
    /// What every command's environment holds beyond the sandbox's own; none
    /// for a run that carries no session secrets.
    pub env_source: Option<CommandEnv>,
    /// The handles the config moves while the session runs.
    pub budgets: ShellBudgets,
    /// The languages the shell runs scripts in.
    pub interpreters: Vec<Interpreter>,
    /// May's rules on scripts.
    pub policies: PolicySet,
    /// The commands scripts call back.
    pub commands: CommandRegistry,
    /// The slot the finished shell is bound into for the commands to reach.
    pub slot: ShellSlot,
    /// The SSH servers of the session.
    pub ssh_servers: Vec<SshServer>,
    /// The ids every name the model sees is leased from; a shell given none
    /// leases from its own.
    pub ids: Option<AnyIdAllocator>,
    /// The spaces subagents work in; none for a shell that launches no
    /// subagent.
    pub spaces: Option<SessionSubagentSpaces>,
    /// The machine's job pool the commands draw from; none when the daemon
    /// runs no pool or the backend cannot reach it.
    pub pool: Option<JobConnection>,
    /// What tells a connection outage from a failing command.
    pub connectivity: Option<Connectivity>,
}

impl<P> ShellAssembly<P>
where
    P: Approver + DomainApprover + Clone + 'static,
{
    /// The shell of the session, bound into its slot.
    ///
    /// # Errors
    /// Fails when the environment cannot be prepared, the host `heel` cannot
    /// be verified, a server is invalid or the shell cannot be built.
    pub async fn build(self) -> Result<Arc<SessionShell>, ShellAssemblyError> {
        self.build_with_sandbox().await.map(|(shell, _)| shell)
    }

    /// The shell of the session, bound into its slot, and the sandbox it
    /// spawns in, for processes of the session that are not scripts (a
    /// connected ACP agent and its commands).
    ///
    /// # Errors
    /// As [`build`](Self::build).
    pub async fn build_with_sandbox(
        self,
    ) -> Result<(Arc<SessionShell>, Sandbox), ShellAssemblyError> {
        let sandbox = self.sandbox().await?;
        let spawner = sandbox.clone();
        let sessions = ShellSessionRegistry::default();
        for server in &self.ssh_servers {
            sessions.add_ssh_server(server.clone(), self.tools.clone())?;
        }
        let builder = Shell::builder(sandbox)
            .interpreters(self.interpreters)
            .limits(self.budgets.limits())
            .output(self.budgets.output())
            .wording(MayShellWording)
            .tools(shell_tool_settings(&self.budgets))
            .policy(ServerApproval::new(sessions.clone()))
            .ipc(CommandBridge::new(self.commands, self.slot.clone()));
        let mut shell = self
            .policies
            .apply(builder)
            .build()
            .await?
            .with_approver(self.permissions)
            .with_shell_sessions(sessions);
        if let Some(ids) = self.ids {
            shell = shell.with_id_allocator(ids);
        }
        if let Some(spaces) = self.spaces {
            shell = shell.with_subagent_spaces(spaces);
        }
        if let Some(pool) = self.pool {
            shell = shell.with_jobserver_pool(pool.pool, pool.fifo_dir)?;
        }
        let shell = match &self.connectivity {
            Some(connectivity) => shell.with_connectivity(connectivity),
            None => shell,
        };
        let shell = Arc::new(shell);
        self.slot.bind(&shell);
        Ok((shell, spawner))
    }

    async fn sandbox(&self) -> Result<Sandbox, ShellAssemblyError> {
        let executor = executor_core::DefaultExecutor;
        let sandbox = match &self.container {
            Some(runtime) => Sandbox::new(
                Backend::Container {
                    runtime: runtime.clone(),
                    launch: self.tools.container_launch()?,
                },
                executor,
                &self.working_dir,
            ),
            None => {
                let tools = self.tools.local_tools()?;
                let heel = tools.heel().ok_or(ShellAssemblyError::NoHeel)?;
                let path = heel.path().to_path_buf();
                let heel = blocking::unblock(move || HeelBinary::verify(path)).await?;
                let session = HostSession::prepare(&self.working_dir, tools).await?;
                Sandbox::new(
                    Backend::Host {
                        heel,
                        launch: self.tools.host_launch()?,
                    },
                    executor,
                    &self.working_dir,
                )
                .with_fixed_grants(session.grants)
                .with_environment(session.environment)
                .with_write_denylist(session.write_denylist)
                .with_audit_log(self.audit_log.clone())
            }
        };
        let sandbox = match &self.env_source {
            Some(source) => sandbox.with_env_source(source.clone()),
            None => sandbox,
        };
        Ok(sandbox
            .with_grants(self.grants.clone())
            .with_domain_approver(self.permissions.clone())
            .with_priority(self.priority))
    }
}
