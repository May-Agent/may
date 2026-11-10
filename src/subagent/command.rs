//! The `subagent` command: runs a child agent on a brief and returns only its
//! final result.
//!
//! A call resolves what to run (a definition, the caller's own context as a
//! `fork`, a connected external agent, or a finished subagent to `resume`),
//! gives it a shell of its own derived from the caller's, registers the run
//! with the caller's [`SubagentRuns`], runs it and reports the result, the
//! commits it made and the id that continues it.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use aither_agent::context::{CacheLifetime, ContextPolicy, HandoffDocument};
use aither_agent::{
    Agent, AgentBuilder, ChildAgent, ChildEvents, ForkSnapshot, ForkSource, RunOutcome,
    SubagentRuns,
};
use aither_core::LanguageModel;
use aither_core::llm::model::ClaudePromptCacheTtl;
use aither_core::llm::tool::{CancelSignal, ToolContext};
use aither_core::llm::{AgentIdentity, Tool, ToolResult};
use aither_sandbox::{CommitLedger, CommitMark, SubagentGrant};

use super::args::{Call, Requested, SubagentArgs};
use super::child::{ChildHooks, ChildParts, ChildSession, KeepAlive};
use super::external::{ExternalAgents, ExternalStart};
use super::finished::{CallId, Expires, Finished};
use super::mounts::PromptFiles;
use super::report::{
    CommitListing, ForkRefusal, ForkRefused, SubagentBrief, SubagentError, SubagentReport,
    SubagentResult, SubagentUsage, render,
};
use super::tier::{Tier, TierModels};
use crate::agent::acp_subagent::AcpSession;
use crate::agent::subagent_tracker::{RunCard, RunUsage, SubagentTracker};
use crate::harness::compact_tool::CompactSignal;
use crate::harness::language::{LanguageGuard, WorkingText, default_refusal};
use crate::harness::subagent_file::{SubagentAccess, SubagentDefinition, SubagentLookup};
use crate::harness::working_document::{DocumentSpec, WorkEvent, WorkEvents, WorkingDocument};
use crate::ipc::{CommandUsage, SessionShell};
use crate::shell::{READ_TOOL, ShellToolkit};

/// What `fork` is for, as the delegating model reads it.
pub const FORK_DESCRIPTION: &str = "Continues your own context on your model; use it for work that depends on what you have read and decided";

/// The name a fork's run is reported under.
const FORK: &str = "fork";

/// What every `subagent` call of a session shares.
pub struct SubagentSettings<LLM> {
    /// Usage of the runs in flight, and the cards they report; its runs are
    /// the main agent's [`SubagentRuns`].
    pub tracker: SubagentTracker,
    /// The model of each tier.
    pub tiers: TierModels<LLM>,
    /// The definitions May supplies, by name.
    pub definitions: BTreeMap<String, SubagentDefinition>,
    /// Where a prompt file resolves.
    pub files: PromptFiles,
    /// The session's own directory: where `agents/` is looked in when the
    /// call names no other.
    pub base_dir: PathBuf,
    /// The requests the main agent publishes, which a fork starts from.
    pub fork_source: ForkSource,
    /// The tools every agent of the session registers.
    pub toolkit: ShellToolkit,
    /// Holds prompts to the working language.
    pub languages: Option<LanguageGuard>,
    /// The connected external agents.
    pub externals: ExternalAgents,
    /// Where the starts and ends of runs are recorded for the main agent's
    /// working document.
    pub work: WorkEvents,
    /// The working document each subagent keeps in its own space.
    pub document: Option<DocumentSpec>,
    /// The reminder that keeps a subagent going while its document has
    /// open rows.
    pub continue_reminder: Option<String>,
    /// The instruction blocks every subagent's context starts with.
    pub instructions: Vec<String>,
    /// The cache lifetime a subagent's context runs with.
    pub cache_lifetime: CacheLifetime,
    /// Cache keep-alives, for a provider whose cache is short enough to go
    /// cold while a subagent waits.
    pub keep_alive: Option<KeepAlive>,
}

impl<LLM> std::fmt::Debug for SubagentSettings<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentSettings").finish_non_exhaustive()
    }
}

/// The subagents a session can continue, and what its calls share.
pub struct SubagentService<LLM: LanguageModel + 'static> {
    settings: SubagentSettings<LLM>,
    finished: Finished<Held<LLM>>,
}

impl<LLM: LanguageModel + 'static> std::fmt::Debug for SubagentService<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentService").finish_non_exhaustive()
    }
}

impl<LLM: LanguageModel + 'static> std::fmt::Debug for SubagentCommand<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubagentCommand").finish_non_exhaustive()
    }
}

impl<LLM: LanguageModel + 'static> SubagentService<LLM> {
    /// The service of a session.
    #[must_use]
    pub fn new(settings: SubagentSettings<LLM>) -> Arc<Self> {
        Arc::new(Self {
            settings,
            finished: Finished::default(),
        })
    }

    /// The runs in flight.
    #[must_use]
    pub const fn runs(&self) -> &SubagentRuns {
        self.settings.tracker.runs()
    }
}

/// How a subagent runs.
enum Body<LLM: LanguageModel + 'static> {
    /// An in-process agent.
    Agent(Box<ChildSession<LLM>>),
    /// A connected agent behind a protocol, and the shell its grant came
    /// from.
    External {
        session: Box<AcpSession>,
        _shell: Option<SessionShell>,
    },
}

impl<LLM: LanguageModel + 'static> ChildAgent for Body<LLM> {
    async fn run(
        &mut self,
        prompt: &str,
        events: &ChildEvents,
        cancel: CancelSignal,
    ) -> RunOutcome {
        match self {
            Self::Agent(session) => session.run(prompt, events, cancel).await,
            Self::External { session, .. } => session.run(prompt, events, cancel).await,
        }
    }
}

/// A subagent the caller can continue.
struct Held<LLM: LanguageModel + 'static> {
    name: String,
    tier: Option<Tier>,
    model: Option<String>,
    ledger: Option<CommitLedger>,
    working_copies: Vec<PathBuf>,
    body: Body<LLM>,
}

impl<LLM: LanguageModel + 'static> Expires for Held<LLM> {
    fn expired(&self) -> Option<CacheLifetime> {
        match &self.body {
            Body::Agent(session) => session.expired(),
            Body::External { .. } => None,
        }
    }
}

/// What a fresh agent is built from, once its run is registered.
struct Pending {
    name: String,
    tier: Tier,
    model: String,
    shell: SessionShell,
    source: Source,
}

enum Source {
    Definition(Box<SubagentDefinition>),
    Fork(Arc<ForkSnapshot>),
}

/// What a call runs.
enum Ready<LLM: LanguageModel + 'static> {
    Continue(Box<Held<LLM>>),
    Fresh(Box<Pending>),
    External(Box<Held<LLM>>),
}

/// Releases the id a continuation checked out when the call is dropped
/// before it stored the subagent back.
struct Checkout<'a, LLM: LanguageModel + 'static> {
    finished: &'a Finished<Held<LLM>>,
    id: Option<String>,
}

impl<LLM: LanguageModel + 'static> Drop for Checkout<'_, LLM> {
    fn drop(&mut self) {
        if let Some(id) = &self.id {
            self.finished.release_checked_out(id);
        }
    }
}

/// The `subagent` command, bound to the shell of the agent calling it.
pub struct SubagentCommand<LLM: LanguageModel + 'static> {
    service: Arc<SubagentService<LLM>>,
    shell: Option<SessionShell>,
    cards: bool,
}

impl<LLM: LanguageModel + 'static> SubagentCommand<LLM> {
    /// The command of `service` for the agent whose shell is `shell`;
    /// without one, nothing that needs a shell can run.
    #[must_use]
    pub const fn new(service: Arc<SubagentService<LLM>>, shell: Option<SessionShell>) -> Self {
        Self {
            service,
            shell,
            cards: false,
        }
    }

    /// Reports every run as the card of the terminal call that started it.
    #[must_use]
    pub const fn with_cards(mut self) -> Self {
        self.cards = true;
        self
    }
}

impl<LLM: LanguageModel + Clone + Sync + 'static> SubagentCommand<LLM> {
    fn settings(&self) -> &SubagentSettings<LLM> {
        &self.service.settings
    }

    fn caller_dir<'a>(&'a self, caller: Option<&'a Path>) -> &'a Path {
        caller
            .or_else(|| {
                self.shell
                    .as_ref()
                    .map(|shell| shell.working_dir().as_path())
            })
            .unwrap_or(&self.settings().base_dir)
    }

    fn lookup<'a>(&'a self, caller: Option<&'a Path>) -> SubagentLookup<'a> {
        SubagentLookup {
            definitions: &self.settings().definitions,
            working_dir: self.caller_dir(caller),
        }
    }

    /// Reads the brief a call names; an inline text is no path, and an empty
    /// file is no brief.
    async fn read_prompt(&self, file: &str, caller: Option<&Path>) -> anyhow::Result<String> {
        let resolved = self
            .settings()
            .files
            .resolve(Path::new(file), caller)
            .await
            .map_err(|error| {
                anyhow::anyhow!("subagent prompt must be a path to a prompt file: {error}")
            })?;
        let content = async_fs::read_to_string(&resolved).await.map_err(|error| {
            anyhow::anyhow!(
                "failed to read subagent prompt file '{}': {error}",
                resolved.display()
            )
        })?;
        let prompt = content.trim();
        if prompt.is_empty() {
            anyhow::bail!("subagent prompt file '{}' is empty", resolved.display());
        }
        if let Some(guard) = &self.settings().languages {
            guard
                .check_working(prompt, WorkingText::Internal)
                .map_err(|refusal| anyhow::anyhow!(default_refusal(&refusal)))?;
        }
        Ok(prompt.to_string())
    }

    fn refuse_fork(refusal: &ForkRefusal) -> anyhow::Error {
        anyhow::anyhow!(render(&ForkRefused { reason: refusal }))
    }

    /// The main agent's request a fork starts from.
    fn fork_snapshot(&self) -> anyhow::Result<Arc<ForkSnapshot>> {
        if self.shell.is_none() {
            return Err(Self::refuse_fork(&ForkRefusal::NoShell));
        }
        let snapshot = self
            .settings()
            .fork_source
            .latest()
            .ok_or_else(|| Self::refuse_fork(&ForkRefusal::NothingSent))?;
        if !snapshot.allows_fork() {
            return Err(Self::refuse_fork(&ForkRefusal::NearCompaction {
                usage: snapshot.usage() * 100.0,
                compaction: snapshot.compaction_usage() * 100.0,
            }));
        }
        Ok(snapshot)
    }

    /// The shell a subagent holding `grant` works in, derived from the
    /// caller's.
    async fn launch(
        &self,
        grant: SubagentGrant,
        caller: Option<&Path>,
    ) -> anyhow::Result<SessionShell> {
        let Some(shell) = &self.shell else {
            anyhow::bail!("a subagent needs the shell of the agent that launches it");
        };
        shell
            .subagent(grant, caller)
            .await
            .map_err(|error| anyhow::anyhow!("failed to create the subagent's shell: {error}"))
    }

    async fn start_external(
        &self,
        agent: &str,
        requested: Requested<'_>,
        caller: Option<&Path>,
    ) -> anyhow::Result<Held<LLM>> {
        let external = self.settings().externals.get(agent)?;
        let working_dir = self.caller_dir(caller);
        let (worktrees, commit_worktrees) = requested.paths(Some(working_dir));
        let shell = self
            .launch(requested.grant(Some(working_dir)), Some(working_dir))
            .await?;
        let ledger = shell.commit_ledger();
        let working_copies = shell
            .prepared_space()
            .map(|space| space.working_copies().to_vec())
            .unwrap_or_default();
        let session = external
            .backend
            .open(ExternalStart {
                working_dir,
                worktrees: &worktrees,
                commit_worktrees: &commit_worktrees,
                working_copies: &working_copies,
            })
            .await
            .map_err(|error| anyhow::anyhow!("failed to open a '{agent}' session: {error}"))?;
        tracing::info!(subagent = %agent, "Starting external subagent");
        Ok(Held {
            name: agent.to_string(),
            tier: None,
            model: session.model(),
            ledger,
            working_copies,
            body: Body::External {
                session: Box::new(session),
                _shell: Some(shell),
            },
        })
    }

    /// Resolves everything a fresh in-process subagent needs that can fail,
    /// so its run is registered only once nothing can.
    async fn prepare(
        &self,
        call: Call<'_>,
        prompt: &str,
        caller: Option<&Path>,
    ) -> anyhow::Result<Pending> {
        let (source, grant, name, declared) = match call {
            Call::Fork { grant } => {
                let snapshot = self.fork_snapshot()?;
                (Source::Fork(snapshot), grant, FORK.to_string(), None)
            }
            Call::Named { name, grant } => {
                let definition = self.lookup(caller).resolve(name).await?;
                if definition.access == SubagentAccess::ReadOnly && !grant.is_empty() {
                    anyhow::bail!(
                        "subagent '{name}' is read-only: it writes only its own working directory and takes no `--worktrees` or `--commit-worktrees`"
                    );
                }
                let declared = definition.tier.map(Tier::try_from).transpose()?;
                (
                    Source::Definition(Box::new(definition.clone())),
                    grant,
                    definition.name,
                    declared,
                )
            }
            Call::External { .. } | Call::Resume { .. } => {
                unreachable!("external and resumed calls do not prepare an agent")
            }
        };
        let tiers = &self.settings().tiers;
        // A fork runs on the main agent's model.
        let (tier, model) = match &source {
            Source::Fork(snapshot) => {
                let (tier, model) = tiers.select(prompt, Some(Tier::Advanced)).await?;
                if model != snapshot.model() {
                    return Err(Self::refuse_fork(&ForkRefusal::OtherModel {
                        main: snapshot.model().to_string(),
                        fork: model,
                    }));
                }
                (tier, model)
            }
            Source::Definition(_) => tiers.select(prompt, declared).await?,
        };
        let shell = self
            .launch(grant.grant(Some(self.caller_dir(caller))), caller)
            .await?;
        Ok(Pending {
            name,
            tier,
            model,
            shell,
            source,
        })
    }

    /// Builds the agent of `pending` under the identity its run was
    /// registered with.
    fn build(&self, pending: Pending, identity: AgentIdentity, usage: RunUsage) -> Held<LLM> {
        let settings = self.settings();
        let Pending {
            name,
            tier,
            model,
            shell,
            source,
        } = pending;
        let slot = settings.tiers.slot(tier);
        let valve = Arc::new(CompactSignal::default());
        let requested = Arc::new(AtomicBool::new(false));
        let mut builder = AgentBuilder::new(slot.llm.clone())
            .hooks(ChildHooks::compacting(
                Some(valve.clone()),
                requested.clone(),
            ))
            .identity(identity);
        let floor = match &source {
            Source::Definition(definition) => {
                builder = builder
                    .request_parameters(slot.parameters.clone())
                    .cache_lifetime(settings.cache_lifetime)
                    .instruction(definition.prompt.clone());
                for block in &settings.instructions {
                    builder = builder.instruction(block.clone());
                }
                ContextPolicy::default().compaction_floor
            }
            Source::Fork(snapshot) => {
                builder = builder.forked_from(snapshot);
                snapshot.policy().compaction_floor
            }
        };
        if settings.keep_alive.is_some() {
            builder = builder.claude_cache_ttl(ClaudePromptCacheTtl::FiveMinutes);
        }
        builder = settings
            .toolkit
            .register(builder, &shell, Some(valve.clone()));
        if let Some(spec) = &settings.document {
            let mut document =
                WorkingDocument::new(spec, shell.working_dir(), settings.work.clone());
            if let Some(reminder) = &settings.continue_reminder {
                document = document.with_continue_reminder(reminder.clone());
            }
            builder = builder.context_source(document);
        }
        let agent: Agent<LLM, ChildHooks, LLM, HandoffDocument> = builder.build();
        let ledger = shell.commit_ledger();
        let working_copies = shell
            .prepared_space()
            .map(|space| space.working_copies().to_vec())
            .unwrap_or_default();
        tracing::info!(subagent = %name, model = %model, "Starting subagent");
        let session = ChildSession::new(
            agent,
            requested,
            shell,
            ChildParts {
                name: name.clone(),
                valve: Some(valve),
                floor,
                keep_alive: settings.keep_alive,
                read_tool: READ_TOOL,
                usage: Some(usage),
            },
        );
        Held {
            name,
            tier: Some(tier),
            model: Some(model),
            ledger,
            working_copies,
            body: Body::Agent(Box::new(session)),
        }
    }

    /// What a call runs: the finished subagent `resume` names, or what a
    /// fresh one is built from. The id is leased before anything starts, so
    /// no work is lost to a missing id.
    async fn acquire(
        &self,
        call: Call<'_>,
        prompt: &str,
        caller: Option<&Path>,
    ) -> anyhow::Result<(Ready<LLM>, CallId)> {
        let finished = &self.service.finished;
        match call {
            Call::Resume { id } => {
                let (id, held) = finished.checkout(id)?;
                tracing::info!(subagent = %held.name, id = %id.lease, "Continuing subagent");
                Ok((Ready::Continue(held), id))
            }
            Call::External { agent, grant } => {
                self.settings().externals.get(agent)?;
                let id = self.lease()?;
                let held = self.start_external(agent, grant, caller).await?;
                Ok((Ready::External(Box::new(held)), id))
            }
            call @ (Call::Fork { .. } | Call::Named { .. }) => {
                let id = self.lease()?;
                let pending = self.prepare(call, prompt, caller).await?;
                Ok((Ready::Fresh(Box::new(pending)), id))
            }
        }
    }

    fn lease(&self) -> anyhow::Result<CallId> {
        let Some(shell) = &self.shell else {
            anyhow::bail!("a subagent needs the shell of the agent that launches it");
        };
        Finished::<Held<LLM>>::lease(shell.id_allocator())
    }
}

/// A run's brief: with working copies in its space, the first prompt says
/// where they are.
fn run_prompt<'a>(prompt: &'a str, resumed: bool, working_copies: &[PathBuf]) -> Cow<'a, str> {
    if resumed || working_copies.is_empty() {
        Cow::Borrowed(prompt)
    } else {
        Cow::Owned(render(&SubagentBrief {
            working_copies,
            brief: prompt,
        }))
    }
}

impl<LLM: LanguageModel + Clone + Sync + 'static> Tool for SubagentCommand<LLM> {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("subagent")
    }

    type Arguments = SubagentArgs;
    type Res = ToolResult;

    #[allow(clippy::too_many_lines)]
    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> aither_core::Result<Self::Res> {
        let Some(started_by) = cx.identity() else {
            anyhow::bail!("the subagent command is called from an agent's tool call");
        };
        let call = args.call()?;
        let caller = cx.working_dir().map(Path::new);
        let prompt = self.read_prompt(&args.prompt_file, caller).await?;
        let service = &self.service;
        let (ready, id) = self.acquire(call, &prompt, caller).await?;
        let lease = id.lease.clone();
        let resumed = id.resumed;
        let mut checkout = Checkout {
            finished: &service.finished,
            id: resumed.then(|| lease.to_string()),
        };

        // Where the committing worktrees stand before the run, since commits
        // are listed per run. A continued subagent goes back where it came
        // from when that fails.
        let ledger = match &ready {
            Ready::Continue(held) | Ready::External(held) => held.ledger.clone(),
            Ready::Fresh(pending) => pending.shell.commit_ledger(),
        };
        let mark: Option<CommitMark> = match &ledger {
            Some(ledger) => match ledger.mark().await {
                Ok(mark) => Some(mark),
                Err(error) => {
                    if let Ready::Continue(held) = ready {
                        checkout.id = None;
                        service.finished.store(lease, *held);
                    }
                    anyhow::bail!(
                        "cannot record where the subagent's worktrees stand before its run: {error}"
                    );
                }
            },
            None => None,
        };

        let name = match &ready {
            Ready::Continue(held) | Ready::External(held) => held.name.clone(),
            Ready::Fresh(pending) => pending.name.clone(),
        };
        let run = service.runs().start(started_by, &name, &prompt);
        let (usage, _tracked) = service.settings.tracker.track(run.id());
        let mut held = match ready {
            Ready::Continue(mut held) => {
                if let Body::Agent(session) = &mut held.body {
                    session.credit_to(Some(usage));
                }
                *held
            }
            Ready::External(held) => *held,
            Ready::Fresh(pending) => self.build(*pending, run.identity(), usage),
        };
        let settings = &service.settings;
        settings
            .work
            .record(WorkEvent::SubagentStarted(name.clone()));
        let card = self.cards.then(|| {
            RunCard::start(
                &settings.tracker,
                started_by.call().to_string(),
                name.clone(),
                prompt.clone(),
            )
        });

        let outcome = run
            .run(
                &mut held.body,
                &run_prompt(&prompt, resumed, &held.working_copies),
            )
            .await;
        settings
            .work
            .record(WorkEvent::SubagentFinished(name.clone()));
        if let Body::Agent(session) = &held.body {
            let stopped = session.stop_jobs().await;
            if stopped > 0 {
                tracing::info!(subagent = %name, stopped, "Stopped background jobs left by a subagent");
            }
        }
        // Whatever way the run ended, the commits it made are reported.
        let commits = CommitListing::of(ledger.as_ref(), mark.as_ref()).await;
        let working_copies = held.working_copies.clone();

        let failure = match outcome {
            RunOutcome::Completed { final_text } => {
                tracing::info!(subagent = %name, "Subagent completed");
                let alert = match &held.body {
                    Body::Agent(session) => session.idle_alert(),
                    Body::External { .. } => None,
                };
                let cache_bound = matches!(held.body, Body::Agent(_));
                let (tier, model) = (held.tier, held.model.clone());
                checkout.id = None;
                service.finished.store(lease.clone(), held);
                let text = render(&SubagentResult {
                    subagent: &name,
                    outcome: SubagentReport::Completed {
                        tier: tier.map(Tier::label),
                        model: model.as_deref(),
                        result: &final_text,
                        id: &lease,
                        cache_bound,
                        alert: alert.as_deref(),
                    },
                    commits: commits.as_ref(),
                    working_copies: &working_copies,
                });
                if let Some(card) = card {
                    card.done(text.clone());
                }
                return Ok(ToolResult::text(text));
            }
            RunOutcome::Cancelled => {
                // The cancelled run left the agent mid-turn; it cannot be
                // continued.
                if let Some(card) = card {
                    card.withdraw();
                }
                tracing::info!(subagent = %name, "Subagent cancelled");
                return Ok(ToolResult::text(render(&SubagentResult {
                    subagent: &name,
                    outcome: SubagentReport::Cancelled,
                    commits: commits.as_ref(),
                    working_copies: &working_copies,
                })));
            }
            RunOutcome::Failed { error } => error.to_string(),
            RunOutcome::Paused { pause } => format!("the run paused: {pause:?}"),
            RunOutcome::IterationLimit => {
                "the subagent kept calling tools past its iteration limit".to_string()
            }
            other => format!("the run ended unexpectedly: {other:?}"),
        };
        if let Some(card) = card {
            card.withdraw();
        }
        let history = match &held.body {
            Body::Agent(session) => session.recent_history(),
            Body::External { .. } => Vec::new(),
        };
        Err(anyhow::anyhow!(render(&SubagentError {
            subagent: &name,
            error: &failure,
            commits: commits.as_ref(),
            working_copies: &working_copies,
            history: &history,
        })))
    }
}

impl<LLM: LanguageModel + Clone + Sync + 'static> CommandUsage for SubagentCommand<LLM> {
    /// The names a call can run at the moment of the call.
    async fn usage(&self) -> String {
        let subagents = self.lookup(None).resolvable().await;
        render(&SubagentUsage {
            subagents: &subagents,
            fork: FORK_DESCRIPTION,
            externals: self.settings().externals.described().collect(),
        })
    }
}
