//! Host-side [`DebugBroker`] managing live DAP sessions, sandboxed debuggees,
//! real-PID `JobRegistry` integration, async `SessionPrompts` wakeups, and teardown.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure};
use aither_dap::{
    AnyDapTransport, ConnectMode, DEFAULT_EXECUTION_TIMEOUT, DapError, DapTransport, DebugArgs,
    DebugNotification, DebugResult, DebugSession, DebugSessionConfig, DebugSessionState,
    DebugStatus, DebugToolError, ExecutionOutcome, FrameReader, OutputBuffer, OutputCategory,
    OutputSnapshot, ProtocolMessage, StdioTransport, TcpTransport, TerminationSummary,
};
use aither_sandbox::{
    Interpreter, Job, JobRead, Jobs, MediaResolution, Pipe, SandboxMode, SandboxedChild,
    ServiceArgs, SpawnSpec, Stdio,
};
use askama::Template;
use async_channel::{Receiver, Sender};
use async_lock::Mutex;
use executor_core::tokio::TokioGlobal;
use futures_lite::io::{AsyncRead, BufReader, Cursor};
use serde_json::json;

use crate::agent::permission::UNSANDBOXED_KIND;
use crate::agent::pipe_tool::PriorJobs;
use crate::debug::adapter::{
    AdapterKind, resolve_native_adapter, select_adapter_for_attach, select_adapter_for_launch,
};
use crate::debug::process::{
    allocate_loopback_port, inspect_process_comm, parse_endpoint, resolve_debuggee_pid,
};
use crate::ipc::SessionShell;
use crate::task_wake::SessionPrompts;

/// How long `launch` waits for a `debugpy` loopback listener to accept TCP connections.
const DEBUGPY_READY_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll interval while waiting for a freshly spawned target or `debugpy` port to come up.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Identifies which agent owns an active debug session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DebugOwner {
    /// Main agent of a session (participates in `SessionPrompts`, `LiveResourcesContext`,
    /// `Moment::FinalTurn`, and session teardown).
    MainSession(String),
    /// Writable subagent scoped strictly to that subagent run's own `Shell` teardown.
    Subagent {
        /// Owning session ID (for session archive/delete cleanup).
        session_id: String,
        /// Unique identifier for the subagent run.
        subagent_key: String,
    },
}

impl DebugOwner {
    /// Session ID associated with this owner.
    #[must_use]
    pub fn session_id(&self) -> &str {
        match self {
            Self::MainSession(id) | Self::Subagent { session_id: id, .. } => id,
        }
    }

    /// Returns `Some(session_id)` only for main-session owners.
    #[must_use]
    pub fn main_session_id(&self) -> Option<&str> {
        match self {
            Self::MainSession(id) => Some(id),
            Self::Subagent { .. } => None,
        }
    }
}

/// Per-call runtime context handed from [`crate::debug::MayDebugTool`] into [`DebugBroker`].
#[derive(Clone)]
pub struct DebugCallContext {
    /// Owner slot (main session vs writable subagent).
    pub owner: DebugOwner,
    /// Calling agent's shell (`None` outside a terminal-backed runtime).
    pub shell: Option<SessionShell>,
    /// Working directory of the calling terminal process (`Caller.working_dir`).
    pub working_dir: Option<String>,
    /// Sandbox mode inherited from `Binding.launch.mode`.
    pub launch_mode: SandboxMode,
    /// Main-session prompt queue for one-shot async stop/termination wakeups.
    pub session_prompts: Option<SessionPrompts>,
    /// Prior shells' `Jobs` handles preserved across main-agent rebuilds.
    pub prior_jobs: Option<PriorJobs>,
}

impl std::fmt::Debug for DebugCallContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugCallContext")
            .field("owner", &self.owner)
            .field("working_dir", &self.working_dir)
            .field("launch_mode", &self.launch_mode)
            .finish_non_exhaustive()
    }
}

#[derive(Template)]
#[template(path = "debug_wake.txt", escape = "none")]
struct DebugWakeText<'a> {
    body: &'a str,
}

/// Approver installed ONLY when `Binding.launch.mode == SandboxMode::Unsandboxed`
/// so launching the target inside `Shell` inherits the already-approved outer
/// `unsafe` script without prompting `PermissionApprover` a second time.
#[derive(Debug, Clone, Copy)]
struct InheritedUnsandboxedApprover;

impl Approver for InheritedUnsandboxedApprover {
    fn approve(
        &self,
        request: ApprovalRequest,
    ) -> impl std::future::Future<Output = Result<Approval, ApproverFailure>> + Send {
        let decision = if request.kind().as_str() == UNSANDBOXED_KIND {
            Ok(Approval::Approved)
        } else {
            Ok(Approval::Denied {
                reason: "debug target spawn only inherits an already-approved unsandboxed mode"
                    .to_owned(),
            })
        };
        std::future::ready(decision)
    }
}

#[derive(Default)]
struct JobStdoutMirror {
    cursor: usize,
    buffer: OutputBuffer,
}

/// Holds a reference to the `JobRegistry` background job backing a launched or attached target.
#[derive(Clone)]
struct OwnedDebugJob {
    jobs: Jobs,
    job_pid: u32,
    /// Holds the job watch alive without terminating on drop (for `detach`)
    /// and reads target stdout captured by `JobRegistry`.
    job: Job,
    /// Incremental mirror of `JobRegistry` stdout so repeated `debug output`
    /// reads work identically to adapter-emitted `output` events.
    stdout_mirror: Arc<Mutex<JobStdoutMirror>>,
}

impl OwnedDebugJob {
    fn new(jobs: Jobs, job_pid: u32, job: Job) -> Self {
        Self {
            jobs,
            job_pid,
            job,
            stdout_mirror: Arc::new(Mutex::new(JobStdoutMirror::default())),
        }
    }
}

struct ActiveDebugSession {
    generation: u64,
    session: DebugSession,
    owned_job: Option<OwnedDebugJob>,
}

#[derive(Default)]
struct BrokerState {
    next_generation: u64,
    sessions: HashMap<DebugOwner, ActiveDebugSession>,
}

/// Daemon-wide host broker managing structured DAP sessions across all May sessions.
///
/// Cloning shares the same underlying broker state.
#[derive(Clone, Default)]
pub struct DebugBroker {
    state: Arc<Mutex<BrokerState>>,
}

impl std::fmt::Debug for DebugBroker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugBroker").finish_non_exhaustive()
    }
}

impl DebugBroker {
    /// Creates an empty debug broker.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes one [`DebugArgs`] command on behalf of `ctx.owner`.
    ///
    /// # Errors
    ///
    /// Returns [`DebugToolError`] when adapter discovery, target spawning,
    /// attachment, connection, or DAP execution fails.
    pub async fn handle(
        &self,
        ctx: DebugCallContext,
        args: DebugArgs,
    ) -> Result<DebugResult, DebugToolError> {
        match args {
            DebugArgs::Status => self.handle_status(&ctx.owner).await,
            DebugArgs::Launch {
                program,
                adapter,
                cwd,
                env,
                stop_on_entry,
                timeout_ms,
                args,
            } => {
                self.handle_launch(
                    ctx,
                    LaunchParams {
                        program,
                        adapter,
                        cwd,
                        env,
                        stop_on_entry,
                        timeout: timeout_ms
                            .map_or(DEFAULT_EXECUTION_TIMEOUT, Duration::from_millis),
                        args,
                    },
                )
                .await
            }
            DebugArgs::Attach {
                pid,
                adapter,
                timeout_ms,
            } => {
                self.handle_attach(
                    ctx,
                    pid,
                    adapter,
                    timeout_ms.map_or(DEFAULT_EXECUTION_TIMEOUT, Duration::from_millis),
                )
                .await
            }
            DebugArgs::Connect {
                endpoint,
                adapter,
                mode,
                program,
                cwd,
                stop_on_entry,
                timeout_ms,
                args,
            } => {
                self.handle_connect(
                    ctx,
                    ConnectParams {
                        endpoint,
                        adapter,
                        mode,
                        program,
                        cwd,
                        stop_on_entry,
                        timeout: timeout_ms
                            .map_or(DEFAULT_EXECUTION_TIMEOUT, Duration::from_millis),
                        args,
                    },
                )
                .await
            }
            DebugArgs::Detach => self.handle_detach(&ctx.owner).await,
            DebugArgs::Terminate => self.handle_terminate(&ctx.owner).await,
            other => self.forward_in_session(&ctx.owner, &other).await,
        }
    }

    /// Queries the live status and instance generation of the main session's
    /// active debug session, if any (for post-compaction inventory and turn-end checks).
    pub async fn active_session_status(&self, session_id: &str) -> Option<(u64, DebugStatus)> {
        let (generation, session) = {
            let guard = self.state.lock().await;
            let entry = guard
                .sessions
                .get(&DebugOwner::MainSession(session_id.to_owned()))?;
            let pair = (entry.generation, entry.session.clone());
            drop(guard);
            pair
        };
        let status = session.status().await.ok()?;
        Some((generation, status))
    }

    /// Terminates and removes all active debug sessions (main and subagent) belonging
    /// to `session_id` (used on session archive and delete).
    pub async fn teardown_session(&self, session_id: &str) {
        let removed: Vec<ActiveDebugSession> = {
            let mut guard = self.state.lock().await;
            let mut out = Vec::new();
            let mut kept = HashMap::new();
            for (owner, entry) in std::mem::take(&mut guard.sessions) {
                if owner.session_id() == session_id {
                    out.push(entry);
                } else {
                    kept.insert(owner, entry);
                }
            }
            guard.sessions = kept;
            drop(guard);
            out
        };
        for entry in removed {
            teardown_entry(entry).await;
        }
    }

    /// Terminates and removes the debug session owned by `owner` (used when a
    /// writable subagent finishes).
    pub async fn teardown_owner(&self, owner: &DebugOwner) {
        let removed = self.state.lock().await.sessions.remove(owner);
        if let Some(entry) = removed {
            teardown_entry(entry).await;
        }
    }

    /// Terminates every active debug session across all sessions (used on daemon shutdown).
    pub async fn shutdown_all(&self) {
        let removed: Vec<ActiveDebugSession> = {
            let mut guard = self.state.lock().await;
            guard.sessions.drain().map(|(_, entry)| entry).collect()
        };
        for entry in removed {
            teardown_entry(entry).await;
        }
    }

    async fn handle_status(&self, owner: &DebugOwner) -> Result<DebugResult, DebugToolError> {
        let session = {
            let guard = self.state.lock().await;
            guard.sessions.get(owner).map(|e| e.session.clone())
        };
        let Some(session) = session else {
            return Ok(DebugResult::Status(DebugStatus {
                state: DebugSessionState::Inactive,
                adapter: None,
                target_pid: None,
                target: None,
                breakpoint_count: 0,
                stop_location: None,
                stop_reason: None,
                exit_code: None,
            }));
        };
        session
            .handle_command(&DebugArgs::Status)
            .await
            .map_err(|err| DebugToolError::request_failed(err.to_string()))
    }

    async fn forward_in_session(
        &self,
        owner: &DebugOwner,
        args: &DebugArgs,
    ) -> Result<DebugResult, DebugToolError> {
        let (session, owned_job) = {
            let guard = self.state.lock().await;
            guard
                .sessions
                .get(owner)
                .map(|e| (e.session.clone(), e.owned_job.clone()))
        }
        .ok_or_else(|| {
            DebugToolError::request_failed(
                "no active debug session; start one with `debug launch`, `debug attach`, or `debug connect`",
            )
        })?;
        let mut result = session
            .handle_command(args)
            .await
            .map_err(|err| DebugToolError::request_failed(err.to_string()))?;
        if let (
            DebugArgs::Output {
                category,
                tail,
                clear,
            },
            DebugResult::Output(snapshot),
            Some(owned),
        ) = (args, &mut result, owned_job.as_ref())
        {
            supplement_job_stdout(snapshot, owned, category.unwrap_or_default(), *tail, *clear)
                .await;
        }
        Ok(result)
    }

    async fn handle_detach(&self, owner: &DebugOwner) -> Result<DebugResult, DebugToolError> {
        let entry = self
            .state
            .lock()
            .await
            .sessions
            .remove(owner)
            .ok_or_else(|| {
                DebugToolError::request_failed("no active debug session to detach from")
            })?;
        let status_before = entry.session.status().await.ok();
        let target_pid = entry
            .owned_job
            .as_ref()
            .map(|j| j.job_pid)
            .or_else(|| status_before.as_ref().and_then(|s| s.target_pid));
        // Detach leaves the owning background job running untouched in JobRegistry.
        let _ = entry.session.detach().await;
        #[cfg(target_os = "macos")]
        if status_before
            .as_ref()
            .is_some_and(|s| s.state == DebugSessionState::Stopped)
            && let Some(pid) = target_pid
        {
            // On XNU, detaching `lldb-dap` while stopped at a breakpoint can leave the
            // process suspended with `SIGSTOP`; resume it so background jobs keep running.
            let _ = async_process::Command::new("/bin/kill")
                .args(["-CONT", &pid.to_string()])
                .status()
                .await;
        }
        Ok(DebugResult::Detached { target_pid })
    }

    async fn handle_terminate(&self, owner: &DebugOwner) -> Result<DebugResult, DebugToolError> {
        let entry = self
            .state
            .lock()
            .await
            .sessions
            .remove(owner)
            .ok_or_else(|| {
                DebugToolError::request_failed("no active debug session to terminate")
            })?;
        let summary = entry
            .session
            .terminate()
            .await
            .unwrap_or_else(|_| TerminationSummary {
                exit_code: None,
                reason: Some("terminated".to_owned()),
            });
        if let Some(owned) = entry.owned_job {
            let _ = owned.jobs.kill(owned.job_pid).await;
        }
        Ok(DebugResult::Terminated(summary))
    }

    async fn handle_launch(
        &self,
        ctx: DebugCallContext,
        params: LaunchParams,
    ) -> Result<DebugResult, DebugToolError> {
        self.ensure_no_active_session(&ctx.owner).await?;
        let kind = select_adapter_for_launch(&params.program, params.adapter.as_deref())
            .map_err(|err| DebugToolError::request_failed(err.to_string()))?;

        let shell = ctx.shell.clone().ok_or_else(|| {
            DebugToolError::request_failed(
                "debug launch requires a session shell, which is unavailable in this runtime",
            )
        })?;
        let effective_cwd = params.cwd.clone().or_else(|| ctx.working_dir.clone());

        if kind == AdapterKind::Debugpy {
            return self.launch_debugpy(ctx, shell, effective_cwd, params).await;
        }

        // Native adapters run on the host outside `heel`, while the debuggee
        // itself spawns inside the session's `Shell` sandbox!
        let resolved = resolve_native_adapter(kind, params.adapter.is_none())
            .await
            .map_err(|err| DebugToolError::request_failed(err.to_string()))?;

        let script = build_native_launch_script(
            &params.program,
            &params.args,
            effective_cwd.as_deref(),
            &params.env,
        );
        let job = spawn_sandboxed_target(
            &shell,
            ctx.launch_mode,
            format!("debug launch {}", params.program),
            script,
        )
        .await?;
        let job_pid = job.pid();
        let host_root_pid = if ctx.launch_mode == SandboxMode::Unsandboxed {
            job_pid
        } else {
            resolve_sandbox_to_host_pid(Some(&shell), job_pid).await
        };
        let host_attach_pid = resolve_debuggee_pid(host_root_pid).await;
        let resolved_program =
            resolve_launch_program_path(&params.program, effective_cwd.as_deref()).await;

        let transport = spawn_host_stdio_adapter(&resolved)?;
        let config = DebugSessionConfig::new(resolved.kind.adapter_id())
            .with_target(params.program.clone())
            .with_target_pid(job_pid);
        let (session, notif_rx) = DebugSession::start(&TokioGlobal, transport, config)
            .await
            .map_err(|err| {
                let jobs = shell.jobs();
                tokio::spawn(async move {
                    let _ = jobs.kill(job_pid).await;
                });
                DebugToolError::request_failed(format!(
                    "failed to initialize {} adapter: {err}",
                    resolved.kind.adapter_id()
                ))
            })?;

        let mut outcome = match attach_launched_native_target(
            &session,
            resolved.kind,
            &job,
            host_attach_pid,
            &resolved_program,
            params.timeout,
        )
        .await
        {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = session.terminate().await;
                let _ = shell.jobs().kill(job_pid).await;
                return Err(DebugToolError::request_failed(format!(
                    "failed to attach {} to sandboxed target pid {job_pid}: {err}",
                    resolved.kind.adapter_id()
                )));
            }
        };

        if !params.stop_on_entry
            && matches!(outcome, ExecutionOutcome::Stopped(_))
            && let Ok(continued) = session.continue_execution(None, params.timeout).await
        {
            outcome = continued;
        }

        let owned_job = OwnedDebugJob::new(shell.jobs(), job_pid, job);
        self.install_session(
            ctx.owner,
            session,
            Some(owned_job),
            notif_rx,
            ctx.session_prompts,
        )
        .await;

        Ok(outcome.into_result("Debug target launched and running."))
    }

    async fn launch_debugpy(
        &self,
        ctx: DebugCallContext,
        shell: SessionShell,
        effective_cwd: Option<String>,
        params: LaunchParams,
    ) -> Result<DebugResult, DebugToolError> {
        let port = allocate_loopback_port().map_err(|err| {
            DebugToolError::request_failed(format!("failed to allocate loopback port: {err}"))
        })?;
        let script = build_debugpy_launch_script(
            port,
            &params.program,
            &params.args,
            effective_cwd.as_deref(),
            &params.env,
        );
        let job = spawn_sandboxed_target(
            &shell,
            ctx.launch_mode,
            format!("debugpy {}", params.program),
            script,
        )
        .await?;
        let job_pid = job.pid();
        let endpoint = format!("127.0.0.1:{port}");

        let transport = match connect_with_retry(&shell, &endpoint, DEBUGPY_READY_TIMEOUT).await {
            Ok(transport) => transport,
            Err(err) => {
                let _ = shell.jobs().kill(job_pid).await;
                return Err(DebugToolError::request_failed(format!(
                    "failed to connect to debugpy on {endpoint} (ensure `debugpy` is installed via `uv pip install debugpy`): {err}"
                )));
            }
        };

        let config = DebugSessionConfig::new(AdapterKind::Debugpy.adapter_id())
            .with_target(params.program.clone())
            .with_target_pid(job_pid);
        let (session, notif_rx) = DebugSession::start(&TokioGlobal, transport, config)
            .await
            .map_err(|err| {
                let jobs = shell.jobs();
                tokio::spawn(async move {
                    let _ = jobs.kill(job_pid).await;
                });
                DebugToolError::request_failed(format!("debugpy initialize failed: {err}"))
            })?;

        let attach_payload = json!({
            "justMyCode": false,
            "stopOnEntry": params.stop_on_entry,
        });
        let outcome = match session.attach(attach_payload, params.timeout).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = session.terminate().await;
                let _ = shell.jobs().kill(job_pid).await;
                return Err(DebugToolError::request_failed(format!(
                    "debugpy attach failed: {err}"
                )));
            }
        };

        let owned_job = OwnedDebugJob::new(shell.jobs(), job_pid, job);
        self.install_session(
            ctx.owner,
            session,
            Some(owned_job),
            notif_rx,
            ctx.session_prompts,
        )
        .await;

        Ok(outcome.into_result("Python debugpy target launched and running."))
    }

    async fn handle_attach(
        &self,
        ctx: DebugCallContext,
        pid: u32,
        adapter: Option<String>,
        timeout: Duration,
    ) -> Result<DebugResult, DebugToolError> {
        self.ensure_no_active_session(&ctx.owner).await?;

        // Look up the PID in JobRegistry (and prior shells across rebuilds) so
        // `detach` preserves it and `terminate` kills it through `Jobs`, then map
        // Linux `heel` nested-PID-namespace IDs to their real host OS PID for the
        // host debug adapter.
        let owned_job = lookup_owned_job(ctx.shell.as_ref(), ctx.prior_jobs.as_ref(), pid).await;
        let host_root_pid = resolve_sandbox_to_host_pid(ctx.shell.as_ref(), pid).await;
        let attach_pid = resolve_debuggee_pid(host_root_pid).await;
        let reported_pid = owned_job.as_ref().map_or(attach_pid, |j| j.job_pid);
        let comm = inspect_process_comm(attach_pid).await;

        let kind = select_adapter_for_attach(comm.as_deref(), adapter.as_deref())
            .map_err(|err| DebugToolError::request_failed(err.to_string()))?;
        let resolved = resolve_native_adapter(kind, adapter.is_none())
            .await
            .map_err(|err| DebugToolError::request_failed(err.to_string()))?;

        let transport = spawn_host_stdio_adapter(&resolved)?;
        let config = DebugSessionConfig::new(resolved.kind.adapter_id())
            .with_target(format!("pid:{reported_pid}"))
            .with_target_pid(reported_pid);
        let (session, notif_rx) = DebugSession::start(&TokioGlobal, transport, config)
            .await
            .map_err(|err| {
                DebugToolError::request_failed(format!(
                    "failed to initialize {} adapter: {err}",
                    resolved.kind.adapter_id()
                ))
            })?;

        let attach_args = native_attach_payload(resolved.kind, attach_pid);
        let outcome = match session.attach(attach_args, timeout).await {
            Ok(outcome) => outcome,
            Err(err) => {
                let _ = session.detach().await;
                return Err(DebugToolError::request_failed(format!(
                    "failed to attach {} to pid {reported_pid} (host pid {attach_pid}): {err}",
                    resolved.kind.adapter_id()
                )));
            }
        };

        self.install_session(ctx.owner, session, owned_job, notif_rx, ctx.session_prompts)
            .await;

        Ok(outcome.into_result(format!(
            "Attached to pid {reported_pid}; target is running."
        )))
    }

    async fn handle_connect(
        &self,
        ctx: DebugCallContext,
        params: ConnectParams,
    ) -> Result<DebugResult, DebugToolError> {
        self.ensure_no_active_session(&ctx.owner).await?;

        let (_host, _port, _is_loopback) =
            parse_endpoint(&params.endpoint).map_err(DebugToolError::request_failed)?;

        let adapter_id = match params.adapter.as_deref() {
            Some(explicit) => AdapterKind::parse_explicit(explicit)
                .map_err(|err| DebugToolError::request_failed(err.to_string()))?
                .adapter_id()
                .to_owned(),
            None => AdapterKind::TcpDirect.adapter_id().to_owned(),
        };

        let transport = connect_endpoint(ctx.shell.as_ref(), &params.endpoint).await?;
        let config = DebugSessionConfig::new(adapter_id).with_target(params.endpoint.clone());
        let (session, notif_rx) = DebugSession::start(&TokioGlobal, transport, config)
            .await
            .map_err(|err| {
                DebugToolError::request_failed(format!(
                    "DAP initialize over {} failed: {err}",
                    params.endpoint
                ))
            })?;

        let outcome = match params.mode {
            ConnectMode::Attach => session
                .attach(
                    json!({
                        "justMyCode": false,
                        "stopOnEntry": params.stop_on_entry,
                    }),
                    params.timeout,
                )
                .await
                .map_err(|err| {
                    DebugToolError::request_failed(format!("DAP attach failed: {err}"))
                })?,
            ConnectMode::Launch => {
                let mut payload = serde_json::Map::new();
                if let Some(program) = params.program {
                    payload.insert("program".to_owned(), json!(program));
                }
                if !params.args.is_empty() {
                    payload.insert("args".to_owned(), json!(params.args));
                }
                if let Some(cwd) = params.cwd.or(ctx.working_dir) {
                    payload.insert("cwd".to_owned(), json!(cwd));
                }
                payload.insert("stopOnEntry".to_owned(), json!(params.stop_on_entry));
                session
                    .launch(serde_json::Value::Object(payload), params.timeout)
                    .await
                    .map_err(|err| {
                        DebugToolError::request_failed(format!("DAP launch failed: {err}"))
                    })?
            }
        };

        self.install_session(ctx.owner, session, None, notif_rx, ctx.session_prompts)
            .await;

        Ok(outcome.into_result(format!(
            "Connected to {}; target is running.",
            params.endpoint
        )))
    }

    async fn ensure_no_active_session(&self, owner: &DebugOwner) -> Result<(), DebugToolError> {
        let existing = {
            let guard = self.state.lock().await;
            guard.sessions.get(owner).map(|entry| entry.session.clone())
        };
        if let Some(session) = existing {
            if let Ok(status) = session.status().await
                && !matches!(
                    status.state,
                    DebugSessionState::Inactive | DebugSessionState::Terminated
                )
            {
                return Err(DebugToolError::request_failed(
                    "a debug session is already active; run `debug detach` or `debug terminate` before starting another",
                ));
            }
            // Clean up already-terminated stale entry.
            let stale = self.state.lock().await.sessions.remove(owner);
            if let Some(entry) = stale {
                teardown_entry(entry).await;
            }
        }
        Ok(())
    }

    async fn install_session(
        &self,
        owner: DebugOwner,
        session: DebugSession,
        owned_job: Option<OwnedDebugJob>,
        notif_rx: Receiver<DebugNotification>,
        session_prompts: Option<SessionPrompts>,
    ) {
        let main_session_id = owner.main_session_id().map(str::to_owned);
        {
            let mut guard = self.state.lock().await;
            guard.next_generation = guard.next_generation.saturating_add(1);
            let generation = guard.next_generation;
            guard.sessions.insert(
                owner,
                ActiveDebugSession {
                    generation,
                    session,
                    owned_job,
                },
            );
        }

        if let (Some(session_id), Some(prompts)) = (main_session_id, session_prompts) {
            tokio::spawn(async move {
                while let Ok(notification) = notif_rx.recv().await {
                    let body = notification.render();
                    let rendered = DebugWakeText { body: &body }
                        .render()
                        .expect("debug wake template must render");
                    if let Err(err) = prompts.deliver_hidden(&session_id, rendered).await {
                        tracing::warn!(
                            %session_id,
                            error = %err,
                            "failed to deliver async debug wakeup through SessionPrompts"
                        );
                    }
                }
            });
        }
    }
}

struct LaunchParams {
    program: String,
    adapter: Option<String>,
    cwd: Option<String>,
    env: Vec<String>,
    stop_on_entry: bool,
    timeout: Duration,
    args: Vec<String>,
}

struct ConnectParams {
    endpoint: String,
    adapter: Option<String>,
    mode: ConnectMode,
    program: Option<String>,
    cwd: Option<String>,
    stop_on_entry: bool,
    timeout: Duration,
    args: Vec<String>,
}

async fn teardown_entry(entry: ActiveDebugSession) {
    let _ = entry.session.terminate().await;
    if let Some(owned) = entry.owned_job {
        let _ = owned.jobs.kill(owned.job_pid).await;
    }
}

async fn lookup_owned_job(
    shell: Option<&SessionShell>,
    prior_jobs: Option<&PriorJobs>,
    pid: u32,
) -> Option<OwnedDebugJob> {
    if let Some(shell) = shell {
        let jobs = shell.jobs();
        if let Some(job) = jobs.get(pid).await {
            return Some(OwnedDebugJob::new(jobs, pid, job));
        }
    }
    if let Some(prior) = prior_jobs {
        for jobs in prior.handles() {
            if let Some(job) = jobs.get(pid).await {
                return Some(OwnedDebugJob::new(jobs, pid, job));
            }
        }
    }
    None
}

/// Maps a PID inside the session's `heel` execution environment (`Job::pid()`)
/// to its host root-namespace OS PID so host-spawned native debug adapters attach
/// to the real debuggee rather than an unrelated host process on Linux nested PID
/// namespaces.
#[cfg_attr(not(target_os = "linux"), allow(clippy::unused_async))]
async fn resolve_sandbox_to_host_pid(shell: Option<&SessionShell>, sandbox_pid: u32) -> u32 {
    #[cfg(target_os = "linux")]
    {
        use futures_lite::StreamExt;

        let Some(shell) = shell else {
            return sandbox_pid;
        };
        let Ok(mut entries) = async_fs::read_dir("/proc").await else {
            return sandbox_pid;
        };
        while let Some(Ok(entry)) = entries.next().await {
            let name = entry.file_name();
            let Some(host_pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if matches!(
                shell.sandbox().pid_in_sandbox(host_pid).await,
                Ok(Some(inner_pid)) if inner_pid == sandbox_pid
            ) {
                return host_pid;
            }
        }
        sandbox_pid
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = shell;
        sandbox_pid
    }
}

/// Maximum bytes read from `JobRegistry` stdout per `debug output` call.
const MAX_SUPPLEMENT_STDOUT_BYTES: usize = 64 * 1024;

/// Supplements DAP `output` snapshots with target stdout captured by `JobRegistry`
/// when a host-attached native debuggee's standard output flows into its `Shell` job
/// rather than DAP `output` events.
async fn supplement_job_stdout(
    snapshot: &mut OutputSnapshot,
    owned: &OwnedDebugJob,
    category: OutputCategory,
    tail: Option<usize>,
    clear: bool,
) {
    if !matches!(category, OutputCategory::All | OutputCategory::Stdout) {
        return;
    }
    if snapshot
        .entries
        .iter()
        .any(|entry| entry.category == OutputCategory::Stdout)
    {
        return;
    }
    let mut mirror = owned.stdout_mirror.lock().await;
    if let Ok(JobRead::Delta(delta)) = owned
        .job
        .read(mirror.cursor, MAX_SUPPLEMENT_STDOUT_BYTES)
        .await
    {
        mirror.cursor = delta.cursor;
        if !delta.bytes.is_empty() {
            let text = String::from_utf8_lossy(&delta.bytes);
            for line in text.split_inclusive('\n').filter(|line| !line.is_empty()) {
                mirror
                    .buffer
                    .push(OutputCategory::Stdout, (*line).to_owned());
            }
        }
    }
    let mirrored = mirror.buffer.snapshot(OutputCategory::Stdout, tail, clear);
    drop(mirror);
    if mirrored.entries.is_empty() {
        return;
    }
    snapshot.dropped_entries = snapshot
        .dropped_entries
        .saturating_add(mirrored.dropped_entries);
    snapshot.dropped_bytes = snapshot
        .dropped_bytes
        .saturating_add(mirrored.dropped_bytes);
    snapshot.entries.extend(mirrored.entries);
    if let Some(limit) = tail
        && snapshot.entries.len() > limit
    {
        let extra = snapshot.entries.len() - limit;
        snapshot.entries.drain(..extra);
    }
}

async fn spawn_sandboxed_target(
    shell: &SessionShell,
    launch_mode: SandboxMode,
    description: String,
    script: String,
) -> Result<Job, DebugToolError> {
    let effective_shell = if launch_mode == SandboxMode::Unsandboxed {
        shell.clone().with_approver(InheritedUnsandboxedApprover)
    } else {
        shell.clone()
    };
    effective_shell
        .run_service(ServiceArgs {
            description,
            script,
            interpreter: Interpreter::default(),
            sandbox: launch_mode,
            location: None,
            resolution: MediaResolution::default(),
            max_lines: None,
            raw: false,
        })
        .await
        .map_err(|err| {
            DebugToolError::request_failed(format!("failed to spawn debug target: {err}"))
        })
}

fn spawn_host_stdio_adapter(
    resolved: &crate::debug::adapter::ResolvedNativeAdapter,
) -> Result<AnyDapTransport, DebugToolError> {
    let mut cmd = async_process::Command::new(&resolved.program);
    cmd.args(&resolved.args);
    let transport = StdioTransport::from_command(&mut cmd).map_err(|err| {
        DebugToolError::request_failed(format!(
            "failed to start host debug adapter `{}`: {err}",
            resolved.program.display()
        ))
    })?;
    Ok(AnyDapTransport::new(transport))
}

fn native_attach_payload(kind: AdapterKind, pid: u32) -> serde_json::Value {
    match kind {
        AdapterKind::DelveDap => json!({
            "mode": "local",
            "processId": pid,
        }),
        _ => json!({
            "pid": pid,
        }),
    }
}

fn build_native_launch_script(
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[String],
) -> String {
    let mut parts = Vec::new();
    if let Some(dir) = cwd
        && !dir.trim().is_empty()
    {
        parts.push(format!("cd {}", shell_escape::escape(Cow::Borrowed(dir))));
    }
    // Gate `exec` on stdin so short-lived targets never run and exit before the
    // host debug adapter completes initialization and prepares to catch entry.
    parts.push("{ IFS= read -r _ || true; }".to_owned());

    let mut exec_tokens = Vec::new();
    for kv in env {
        if let Some((k, v)) = kv.split_once('=')
            && !k.is_empty()
        {
            exec_tokens.push(format!("{k}={}", shell_escape::escape(Cow::Borrowed(v))));
        }
    }
    exec_tokens.push("exec".to_owned());
    exec_tokens.push(shell_escape::escape(Cow::Borrowed(program)).into_owned());
    for arg in args {
        exec_tokens.push(shell_escape::escape(Cow::Borrowed(arg.as_str())).into_owned());
    }
    parts.push(exec_tokens.join(" "));
    parts.join(" && ")
}

async fn resolve_launch_program_path(program: &str, cwd: Option<&str>) -> PathBuf {
    let candidate = Path::new(program);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else if let Some(dir) = cwd
        && !dir.trim().is_empty()
    {
        Path::new(dir).join(candidate)
    } else {
        candidate.to_path_buf()
    };
    async_fs::canonicalize(&joined).await.unwrap_or(joined)
}

async fn attach_launched_native_target(
    session: &DebugSession,
    kind: AdapterKind,
    job: &Job,
    job_pid: u32,
    resolved_program: &Path,
    timeout: Duration,
) -> Result<ExecutionOutcome, DapError> {
    let handshake_timeout = timeout.max(Duration::from_secs(5));

    #[cfg(target_os = "macos")]
    if kind == AdapterKind::LldbDap {
        let release_job = job.clone();
        tokio::spawn(async move {
            async_io::Timer::after(Duration::from_millis(120)).await;
            let _ = release_job.input(b"\n".to_vec()).await;
        });
        return session
            .attach(
                json!({
                    "program": resolved_program.display().to_string(),
                    "waitFor": true,
                    "stopOnEntry": true,
                }),
                handshake_timeout,
            )
            .await;
    }

    #[cfg(not(target_os = "macos"))]
    let _ = resolved_program;

    let attach_args = match kind {
        AdapterKind::DelveDap => native_attach_payload(kind, job_pid),
        _ => json!({
            "pid": job_pid,
            "stopOnEntry": true,
        }),
    };
    let outcome = session.attach(attach_args, handshake_timeout).await?;
    match outcome {
        ExecutionOutcome::Stopped(_) => {
            let release_job = job.clone();
            tokio::spawn(async move {
                async_io::Timer::after(Duration::from_millis(40)).await;
                let _ = release_job.input(b"\n".to_vec()).await;
            });
            session.continue_execution(None, handshake_timeout).await
        }
        other => {
            let _ = job.input(b"\n".to_vec()).await;
            Ok(other)
        }
    }
}

fn build_debugpy_launch_script(
    port: u16,
    program: &str,
    args: &[String],
    cwd: Option<&str>,
    env: &[String],
) -> String {
    let mut parts = Vec::new();
    if let Some(dir) = cwd
        && !dir.trim().is_empty()
    {
        parts.push(format!("cd {}", shell_escape::escape(Cow::Borrowed(dir))));
    }
    let mut exec_tokens = Vec::new();
    for kv in env {
        if let Some((k, v)) = kv.split_once('=')
            && !k.is_empty()
        {
            exec_tokens.push(format!("{k}={}", shell_escape::escape(Cow::Borrowed(v))));
        }
    }
    exec_tokens.push("exec".to_owned());
    exec_tokens.push("python3".to_owned());
    exec_tokens.push("-m".to_owned());
    exec_tokens.push("debugpy".to_owned());
    exec_tokens.push("--listen".to_owned());
    exec_tokens.push(format!("127.0.0.1:{port}"));
    exec_tokens.push("--wait-for-client".to_owned());
    // If caller passed `python3 script.py`, avoid duplicating `python3`.
    let base = Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program);
    if !matches!(base, "python" | "python3") {
        exec_tokens.push(shell_escape::escape(Cow::Borrowed(program)).into_owned());
    }
    for arg in args {
        exec_tokens.push(shell_escape::escape(Cow::Borrowed(arg.as_str())).into_owned());
    }
    parts.push(exec_tokens.join(" "));
    parts.join(" && ")
}

async fn connect_with_retry(
    shell: &SessionShell,
    endpoint: &str,
    timeout: Duration,
) -> Result<AnyDapTransport, DebugToolError> {
    let deadline = Instant::now() + timeout;
    loop {
        match connect_endpoint(Some(shell), endpoint).await {
            Ok(transport) => return Ok(transport),
            Err(err) => {
                if Instant::now() >= deadline {
                    return Err(err);
                }
                async_io::Timer::after(STARTUP_POLL_INTERVAL).await;
            }
        }
    }
}

async fn connect_endpoint(
    shell: Option<&SessionShell>,
    endpoint: &str,
) -> Result<AnyDapTransport, DebugToolError> {
    if let Ok(tcp) = TcpTransport::connect(endpoint).await {
        return Ok(AnyDapTransport::new(tcp));
    }

    // In container or remote sessions, loopback DAP servers listen inside the
    // sandbox's own network namespace; bridge them over exec stdio when host TCP fails.
    let (_host, _port, is_loopback) =
        parse_endpoint(endpoint).map_err(DebugToolError::request_failed)?;
    if is_loopback && let Some(shell) = shell {
        return spawn_container_loopback_bridge(shell, endpoint).await;
    }

    Err(DebugToolError::request_failed(format!(
        "failed to connect to DAP server at {endpoint}"
    )))
}

async fn spawn_container_loopback_bridge(
    shell: &SessionShell,
    endpoint: &str,
) -> Result<AnyDapTransport, DebugToolError> {
    let (host, port, _) = parse_endpoint(endpoint).map_err(DebugToolError::request_failed)?;
    let bridge_py = format!(
        "import os, select, socket, sys\n\
         s = socket.create_connection(({host:?}, {port}))\n\
         s.setblocking(False)\n\
         in_fd = sys.stdin.fileno()\n\
         out_fd = sys.stdout.fileno()\n\
         os.write(out_fd, b'READY\\n')\n\
         while True:\n\
             r, _, _ = select.select([in_fd, s], [], [])\n\
             if in_fd in r:\n\
                 data = os.read(in_fd, 65536)\n\
                 if not data: break\n\
                 s.sendall(data)\n\
             if s in r:\n\
                 data = s.recv(65536)\n\
                 if not data: break\n\
                 os.write(out_fd, data)\n"
    );
    let spec = SpawnSpec::new("python3")
        .args(["-u", "-c", &bridge_py])
        .stdio(Stdio::pipes(Pipe::Piped, Pipe::Piped, Pipe::Null));
    let child = shell.sandbox().spawn(spec).await.map_err(|err| {
        DebugToolError::request_failed(format!(
            "failed to bridge sandbox loopback DAP server at {endpoint}: {err}"
        ))
    })?;
    let transport = SandboxedChildTransport::connect_from_child(child, endpoint).await?;
    Ok(AnyDapTransport::new(transport))
}

/// [`DapTransport`] backed by a piped [`SandboxedChild`] (used for container loopback bridges).
struct SandboxedChildTransport {
    child: SandboxedChild,
    stdin: Sender<Vec<u8>>,
    stdout: Receiver<Vec<u8>>,
    buffered: Vec<u8>,
    framer: FrameReader,
    closed: bool,
}

impl SandboxedChildTransport {
    async fn connect_from_child(
        mut child: SandboxedChild,
        endpoint: &str,
    ) -> Result<Self, DebugToolError> {
        const READY_MARKER: &[u8] = b"READY\n";
        let stdin = child
            .take_stdin()
            .ok_or_else(|| DebugToolError::request_failed("sandbox DAP bridge stdin not piped"))?;
        let stdout = child
            .take_stdout()
            .ok_or_else(|| DebugToolError::request_failed("sandbox DAP bridge stdout not piped"))?;
        let mut buffered = Vec::new();
        while buffered.len() < READY_MARKER.len() {
            match stdout.recv().await {
                Ok(chunk) if !chunk.is_empty() => buffered.extend_from_slice(&chunk),
                _ => {
                    let _ = child.kill().await;
                    return Err(DebugToolError::request_failed(format!(
                        "failed to connect to DAP server at {endpoint}"
                    )));
                }
            }
        }
        if !buffered.starts_with(READY_MARKER) {
            let _ = child.kill().await;
            return Err(DebugToolError::request_failed(format!(
                "sandbox DAP bridge handshake failed for {endpoint}"
            )));
        }
        buffered.drain(..READY_MARKER.len());
        Ok(Self {
            child,
            stdin,
            stdout,
            buffered,
            framer: FrameReader::new(),
            closed: false,
        })
    }
}

struct ChannelAsyncReader<'a> {
    rx: &'a Receiver<Vec<u8>>,
    pending: &'a mut Vec<u8>,
}

impl AsyncRead for ChannelAsyncReader<'_> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut [u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if !self.pending.is_empty() {
            let n = buf.len().min(self.pending.len());
            buf[..n].copy_from_slice(&self.pending[..n]);
            self.pending.drain(..n);
            return std::task::Poll::Ready(Ok(n));
        }
        let mut recv_fut = std::pin::pin!(self.rx.recv());
        match std::future::Future::poll(recv_fut.as_mut(), cx) {
            std::task::Poll::Ready(Ok(chunk)) => {
                if chunk.is_empty() {
                    return std::task::Poll::Ready(Ok(0));
                }
                let n = buf.len().min(chunk.len());
                buf[..n].copy_from_slice(&chunk[..n]);
                if chunk.len() > n {
                    self.pending.extend_from_slice(&chunk[n..]);
                }
                std::task::Poll::Ready(Ok(n))
            }
            std::task::Poll::Ready(Err(_)) => std::task::Poll::Ready(Ok(0)),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

impl DapTransport for SandboxedChildTransport {
    async fn send(&mut self, message: &ProtocolMessage) -> Result<(), DapError> {
        if self.closed {
            return Err(DapError::ConnectionClosed);
        }
        let mut frame = Vec::new();
        aither_dap::write_message(&mut Cursor::new(&mut frame), message).await?;
        self.stdin
            .send(frame)
            .await
            .map_err(|_| DapError::ConnectionClosed)
    }

    async fn recv(&mut self) -> Result<Option<ProtocolMessage>, DapError> {
        if self.closed {
            return Ok(None);
        }
        let mut reader = BufReader::new(ChannelAsyncReader {
            rx: &self.stdout,
            pending: &mut self.buffered,
        });
        self.framer.read_message(&mut reader).await
    }

    async fn close(&mut self) -> Result<(), DapError> {
        self.closed = true;
        let _ = self.child.kill().await;
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod sandbox_tests {
    use super::*;
    use crate::test_sandbox::TestTerminal;
    use aither_sandbox::JobStatus;

    /// Verifies that a target started inside a confined `heel` sandbox can be
    /// attached to by real `pid` from the host `DebugBroker` without relaxing
    /// `heel` confinement, that `detach` leaves the `JobRegistry` job running,
    /// and that `terminate` tears down the owning job through `Jobs`.
    #[tokio::test]
    async fn host_broker_attaches_to_heel_sandboxed_native_target_and_tracks_job_registry() {
        if !macos_developer_mode_enabled().await {
            return;
        }
        let Ok(resolved) = resolve_native_adapter(AdapterKind::LldbDap, true).await else {
            // Skip cleanly on hosts without lldb-dap or gdb installed.
            return;
        };
        let Ok(cc) = which::which("cc") else {
            return;
        };

        let temp = tempfile::tempdir().expect("tempdir");
        let session_root = temp.path().join("session");
        let outside_probe = temp.path().join("outside-secret.txt");
        std::fs::create_dir_all(&session_root).expect("session dir");

        let src_path = session_root.join("target.c");
        let bin_path = session_root.join("target_bin");
        // Tiny native debuggee that proves it runs inside `heel` (attempting to
        // write outside `session_root` fails with EPERM/EACCES) and loops cleanly
        // so the host adapter can attach, inspect stack/threads, detach, re-attach,
        // and terminate through `Jobs`.
        std::fs::write(
            &src_path,
            format!(
                r#"#include <stdio.h>
#include <unistd.h>
static volatile int counter = 7;
int main(void) {{
    FILE *f = fopen("{outside}", "w");
    if (f != NULL) {{
        fputs("escaped", f);
        fclose(f);
        return 99;
    }}
    while (counter > 0) {{
        usleep(20000);
    }}
    return 0;
}}
"#,
                outside = outside_probe.display()
            ),
        )
        .expect("write target.c");

        let compiled = async_process::Command::new(cc)
            .args([
                "-g",
                "-O0",
                src_path.to_str().unwrap(),
                "-o",
                bin_path.to_str().unwrap(),
            ])
            .output()
            .await
            .expect("compile target.c");
        assert!(
            compiled.status.success(),
            "cc failed: {}",
            String::from_utf8_lossy(&compiled.stderr)
        );

        let terminal = TestTerminal::new(&session_root).await;
        let shell = SessionShell::clone(terminal.shell());
        let job = shell
            .run_service(ServiceArgs {
                description: "sandboxed native debuggee".to_owned(),
                script: format!(
                    "exec {}",
                    shell_escape::escape(Cow::Borrowed(bin_path.to_str().unwrap()))
                ),
                interpreter: Interpreter::default(),
                sandbox: SandboxMode::Sandboxed,
                location: None,
                resolution: MediaResolution::default(),
                max_lines: None,
                raw: false,
            })
            .await
            .expect("spawn sandboxed target");
        let pid = job.pid();
        async_io::Timer::after(Duration::from_millis(120)).await;
        assert!(
            !outside_probe.exists(),
            "heel sandbox must still block writes outside session_root"
        );

        let broker = DebugBroker::new();
        let ctx = DebugCallContext {
            owner: DebugOwner::MainSession("heel-native".to_owned()),
            shell: Some(shell.clone()),
            working_dir: Some(session_root.display().to_string()),
            launch_mode: SandboxMode::Sandboxed,
            session_prompts: None,
            prior_jobs: None,
        };

        let attached = broker
            .handle(
                ctx.clone(),
                DebugArgs::Attach {
                    pid,
                    adapter: Some(resolved.kind.adapter_id().to_owned()),
                    timeout_ms: Some(5000),
                },
            )
            .await;
        let attached = match attached {
            Ok(res) => res,
            Err(err) if is_host_ptrace_permission_denied(&err.to_string()) => {
                // Unprivileged containers or hosts without developer-mode ptrace grant.
                let _ = shell.jobs().kill(pid).await;
                return;
            }
            Err(err) => panic!("host adapter attach failed unexpectedly: {err}"),
        };
        assert!(
            matches!(
                attached,
                DebugResult::Stopped(_) | DebugResult::Running { .. }
            ),
            "unexpected attach result: {attached:?}"
        );

        // Detach leaves the owning JobRegistry job running!
        let detached = broker.handle(ctx.clone(), DebugArgs::Detach).await.unwrap();
        assert!(matches!(detached, DebugResult::Detached { target_pid: Some(p) } if p == pid));
        let info = job.info().await;
        assert!(
            matches!(info.status, JobStatus::Running),
            "detach must leave JobRegistry job running, got {:?}",
            info.status
        );

        // Re-attach and terminate: JobRegistry must record the job as no longer running!
        let _ = broker
            .handle(
                ctx.clone(),
                DebugArgs::Attach {
                    pid,
                    adapter: Some(resolved.kind.adapter_id().to_owned()),
                    timeout_ms: Some(5000),
                },
            )
            .await
            .expect("re-attach");
        let terminated = broker
            .handle(ctx, DebugArgs::Terminate)
            .await
            .expect("terminate");
        assert!(matches!(terminated, DebugResult::Terminated(_)));

        // Wait briefly for JobRegistry status to settle after Jobs::kill.
        let mut stopped = false;
        for _ in 0..40 {
            if !matches!(job.info().await.status, JobStatus::Running) {
                stopped = true;
                break;
            }
            async_io::Timer::after(Duration::from_millis(50)).await;
        }
        assert!(stopped, "terminate must stop the owning JobRegistry job");
    }

    fn is_host_ptrace_permission_denied(msg: &str) -> bool {
        let lower = msg.to_ascii_lowercase();
        lower.contains("operation not permitted")
            || lower.contains("permission denied")
            || lower.contains("cannot get permission to debug")
            || lower.contains("eperm")
            || lower.contains("ptrace")
    }

    #[cfg_attr(not(target_os = "macos"), allow(clippy::unused_async))]
    async fn macos_developer_mode_enabled() -> bool {
        #[cfg(target_os = "macos")]
        {
            let Ok(out) = async_process::Command::new("/usr/sbin/DevToolsSecurity")
                .arg("-status")
                .output()
                .await
            else {
                return false;
            };
            String::from_utf8_lossy(&out.stdout)
                .to_ascii_lowercase()
                .contains("enabled")
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }

    /// Verifies that a short-lived native C program launched via `DebugArgs::Launch`
    /// with `stop_on_entry: true` stops before `main` exits, binds source breakpoints,
    /// evaluates frame expressions, and surfaces `JobRegistry` stdout via `debug output`.
    #[tokio::test]
    async fn host_broker_launches_short_lived_sandboxed_target_and_reads_job_stdout() {
        if !macos_developer_mode_enabled().await {
            return;
        }
        let Ok(resolved) = resolve_native_adapter(AdapterKind::LldbDap, true).await else {
            return;
        };
        let Ok(cc) = which::which("cc") else {
            return;
        };

        let temp = tempfile::tempdir().expect("tempdir");
        let session_root = temp.path().join("session");
        std::fs::create_dir_all(&session_root).expect("session dir");

        let src_path = session_root.join("short_calc.c");
        let bin_path = session_root.join("short_calc");
        std::fs::write(
            &src_path,
            r#"#include <stdio.h>
int compute_twice(int x) {
    int doubled = x * 2;
    printf("STEP_STDOUT:%d\n", doubled);
    fflush(stdout);
    return doubled;
}
int main(void) {
    return compute_twice(60) == 120 ? 0 : 1;
}
"#,
        )
        .expect("write short_calc.c");

        let compiled = async_process::Command::new(cc)
            .args([
                "-g",
                "-O0",
                src_path.to_str().unwrap(),
                "-o",
                bin_path.to_str().unwrap(),
            ])
            .output()
            .await
            .expect("compile short_calc.c");
        assert!(compiled.status.success());

        let terminal = TestTerminal::new(&session_root).await;
        let shell = SessionShell::clone(terminal.shell());
        let broker = DebugBroker::new();
        let ctx = DebugCallContext {
            owner: DebugOwner::MainSession("heel-launch-short".to_owned()),
            shell: Some(shell),
            working_dir: Some(session_root.display().to_string()),
            launch_mode: SandboxMode::Sandboxed,
            session_prompts: None,
            prior_jobs: None,
        };

        let launched = broker
            .handle(
                ctx.clone(),
                DebugArgs::Launch {
                    program: bin_path.display().to_string(),
                    adapter: Some(resolved.kind.adapter_id().to_owned()),
                    cwd: Some(session_root.display().to_string()),
                    env: Vec::new(),
                    stop_on_entry: true,
                    timeout_ms: Some(5000),
                    args: Vec::new(),
                },
            )
            .await;
        let launched = match launched {
            Ok(res) => res,
            Err(err) if is_host_ptrace_permission_denied(&err.to_string()) => return,
            Err(err) => panic!("short-lived debug launch failed unexpectedly: {err}"),
        };
        assert!(
            matches!(launched, DebugResult::Stopped(_)),
            "expected stop-on-entry for short-lived binary, got {launched:?}"
        );

        let bp = broker
            .handle(
                ctx.clone(),
                DebugArgs::Break {
                    file: src_path.display().to_string(),
                    line: 6,
                    column: None,
                    condition: None,
                    hit_condition: None,
                    log_message: None,
                },
            )
            .await
            .expect("set breakpoint on return line");
        assert!(matches!(bp, DebugResult::BreakpointSet(_)));

        let resumed = broker
            .handle(
                ctx.clone(),
                DebugArgs::Continue {
                    thread_id: None,
                    timeout_ms: Some(5000),
                },
            )
            .await
            .expect("continue to breakpoint");
        assert!(
            matches!(resumed, DebugResult::Stopped(_)),
            "expected breakpoint stop, got {resumed:?}"
        );

        let eval = broker
            .handle(
                ctx.clone(),
                DebugArgs::Eval {
                    expression: "doubled + 7".to_owned(),
                    frame: Some(0),
                    context: None,
                },
            )
            .await
            .expect("evaluate doubled + 7");
        assert!(
            matches!(&eval, DebugResult::Eval(r) if r.value.contains("127")),
            "expected 127 in eval result, got {eval:?}"
        );

        // Give JobRegistry output pipe a brief moment to ingest flushed stdout.
        async_io::Timer::after(Duration::from_millis(60)).await;
        let out = broker
            .handle(
                ctx.clone(),
                DebugArgs::Output {
                    category: Some(OutputCategory::Stdout),
                    tail: None,
                    clear: false,
                },
            )
            .await
            .expect("read debug output");
        let DebugResult::Output(snap) = out else {
            panic!("expected Output snapshot");
        };
        let combined: String = snap.entries.iter().map(|e| e.text.as_str()).collect();
        assert!(
            combined.contains("STEP_STDOUT:120"),
            "expected STEP_STDOUT:120 in debug output, got {combined:?}"
        );

        let _ = broker.handle(ctx, DebugArgs::Terminate).await;
    }

    /// Verifies that a sandboxed Python process speaking DAP over loopback TCP
    /// works end-to-end inside `heel` without relaxing `heel` confinement.
    #[tokio::test]
    async fn host_broker_connects_to_heel_sandboxed_python_dap_stub() {
        let Ok(python) = which::which("python3") else {
            return;
        };
        let temp = tempfile::tempdir().expect("tempdir");
        let session_root = temp.path().join("session");
        std::fs::create_dir_all(&session_root).expect("session dir");

        let port = allocate_loopback_port().expect("port");
        let stub_path = session_root.join("dap_stub.py");
        std::fs::write(
            &stub_path,
            format!(
                r#"import json, socket
srv = socket.socket()
srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
srv.bind(("127.0.0.1", {port}))
srv.listen(1)
conn, _ = srv.accept()
seq = 1
def send(obj):
    global seq
    obj["seq"] = seq
    seq += 1
    data = json.dumps(obj).encode("utf-8")
    conn.sendall(f"Content-Length: {{len(data)}}\r\n\r\n".encode("ascii") + data)
buf = b""
while True:
    chunk = conn.recv(4096)
    if not chunk:
        break
    buf += chunk
    while b"\r\n\r\n" in buf:
        hdr, rest = buf.split(b"\r\n\r\n", 1)
        length = 0
        for line in hdr.decode("ascii").split("\r\n"):
            if line.lower().startswith("content-length:"):
                length = int(line.split(":", 1)[1].strip())
        if len(rest) < length:
            break
        body = json.loads(rest[:length].decode("utf-8"))
        buf = rest[length:]
        cmd = body.get("command")
        rseq = body.get("seq")
        if cmd == "initialize":
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd, "body": {{"supportsConfigurationDoneRequest": True}}}})
        elif cmd == "attach":
            send({{"type": "event", "event": "initialized"}})
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd}})
        elif cmd == "configurationDone":
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd}})
            send({{"type": "event", "event": "stopped", "body": {{"reason": "entry", "threadId": 1}}}})
        elif cmd == "threads":
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd, "body": {{"threads": [{{"id": 1, "name": "MainThread"}}]}}}})
        elif cmd == "stackTrace":
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd, "body": {{"stackFrames": [{{"id": 1, "name": "<module>", "source": {{"path": "dap_stub.py"}}, "line": 1, "column": 1}}]}}}})
        elif cmd == "disconnect":
            send({{"type": "response", "request_seq": rseq, "success": True, "command": cmd}})
            conn.close()
            srv.close()
            raise SystemExit(0)
"#
            ),
        )
        .expect("write stub");

        let terminal = TestTerminal::new(&session_root).await;
        let shell = SessionShell::clone(terminal.shell());
        let job = shell
            .run_service(ServiceArgs {
                description: "sandboxed python dap stub".to_owned(),
                script: format!(
                    "exec {} {}",
                    shell_escape::escape(Cow::Borrowed(python.to_str().unwrap())),
                    shell_escape::escape(Cow::Borrowed(stub_path.to_str().unwrap()))
                ),
                interpreter: Interpreter::default(),
                sandbox: SandboxMode::Sandboxed,
                location: None,
                resolution: MediaResolution::default(),
                max_lines: None,
                raw: false,
            })
            .await
            .expect("spawn python stub");

        let broker = DebugBroker::new();
        let ctx = DebugCallContext {
            owner: DebugOwner::MainSession("heel-py".to_owned()),
            shell: Some(shell.clone()),
            working_dir: Some(session_root.display().to_string()),
            launch_mode: SandboxMode::Sandboxed,
            session_prompts: None,
            prior_jobs: None,
        };

        // Wait briefly for the Python loopback listener to bind inside `heel`.
        let endpoint = format!("127.0.0.1:{port}");
        let mut connected = None;
        for _ in 0..40 {
            match broker
                .handle(
                    ctx.clone(),
                    DebugArgs::Connect {
                        endpoint: endpoint.clone(),
                        adapter: Some("debugpy".to_owned()),
                        mode: ConnectMode::Attach,
                        program: None,
                        cwd: None,
                        stop_on_entry: true,
                        timeout_ms: Some(2000),
                        args: Vec::new(),
                    },
                )
                .await
            {
                Ok(res) => {
                    connected = Some(res);
                    break;
                }
                Err(_) => {
                    async_io::Timer::after(Duration::from_millis(50)).await;
                }
            }
        }
        assert!(
            matches!(connected, Some(DebugResult::Stopped(_))),
            "expected stopped outcome from sandboxed Python DAP stub, got {connected:?}"
        );
        let _ = broker.handle(ctx, DebugArgs::Detach).await;
        let _ = shell.jobs().kill(job.pid()).await;
    }
}
