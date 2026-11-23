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
use aither_core::llm::tool::{CancelSignal, ToolContext};
use aither_core::llm::{AgentIdentity, Approval, CallIdentity, Tool, ToolResult};
use aither_sandbox::{CommitLedger, CommitMark, SubagentGrant};

use super::approvals::ChildApproval;
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
use crate::agent::ModelSlot;
use crate::agent::acp_subagent::AcpSession;
use crate::agent::subagent_tracker::{RunCard, RunUsage, SubagentTracker};
use crate::harness::compact_tool::CompactSignal;
use crate::harness::language::{LanguageGuard, WorkingText};
use crate::harness::subagent_file::{SubagentAccess, SubagentDefinition, SubagentLookup};
use crate::harness::working_document::{DocumentSpec, WorkEvent, WorkEvents, WorkingDocument};
use crate::ipc::{CommandUsage, SessionShell};
use crate::shell::{READ_TOOL, ShellToolkit};

/// What `fork` is for, as the delegating model reads it.
pub const FORK_DESCRIPTION: &str = "Continues your own context on your model; use it for work that depends on what you have read and decided";

/// The name a fork's run is reported under.
const FORK: &str = "fork";

/// The builder a [`SubagentModelResolver`] wraps.
type ResolveModel<LLM> = dyn Fn(
        &crate::ModelSelector,
    ) -> futures::future::BoxFuture<'static, anyhow::Result<crate::agent::ModelSlot<LLM>>>
    + Send
    + Sync;

/// Resolves a `provider::model` selector into a model slot, for a call's
/// `--model` or a definition's `model` pin.
#[derive(Clone)]
pub struct SubagentModelResolver<LLM> {
    /// The builder.
    build: std::sync::Arc<ResolveModel<LLM>>,
}

impl<LLM> std::fmt::Debug for SubagentModelResolver<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubagentModelResolver")
    }
}

impl<LLM> SubagentModelResolver<LLM> {
    /// Wraps `build`.
    pub fn new<F>(build: F) -> Self
    where
        F: Fn(
                &crate::ModelSelector,
            )
                -> futures::future::BoxFuture<'static, anyhow::Result<crate::agent::ModelSlot<LLM>>>
            + Send
            + Sync
            + 'static,
    {
        Self {
            build: std::sync::Arc::new(build),
        }
    }

    /// The slot `selector` builds on.
    ///
    /// # Errors
    /// Returns what `build` rejected.
    pub async fn resolve(
        &self,
        selector: &crate::ModelSelector,
    ) -> anyhow::Result<crate::agent::ModelSlot<LLM>> {
        (self.build)(selector).await
    }
}

/// What every `subagent` call of a session shares.
pub struct SubagentSettings<LLM> {
    /// Usage of the runs in flight, and the cards they report; its runs are
    /// the main agent's [`SubagentRuns`].
    pub tracker: SubagentTracker,
    /// The requests runs put to the agent that launched them: a call watches
    /// its children's and `subagent approve`/`deny` answers them.
    pub approvals: super::ChildApprovals,
    /// The model of each tier.
    pub tiers: TierModels<LLM>,
    /// Builds a model for an explicit `provider::model`; `None` refuses
    /// `--model` calls and `model = "..."` definitions.
    pub models: Option<SubagentModelResolver<LLM>>,
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
    /// What a read-only subagent's shell escalates through: nothing its
    /// scripts run may leave the sandbox.
    pub read_only_permissions: crate::terminal_permission_handler::TerminalPermissionHandler,
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
    /// Shared per-call image store so subagent tool hooks attach & OCR command images.
    pub call_images: Option<crate::ipc::CallImageStore>,
    /// Shared native OCR runtime so subagents reuse the session's initialized backend.
    pub vision_ocr: crate::vision_ocr::VisionOcr,
    /// The session's secrets: tool results a subagent sees and the output it
    /// returns are redacted with it, fail closed.
    pub redactor: crate::secrets::SecretRedactor,
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
    /// A connected agent behind a protocol, and the shell whose space and
    /// grants its processes run with.
    External {
        session: Box<AcpSession>,
        _shell: Option<Box<SessionShell>>,
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
struct Pending<LLM: LanguageModel + 'static> {
    name: String,
    /// The tier the run reports; `None` for an explicitly chosen model.
    tier: Option<Tier>,
    /// The model the agent is built on.
    slot: ModelSlot<LLM>,
    /// The model name the run reports.
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
    Fresh(Box<Pending<LLM>>),
    External(Box<Held<LLM>>),
}

/// What the wait on a spawned run resolves to.
enum Awaited<LLM: LanguageModel + 'static> {
    /// The run ended.
    Done(RunOutcome, Held<LLM>),
    /// One of its children put a request to this call's agent.
    Approval(ChildApproval),
    /// The run task died without reporting.
    Lost,
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
                .map_err(|refusal| crate::agent::notices::subagent_language_refused(&refusal))?;
        }
        Ok(prompt.to_string())
    }

    fn refuse_fork(refusal: &ForkRefusal) -> anyhow::Error {
        anyhow::anyhow!(render(&ForkRefused { reason: refusal }))
    }

    /// Answers a request a subagent parked for `decided_by` — the agent its
    /// launching call names, never another.
    fn answer(
        &self,
        decided_by: &CallIdentity,
        id: &str,
        approval: Approval,
    ) -> anyhow::Result<ToolResult> {
        let approved = approval.is_approved();
        let undecided = self
            .service
            .settings
            .approvals
            .decide(id, decided_by.agent(), approval);
        anyhow::ensure!(
            undecided.is_none(),
            "no request `{id}` of a subagent you launched awaits your decision"
        );
        Ok(ToolResult::text(render(&super::report::ApprovalAnswered {
            id,
            approved,
        })))
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
    /// caller's; a `confined` one's scripts stay inside the sandbox.
    async fn launch(
        &self,
        grant: SubagentGrant,
        caller: Option<&Path>,
        name: &str,
        confined: bool,
    ) -> anyhow::Result<SessionShell> {
        let Some(shell) = &self.shell else {
            anyhow::bail!("a subagent needs the shell of the agent that launches it");
        };
        let mut shell = shell
            .subagent(grant, caller, name)
            .await
            .map_err(|error| anyhow::anyhow!("failed to create the subagent's shell: {error}"))?;
        if confined {
            shell = shell.with_approver(self.settings().read_only_permissions.clone());
        }
        Ok(shell)
    }

    async fn start_external(
        &self,
        agent: &str,
        requested: Requested<'_>,
        caller: Option<&Path>,
    ) -> anyhow::Result<Held<LLM>> {
        let external = self.settings().externals.get(agent)?;
        let working_dir = self.caller_dir(caller);
        // The agent and its commands run in the space and with the grants of
        // this shell, as a native subagent's commands do.
        let shell = self
            .launch(
                requested.grant(Some(working_dir)),
                Some(working_dir),
                agent,
                false,
            )
            .await?;
        let ledger = shell.commit_ledger();
        let working_copies = shell
            .prepared_space()
            .map_or_default(|space| space.working_copies().to_vec());
        let session = external
            .backend
            .open(ExternalStart {
                sandbox: shell.sandbox(),
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
                _shell: Some(Box::new(shell)),
            },
        })
    }

    /// The slot an explicit `provider::model` builds on, and the name the
    /// run reports.
    async fn explicit_slot(
        &self,
        selector: &crate::ModelSelector,
    ) -> anyhow::Result<(ModelSlot<LLM>, String)> {
        let Some(resolve) = &self.settings().models else {
            anyhow::bail!(
                "this session builds no explicit models; run `models` to see the models a call may run on"
            );
        };
        let slot = resolve.resolve(selector).await?;
        let model = slot
            .llm
            .profile()
            .await
            .map_or_else(|_| selector.to_string(), |profile| profile.name);
        Ok((slot, model))
    }

    /// Resolves everything a fresh in-process subagent needs that can fail,
    /// so its run is registered only once nothing can.
    async fn prepare(
        &self,
        call: Call<'_>,
        prompt: &str,
        caller: Option<&Path>,
    ) -> anyhow::Result<Pending<LLM>> {
        let (source, grant, name, request) = match call {
            Call::Fork { grant } => {
                let snapshot = self.fork_snapshot()?;
                (Source::Fork(snapshot), grant, FORK.to_string(), None)
            }
            Call::Named {
                name,
                grant,
                model,
                tier,
            } => {
                let definition = self.lookup(caller).resolve(name).await?;
                if definition.access == SubagentAccess::ReadOnly && !grant.is_empty() {
                    anyhow::bail!(
                        "subagent '{name}' is read-only: it writes only its own working directory and takes no `--worktrees` or `--commit-worktrees`"
                    );
                }
                // A `model` pin is absolute: an agent defined as an opus 5.5
                // agent is that agent only on that model. A `tier` pin fixes
                // the class, not the concrete model — it exists to abstract
                // the choice, and a call naming a concrete model brings
                // information the class cannot hold, so `--model` still wins.
                if let Some(pinned) = &definition.model
                    && (model.is_some() || tier.is_some())
                {
                    anyhow::bail!(
                        "subagent '{name}' pins its model to '{pinned}': a call chooses no model"
                    );
                }
                if let Some(pinned) = definition.tier
                    && tier.is_some()
                {
                    anyhow::bail!(
                        "subagent '{name}' pins the {} tier; a call chooses a concrete model with `--model`",
                        pinned.short_name()
                    );
                }
                let declared = definition.tier.map(Tier::try_from).transpose()?;
                (
                    Source::Definition(Box::new(definition.clone())),
                    grant,
                    definition.name,
                    Some((declared, model, tier)),
                )
            }
            Call::External { .. } | Call::Resume { .. } => {
                unreachable!("external and resumed calls do not prepare an agent")
            }
            Call::Approve { .. } | Call::Deny { .. } => {
                unreachable!("approve and deny calls are answered, never prepared")
            }
        };
        let tiers = &self.settings().tiers;
        let (tier, slot, model) = match &source {
            // A fork runs on the main agent's model.
            Source::Fork(snapshot) => {
                let (tier, model) = tiers.select(prompt, Some(Tier::Advanced)).await?;
                if model != snapshot.model() {
                    return Err(Self::refuse_fork(&ForkRefusal::OtherModel {
                        main: snapshot.model().to_string(),
                        fork: model,
                    }));
                }
                (Some(tier), tiers.slot(tier).clone(), model)
            }
            Source::Definition(definition) => {
                let (declared, call_model, call_tier) =
                    request.expect("a named call carries its choice");
                if let Some(selector) = definition.model.as_ref().or(call_model.as_ref()) {
                    let (slot, model) = self.explicit_slot(selector).await?;
                    (None, slot, model)
                } else {
                    let (tier, model) = tiers.select(prompt, call_tier.or(declared)).await?;
                    (Some(tier), tiers.slot(tier).clone(), model)
                }
            }
        };
        let confined = matches!(
            &source,
            Source::Definition(definition)
                if definition.access == SubagentAccess::ReadOnly
        );
        let shell = self
            .launch(
                grant.grant(Some(self.caller_dir(caller))),
                caller,
                &name,
                confined,
            )
            .await?;
        Ok(Pending {
            name,
            tier,
            slot,
            model,
            shell,
            source,
        })
    }

    /// Builds the agent of `pending` under the identity its run was
    /// registered with.
    fn build(&self, pending: Pending<LLM>, identity: AgentIdentity, usage: RunUsage) -> Held<LLM> {
        let settings = self.settings();
        let Pending {
            name,
            tier,
            slot,
            model,
            shell,
            source,
        } = pending;
        let floor = match &source {
            Source::Definition(_) => ContextPolicy::default().compaction_floor,
            Source::Fork(snapshot) => snapshot.policy().compaction_floor,
        };
        // The valve starts at the same floor the policy reports, so the first
        // sync does not see a zero floor where the context never was empty.
        let valve = Arc::new(CompactSignal::with_floor(floor));
        let requested = Arc::new(AtomicBool::new(false));
        let child_hooks = ChildHooks::compacting(Some(valve.clone()), requested.clone())
            .with_redactor(settings.redactor.clone());
        let child_hooks = match settings.call_images.clone() {
            Some(call_images) => child_hooks.with_image_ocr(
                settings.vision_ocr.clone(),
                call_images,
                shell.output_store().dir().to_path_buf(),
            ),
            None => child_hooks,
        };
        let mut builder = AgentBuilder::new(slot.llm.clone())
            .hooks(child_hooks)
            .identity(identity);
        match &source {
            Source::Definition(definition) => {
                builder = builder
                    .request_parameters(slot.parameters)
                    .cache_lifetime(settings.cache_lifetime)
                    .instruction(definition.prompt.clone());
                for block in &settings.instructions {
                    builder = builder.instruction(block.clone());
                }
            }
            Source::Fork(snapshot) => {
                builder = builder.forked_from(snapshot);
            }
        }
        builder = settings
            .toolkit
            .register(builder, &shell, Some(valve.clone()));
        if let Some(spec) = &settings.document {
            // Its own inbox: the main agent's holds the starts and ends of
            // the main agent's runs, which are not this one's to answer.
            let mut document = WorkingDocument::new(spec, shell.working_dir(), WorkEvents::new());
            if let Some(reminder) = &settings.continue_reminder {
                document = document.with_continue_reminder(reminder.clone());
            }
            builder = builder.context_source(document);
        }
        let agent: Agent<LLM, ChildHooks, LLM, HandoffDocument> = builder.build();
        let ledger = shell.commit_ledger();
        let working_copies = shell
            .prepared_space()
            .map_or_default(|space| space.working_copies().to_vec());
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
                read_epochs: settings.toolkit.read_epochs().clone(),
            },
        );
        Held {
            name,
            tier,
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
            Call::Approve { .. } | Call::Deny { .. } => {
                unreachable!("approve and deny calls are answered, never run")
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
    type Error = SubagentCallError;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        self.run(args, cx)
            .await
            .map_err(|error| SubagentCallError(error.to_string()))
    }
}

/// A `subagent` call failed before it could return a tool result.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SubagentCallError(String);

impl<LLM: LanguageModel + Clone + 'static> SubagentCommand<LLM> {
    #[allow(clippy::too_many_lines)]
    async fn run(&self, args: SubagentArgs, cx: ToolContext) -> anyhow::Result<ToolResult> {
        let Some(started_by) = cx.identity() else {
            anyhow::bail!("the subagent command is called from an agent's tool call");
        };
        let call = args.call()?;
        match call {
            Call::Approve { id } => {
                return self.answer(started_by, id, Approval::Approved);
            }
            Call::Deny { id, reason } => {
                return self.answer(
                    started_by,
                    id,
                    Approval::Denied {
                        reason: reason
                            .map_or_else(|| "denied by its parent".to_string(), str::to_string),
                    },
                );
            }
            _ => {}
        }
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
        let (usage, tracked) = service.settings.tracker.track(run.id());
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
        let card_seed = self.cards.then(|| CardSeed {
            call_id: started_by.call().to_string(),
            prompt: prompt.clone(),
        });

        let prompt_text = run_prompt(&prompt, resumed, &held.working_copies).into_owned();
        let (done_tx, done_rx) = async_channel::bounded::<(RunOutcome, Held<LLM>)>(1);
        executor_core::Executor::spawn(&executor_core::DefaultExecutor, async move {
            let outcome = run.run(&mut held.body, &prompt_text).await;
            let _ = done_tx.try_send((outcome, held));
            drop(tracked);
        })
        .detach();

        // The turn stays free while a child waits on a decision: the request
        // reaches the caller as this call's result, and the run reports back
        // on the session when it ends.
        let awaited = futures_lite::future::or(
            async {
                match done_rx.recv().await {
                    Ok((outcome, held)) => Awaited::Done(outcome, held),
                    Err(_) => Awaited::Lost,
                }
            },
            async { Awaited::Approval(service.settings.approvals.wait_for(started_by).await) },
        )
        .await;

        let finish = Finish {
            service: service.clone(),
            name: name.clone(),
            lease: lease.clone(),
            ledger,
            mark,
            card_seed,
        };
        match awaited {
            Awaited::Done(outcome, held) => finish.report(held, outcome, card).await,
            Awaited::Approval(approval) => {
                // The call returns to answer: its card ends here while the
                // run's report back recreates one when it ends.
                if let Some(card) = card {
                    card.withdraw();
                }
                executor_core::Executor::spawn(&executor_core::DefaultExecutor, async move {
                    let Ok((outcome, held)) = done_rx.recv().await else {
                        return;
                    };
                    finish.deliver(held, outcome).await;
                })
                .detach();
                Ok(ToolResult::text(render(&super::report::ApprovalNeeded {
                    subagent: approval.child_name(),
                    id: approval.id(),
                    subject: approval.request().subject(),
                    reason: approval.request().reason(),
                })))
            }
            Awaited::Lost => {
                if let Some(card) = card {
                    card.withdraw();
                }
                anyhow::bail!("the subagent's run ended without a result")
            }
        }
    }
}

/// What a parked run's card is rebuilt from when its report lands.
struct CardSeed {
    call_id: String,
    prompt: String,
}

/// What a run's report shares between the path that waits on it and the one
/// delivering a parked run's result to its session.
struct Finish<LLM: LanguageModel + 'static> {
    service: Arc<SubagentService<LLM>>,
    name: String,
    lease: aither_core::IdLease,
    ledger: Option<CommitLedger>,
    mark: Option<CommitMark>,
    card_seed: Option<CardSeed>,
}

impl<LLM: LanguageModel + 'static> Finish<LLM> {
    /// Renders `outcome` into the call's result: jobs the run left are
    /// stopped, its commits are listed, a completed run's subagent is kept
    /// for continuation and its answer is redacted.
    async fn report(
        &self,
        held: Held<LLM>,
        outcome: RunOutcome,
        card: Option<RunCard<'_>>,
    ) -> anyhow::Result<ToolResult> {
        let settings = &self.service.settings;
        settings
            .work
            .record(WorkEvent::SubagentFinished(self.name.clone()));
        if let Body::Agent(session) = &held.body {
            let stopped = session.stop_jobs().await;
            if stopped > 0 {
                tracing::info!(subagent = %self.name, stopped, "Stopped background jobs left by a subagent");
            }
        }
        // Whatever way the run ended, the commits it made are reported.
        let commits = CommitListing::of(self.ledger.as_ref(), self.mark.as_ref()).await;
        let working_copies = held.working_copies.clone();

        let failure = match outcome {
            RunOutcome::Completed { final_text } => {
                tracing::info!(subagent = %self.name, "Subagent completed");
                let alert = match &held.body {
                    Body::Agent(session) => session.idle_alert(),
                    Body::External { .. } => None,
                };
                let cache_bound = matches!(held.body, Body::Agent(_));
                let (tier, model) = (held.tier, held.model.clone());
                self.service.finished.store(self.lease.clone(), held);
                // What the run returns is output of its commands too: the
                // session's secrets are redacted from it, fail closed.
                let redactions = settings.redactor.current().await.map_err(|error| {
                    anyhow::anyhow!("failed to load the session's secrets: {error}")
                })?;
                let final_text = redactions.redact(&final_text);
                let text = render(&SubagentResult {
                    subagent: &self.name,
                    outcome: SubagentReport::Completed {
                        tier: tier.map(Tier::label),
                        model: model.as_deref(),
                        result: &final_text,
                        id: &self.lease,
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
                tracing::info!(subagent = %self.name, "Subagent cancelled");
                return Ok(ToolResult::text(render(&SubagentResult {
                    subagent: &self.name,
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
            subagent: &self.name,
            error: &failure,
            commits: commits.as_ref(),
            working_copies: &working_copies,
            history: &history,
        })))
    }

    /// Delivers a parked run's report to the session that launched it — the
    /// call already returned when its subagent asked for a decision, so the
    /// result arrives as hidden context that wakes the session.
    async fn deliver(self, held: Held<LLM>, outcome: RunOutcome) {
        let card = self.card_seed.as_ref().map(|seed| {
            RunCard::start(
                &self.service.settings.tracker,
                seed.call_id.clone(),
                self.name.clone(),
                seed.prompt.clone(),
            )
        });
        let result = self.report(held, outcome, card).await;
        let Some((prompts, session_id)) = self.service.settings.approvals.session() else {
            return;
        };
        let text = match &result {
            Ok(result) => result.as_text().unwrap_or_default().to_string(),
            Err(error) => format!("{error:#}"),
        };
        if let Err(error) = prompts
            .deliver_hidden(
                &session_id,
                render(&super::report::RunFinished {
                    subagent: &self.name,
                    report: &text,
                }),
            )
            .await
        {
            tracing::warn!(%error, subagent = %self.name, "failed to deliver a parked run's result");
        }
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
