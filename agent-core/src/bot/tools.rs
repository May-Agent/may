//! Narrow native `Tool` implementations for a May Bot's non-bash-first agent.
//!
//! Bots never register `terminal` or `monitor`; every capability is an explicit
//! native `Tool` whose output is natural language rendered from a typed Askama
//! template under `agent-core/templates/bot/`.

use std::borrow::Cow;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use may_db::{ChatDb, NewBotInboxEntry, NewBotSchedule};
use may_harness::task_trigger::{TimeTriggerType, normalize_cron_expression};
use may_protocol::bot::{
    BotApprovalDecision, BotInboxKind, BotScheduleItem, BotSseEvent, SeenMemoryLedger,
    format_monotonic_utc, next_monotonic_ts_ms,
};
use schemars::JsonSchema;
use serde::Deserialize;

use super::approvals::{BotApprovalRouter, BotEventPublishFn, BotWakeFn};
use super::delegates::{BotDelegateManager, DelegateSpawnSpec};
use super::memory::{
    BotMemoryCuratorQueue, recall_leaf, record_leaf_seen, resolve_store_fact_source,
};
use crate::agent_link::AgentMessaging;
use crate::notification::{NotificationPayload, NotificationService};
use crate::sleep::SleepState;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// Errors returned by a May Bot native tool.
#[derive(Debug, thiserror::Error)]
pub enum BotToolError {
    /// Invalid tool arguments or refused path/target.
    #[error("{0}")]
    Invalid(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
    /// Memory error.
    #[error(transparent)]
    Memory(#[from] super::memory::BotMemoryError),
    /// Delegate lifecycle error.
    #[error(transparent)]
    Delegate(#[from] super::delegates::DelegateError),
    /// Approval error.
    #[error(transparent)]
    Approval(#[from] super::approvals::BotApprovalError),
}

/// Callback that creates a supervised user-visible session, records supervising
/// `bot_id`, enqueues its required initial `prompt`, and wakes it.
pub trait SessionBridge: Send + Sync + 'static {
    /// Creates a supervised session and queues its initial prompt.
    #[allow(clippy::too_many_arguments)]
    fn create_session(
        &self,
        bot_id: String,
        prompt: String,
        title: Option<String>,
        profile_id: Option<String>,
        model: Option<String>,
        cwd: Option<String>,
        repo: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>>;

    /// Stops an active run on `session_id`.
    fn stop_session(
        &self,
        session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;
}

/// Shared runtime handles bound to one bot's native tool instances.
#[derive(Clone)]
pub struct BotToolContext {
    /// Bot primary id.
    pub bot_id: String,
    /// Bot stable `word-word-word` `agent_id`.
    pub bot_agent_id: String,
    /// Bot display name.
    pub bot_name: String,
    /// Live memory root (`~/.may/bots/<bot_id>/memory`).
    pub memory_root: PathBuf,
    /// Shared database.
    pub db: ChatDb,
    /// Inter-agent messaging directory (optional in unit tests).
    pub messaging: Option<AgentMessaging>,
    /// Delegated worker manager.
    pub delegates: BotDelegateManager,
    /// Approval triage router.
    pub approvals: BotApprovalRouter,
    /// Serial FIFO `store_fact` curator queue.
    pub curator: BotMemoryCuratorQueue,
    /// Supervised session bridge.
    pub sessions: Arc<dyn SessionBridge>,
    /// Notification service (optional in unit tests).
    pub notifications: Option<Arc<NotificationService>>,
    /// Daemon-wide sleep state (honored on user notifications).
    pub sleep: Option<Arc<SleepState>>,
    /// Shared `SeenMemoryLedger` updated when `recall(path)` reads a leaf.
    pub seen_memory: Arc<Mutex<SeenMemoryLedger>>,
    /// Per-run flag flipped to `true` when `send_message(to="user", ..)` succeeds.
    pub sent_to_user_flag: Arc<AtomicBool>,
    /// Wake callback for any bot (used when messaging another bot).
    pub wake_bot: BotWakeFn,
    /// Live SSE event publisher (`bot_*` on `/api/events`).
    pub publish_event: BotEventPublishFn,
    /// UI event bus.
    pub ui_event_bus: UiEventBus,
}

impl std::fmt::Debug for BotToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotToolContext")
            .field("bot_id", &self.bot_id)
            .field("bot_agent_id", &self.bot_agent_id)
            .finish_non_exhaustive()
    }
}

// ── 1. send_message ─────────────────────────────────────────────

/// Arguments for `send_message(to, message)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SendMessageArgs {
    /// Recipient: `"user"`, a session id or title, a delegated worker `agent_id`,
    /// or another bot's `agent_id` / name.
    pub to: String,
    /// Natural-language message body.
    pub message: String,
}

#[derive(Template)]
#[template(path = "bot/tool_send_message.txt", escape = "none")]
struct SendMessageTemplate<'a> {
    target_kind: &'a str,
    target_label: &'a str,
    note: Option<&'a str>,
}

/// Unified outbound message tool (`user`, `session`, `delegate`, or `bot`).
#[derive(Debug, Clone)]
pub struct SendMessageTool {
    ctx: BotToolContext,
}

impl SendMessageTool {
    /// Creates a `send_message` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for SendMessageTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("send_message")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Deliver a message to \"user\", a session (by id or title), a delegated worker (steering or waking it), or another bot.",
        )
    }

    type Arguments = SendMessageArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let to = args.to.trim();
        let body = args.message.trim();
        if to.is_empty() {
            return Err(BotToolError::Invalid("`to` must not be empty".to_string()));
        }
        if body.is_empty() {
            return Err(BotToolError::Invalid(
                "`message` must not be empty".to_string(),
            ));
        }

        // 1. User conversation delivery
        if to.eq_ignore_ascii_case("user") {
            let now = jiff::Timestamp::now();
            let last_ms = self
                .ctx
                .db
                .get_bot(&self.ctx.bot_id)
                .await?
                .map_or(0, |b| b.last_ts_ms);
            let ts_ms = next_monotonic_ts_ms(now, last_ms);
            let ts_utc = format_monotonic_utc(ts_ms);
            let chat_msg = self
                .ctx
                .db
                .append_bot_chat_message(&self.ctx.bot_id, "bot", body, ts_ms, &ts_utc, None)
                .await?;
            self.ctx.sent_to_user_flag.store(true, Ordering::Release);
            (self.ctx.publish_event)(BotSseEvent::BotChat { message: chat_msg });
            self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);

            let asleep = self
                .ctx
                .sleep
                .as_ref()
                .is_some_and(|sleep| sleep.is_asleep());
            let note = if asleep {
                Some("recorded in chat; push notification deferred because user is asleep")
            } else {
                if let Some(notifications) = &self.ctx.notifications {
                    let _ = notifications
                        .send_payload(NotificationPayload {
                            notification_id: None,
                            title: self.ctx.bot_name.clone(),
                            body: body.to_string(),
                            session_id: Some(self.ctx.bot_id.clone()),
                            profile_id: None,
                        })
                        .await;
                }
                None
            };
            return Ok(ToolResult::text(
                SendMessageTemplate {
                    target_kind: "user",
                    target_label: "conversation",
                    note,
                }
                .render()
                .expect("send_message template must render"),
            ));
        }

        // 2. Delegated worker owned by this bot
        if let Some(delegate) = self.ctx.db.get_bot_delegate(to).await?
            && delegate.bot_id == self.ctx.bot_id
        {
            let note = self
                .ctx
                .delegates
                .message(&self.ctx.bot_id, &delegate.agent_id, body.to_string())
                .await?;
            return Ok(ToolResult::text(
                SendMessageTemplate {
                    target_kind: "delegated worker",
                    target_label: &delegate.agent_id,
                    note: Some(note),
                }
                .render()
                .expect("send_message template must render"),
            ));
        }

        // 3. Another bot by id, agent_id, or name
        if let Some(other_bot) = self.ctx.db.find_bot_by_identifier(to).await? {
            if other_bot.id == self.ctx.bot_id {
                return Err(BotToolError::Invalid(
                    "a bot cannot send a message to itself".to_string(),
                ));
            }
            let _ = self
                .ctx
                .db
                .enqueue_bot_inbox(NewBotInboxEntry {
                    bot_id: &other_bot.id,
                    kind: BotInboxKind::AgentMessage,
                    sender_id: Some(&self.ctx.bot_agent_id),
                    sender_name: Some(&self.ctx.bot_name),
                    body,
                    meta_json: None,
                })
                .await?;
            (self.ctx.wake_bot)(&other_bot.id);
            return Ok(ToolResult::text(
                SendMessageTemplate {
                    target_kind: "bot",
                    target_label: &other_bot.agent_id,
                    note: Some("queued in bot inbox"),
                }
                .render()
                .expect("send_message template must render"),
            ));
        }

        // 4. Session via AgentDirectory / AgentMessaging
        if let Some(messaging) = &self.ctx.messaging {
            let recipient_name = messaging
                .send(&self.ctx.bot_agent_id, to, body)
                .await
                .map_err(|error| BotToolError::Invalid(error.to_string()))?;
            return Ok(ToolResult::text(
                SendMessageTemplate {
                    target_kind: "session",
                    target_label: &recipient_name,
                    note: None,
                }
                .render()
                .expect("send_message template must render"),
            ));
        }

        Err(BotToolError::Invalid(format!(
            "no user, delegated worker, bot, or session matched `{to}`"
        )))
    }
}

// ── 2. store_fact ───────────────────────────────────────────────

/// Arguments for `store_fact(fact, source_time, source_agent?)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StoreFactArgs {
    /// Natural-language fact, correction, or deletion to curate into long-term memory.
    pub fact: String,
    /// Exact UTC timestamp of the originating message (`YYYY-MM-DDTHH:MM:SSZ` or `.sssZ`).
    pub source_time: String,
    /// Optional `session_id` or delegated worker `agent_id`; omit for this bot's own inbox.
    #[serde(default)]
    pub source_agent: Option<String>,
}

#[derive(Template)]
#[template(path = "bot/tool_store_fact.txt", escape = "none")]
struct StoreFactTemplate<'a> {
    queue_id: i64,
    source_agent: &'a str,
    source_time: &'a str,
}

/// Enqueues a fact curation request onto the bot's serial FIFO curator queue.
#[derive(Debug, Clone)]
pub struct StoreFactTool {
    ctx: BotToolContext,
}

impl StoreFactTool {
    /// Creates a `store_fact` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for StoreFactTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("store_fact")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Enqueue a natural-language fact, correction, or deletion linked to its originating message's UTC source_time and optional source_agent.",
        )
    }

    type Arguments = StoreFactArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let fact = args.fact.trim();
        if fact.is_empty() {
            return Err(BotToolError::Invalid(
                "`fact` must not be empty".to_string(),
            ));
        }
        // Validate source provenance synchronously (fail fast on unknown timestamp or unauthorized target).
        let resolved = resolve_store_fact_source(
            &self.ctx.db,
            &self.ctx.bot_id,
            &self.ctx.bot_agent_id,
            &args.source_time,
            args.source_agent.as_deref(),
        )
        .await?;

        let row = self
            .ctx
            .db
            .enqueue_bot_store_fact(
                &self.ctx.bot_id,
                fact,
                &resolved.source_time,
                args.source_agent.as_deref(),
            )
            .await?;
        self.ctx.curator.wake();

        Ok(ToolResult::text(
            StoreFactTemplate {
                queue_id: row.id,
                source_agent: &resolved.source_agent,
                source_time: &resolved.source_time,
            }
            .render()
            .expect("store_fact template must render"),
        ))
    }
}

// ── 3. recall ───────────────────────────────────────────────────

/// Arguments for `recall(path)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RecallArgs {
    /// Relative path to a leaf Markdown file under memory (for example `preference/food`).
    pub path: String,
}

#[derive(Template)]
#[template(path = "bot/tool_recall.txt", escape = "none")]
struct RecallTemplate<'a> {
    path: &'a str,
    content: &'a str,
}

/// Reads a concrete leaf memory file and marks its `blake3` hash seen in the window ledger.
#[derive(Debug, Clone)]
pub struct RecallTool {
    ctx: BotToolContext,
}

impl RecallTool {
    /// Creates a `recall` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for RecallTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("recall")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Read a concrete leaf Markdown file from long-term memory (e.g. preference/food). Rejects directories and MEMORY.md.",
        )
    }

    type Arguments = RecallArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let (rel_path, content, hash) = recall_leaf(&self.ctx.memory_root, &args.path).await?;
        if let Ok(mut ledger) = self.ctx.seen_memory.lock() {
            record_leaf_seen(&mut ledger, &rel_path, hash);
        }
        Ok(ToolResult::text(
            RecallTemplate {
                path: &rel_path,
                content: &content,
            }
            .render()
            .expect("recall template must render"),
        ))
    }
}

// ── 4. delegate_agent ───────────────────────────────────────────

/// Arguments for `delegate_agent`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DelegateAgentArgs {
    /// Task brief for the delegated bash-first worker.
    pub prompt: String,
    /// Optional short label for this worker.
    #[serde(default)]
    pub name: Option<String>,
    /// Subagent definition name (defaults to `"general"`).
    #[serde(default)]
    pub definition: Option<String>,
    /// Optional host worktree directories granted read/write to this worker.
    #[serde(default)]
    pub worktrees: Vec<String>,
}

#[derive(Template)]
#[template(path = "bot/tool_delegate_agent.txt", escape = "none")]
struct DelegateAgentTemplate<'a> {
    agent_id: &'a str,
    name: &'a str,
    definition: &'a str,
    work_dir: &'a str,
}

/// Spawns an asynchronous bash-first delegated worker.
#[derive(Debug, Clone)]
pub struct DelegateAgentTool {
    ctx: BotToolContext,
}

impl DelegateAgentTool {
    /// Creates a `delegate_agent` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for DelegateAgentTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("delegate_agent")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Asynchronously spawn a bash-first delegated worker with full sandboxed terminal and IPC tools. Returns agent_id immediately.",
        )
    }

    type Arguments = DelegateAgentArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err(BotToolError::Invalid(
                "`prompt` must not be empty".to_string(),
            ));
        }
        let definition = args
            .definition
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("general")
            .to_string();
        let name = args
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&definition)
            .to_string();

        let summary = self
            .ctx
            .delegates
            .spawn(DelegateSpawnSpec {
                bot_id: self.ctx.bot_id.clone(),
                name,
                definition,
                prompt: prompt.to_string(),
                worktrees: args.worktrees,
            })
            .await?;
        self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);

        Ok(ToolResult::text(
            DelegateAgentTemplate {
                agent_id: &summary.agent_id,
                name: &summary.name,
                definition: &summary.definition,
                work_dir: &summary.work_dir,
            }
            .render()
            .expect("delegate_agent template must render"),
        ))
    }
}

// ── 5. stop_agent ───────────────────────────────────────────────

/// Arguments for `stop_agent(agent_id)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StopAgentArgs {
    /// Delegated worker `agent_id` to stop and checkpoint.
    pub agent_id: String,
}

#[derive(Template)]
#[template(path = "bot/tool_stop_agent.txt", escape = "none")]
struct StopAgentTemplate<'a> {
    agent_id: &'a str,
}

/// Cancels and checkpoints a running delegated worker.
#[derive(Debug, Clone)]
pub struct StopAgentTool {
    ctx: BotToolContext,
}

impl StopAgentTool {
    /// Creates a `stop_agent` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for StopAgentTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("stop_agent")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Stop a running delegated worker and checkpoint its context to history.db for later resumption.",
        )
    }

    type Arguments = StopAgentArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let summary = self
            .ctx
            .delegates
            .stop(&self.ctx.bot_id, args.agent_id.trim())
            .await?;
        self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);
        Ok(ToolResult::text(
            StopAgentTemplate {
                agent_id: &summary.agent_id,
            }
            .render()
            .expect("stop_agent template must render"),
        ))
    }
}

// ── 6. resume_agent ─────────────────────────────────────────────

/// Arguments for `resume_agent(agent_id, prompt?)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ResumeAgentArgs {
    /// Delegated worker `agent_id` to resume from its persisted checkpoint.
    pub agent_id: String,
    /// Optional follow-up prompt.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Template)]
#[template(path = "bot/tool_resume_agent.txt", escape = "none")]
struct ResumeAgentTemplate<'a> {
    agent_id: &'a str,
}

/// Resumes a stopped, completed, or failed delegated worker from its checkpoint.
#[derive(Debug, Clone)]
pub struct ResumeAgentTool {
    ctx: BotToolContext,
}

impl ResumeAgentTool {
    /// Creates a `resume_agent` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for ResumeAgentTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("resume_agent")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Resume a stopped, completed, or failed delegated worker from its persisted context checkpoint.",
        )
    }

    type Arguments = ResumeAgentArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let summary = self
            .ctx
            .delegates
            .resume(&self.ctx.bot_id, args.agent_id.trim(), args.prompt)
            .await?;
        self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);
        Ok(ToolResult::text(
            ResumeAgentTemplate {
                agent_id: &summary.agent_id,
            }
            .render()
            .expect("resume_agent template must render"),
        ))
    }
}

// ── 7. read_agent ───────────────────────────────────────────────

/// Arguments for `read_agent(target, tail?)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReadAgentArgs {
    /// Delegated worker `agent_id` or session id to inspect.
    pub target: String,
    /// Number of recent transcript entries to include (defaults to 15).
    #[serde(default)]
    pub tail: Option<usize>,
}

struct ReadAgentEntryView {
    ts_utc: String,
    role: String,
    content: String,
}

#[derive(Template)]
#[template(path = "bot/tool_read_agent.txt", escape = "none")]
struct ReadAgentTemplate<'a> {
    kind: &'a str,
    id: &'a str,
    name: &'a str,
    state: &'a str,
    elapsed_ms: i64,
    prompt_tokens: i64,
    completion_tokens: i64,
    active_command: Option<&'a str>,
    entries: &'a [ReadAgentEntryView],
}

/// Inspects live state, elapsed time, token usage, active command, and recent
/// UTC-timestamped transcript entries of a delegated worker or session.
#[derive(Debug, Clone)]
pub struct ReadAgentTool {
    ctx: BotToolContext,
}

impl ReadAgentTool {
    /// Creates a `read_agent` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for ReadAgentTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("read_agent")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Inspect live state, elapsed time, token usage, active command, and recent UTC-timestamped transcript entries of a delegated worker or session.",
        )
    }

    type Arguments = ReadAgentArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let target = args.target.trim();
        let tail = i64::try_from(args.tail.unwrap_or(15).clamp(1, 100)).unwrap_or(15);

        // 1. Delegated worker
        if let Ok(summary) = self.ctx.delegates.inspect(&self.ctx.bot_id, target).await {
            let messages = self.ctx.db.list_bot_delegate_messages(target, tail).await?;
            let entries: Vec<ReadAgentEntryView> = messages
                .into_iter()
                .map(|m| ReadAgentEntryView {
                    ts_utc: m.ts_utc,
                    role: m.role,
                    content: m.content,
                })
                .collect();
            return Ok(ToolResult::text(
                ReadAgentTemplate {
                    kind: "delegated worker",
                    id: &summary.agent_id,
                    name: &summary.name,
                    state: summary.status.as_str(),
                    elapsed_ms: summary.elapsed_ms,
                    prompt_tokens: summary.prompt_tokens,
                    completion_tokens: summary.completion_tokens,
                    active_command: summary.active_command.as_deref(),
                    entries: &entries,
                }
                .render()
                .expect("read_agent template must render"),
            ));
        }

        // 2. Session
        if let Some(session) = self.ctx.db.get_session(target).await? {
            let runtime = self.ctx.db.get_session_runtime(target).await?;
            let state = runtime.as_ref().map_or("idle", |r| r.state.as_str());
            let all_msgs = self.ctx.db.get_messages(target).await?;
            let mut prompt_tokens: i64 = 0;
            let mut completion_tokens: i64 = 0;
            let mut elapsed_ms: i64 = 0;
            for m in &all_msgs {
                prompt_tokens =
                    prompt_tokens.saturating_add(i64::from(m.prompt_tokens.unwrap_or(0)));
                completion_tokens =
                    completion_tokens.saturating_add(i64::from(m.completion_tokens.unwrap_or(0)));
                elapsed_ms = elapsed_ms.saturating_add(m.elapsed_ms.unwrap_or(0));
            }
            let tail_usize = usize::try_from(tail).unwrap_or(15);
            let skip = all_msgs.len().saturating_sub(tail_usize);
            let entries: Vec<ReadAgentEntryView> = all_msgs
                .into_iter()
                .skip(skip)
                .map(|m| ReadAgentEntryView {
                    ts_utc: m.timestamp.unwrap_or_else(|| "unknown-time".to_string()),
                    role: m.role,
                    content: m.content,
                })
                .collect();
            let title = session.title.as_deref().unwrap_or(&session.id);
            return Ok(ToolResult::text(
                ReadAgentTemplate {
                    kind: "session",
                    id: &session.id,
                    name: title,
                    state,
                    elapsed_ms,
                    prompt_tokens,
                    completion_tokens,
                    active_command: None,
                    entries: &entries,
                }
                .render()
                .expect("read_agent template must render"),
            ));
        }

        Err(BotToolError::Invalid(format!(
            "no delegated worker or session found matching `{target}`"
        )))
    }
}

// ── 8. create_session ───────────────────────────────────────────

/// Arguments for `create_session(prompt, ...)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateSessionArgs {
    /// Required initial prompt enqueued to wake the new session.
    pub prompt: String,
    /// Optional display title for the session.
    #[serde(default)]
    pub title: Option<String>,
    /// Optional profile id.
    #[serde(default)]
    pub profile_id: Option<String>,
    /// Optional explicit model selector.
    #[serde(default)]
    pub model: Option<String>,
    /// Optional working directory path.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Optional repository binding (`owner/repo`).
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Template)]
#[template(path = "bot/tool_create_session.txt", escape = "none")]
struct CreateSessionTemplate<'a> {
    session_id: &'a str,
    title: Option<&'a str>,
}

/// Creates a user-visible supervised session with a required initial prompt.
#[derive(Debug, Clone)]
pub struct CreateSessionTool {
    ctx: BotToolContext,
}

impl CreateSessionTool {
    /// Creates a `create_session` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for CreateSessionTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("create_session")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Create a user-visible session supervised by this bot, enqueue its required initial prompt, and wake it.",
        )
    }

    type Arguments = CreateSessionArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let prompt = args.prompt.trim();
        if prompt.is_empty() {
            return Err(BotToolError::Invalid(
                "create_session requires a non-empty initial `prompt`".to_string(),
            ));
        }
        let session_id = self
            .ctx
            .sessions
            .create_session(
                self.ctx.bot_id.clone(),
                prompt.to_string(),
                args.title.clone(),
                args.profile_id,
                args.model,
                args.cwd,
                args.repo,
            )
            .await
            .map_err(BotToolError::Invalid)?;

        Ok(ToolResult::text(
            CreateSessionTemplate {
                session_id: &session_id,
                title: args.title.as_deref(),
            }
            .render()
            .expect("create_session template must render"),
        ))
    }
}

// ── 9. stop_session ─────────────────────────────────────────────

/// Arguments for `stop_session(session)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StopSessionArgs {
    /// Session id to stop.
    pub session: String,
}

#[derive(Template)]
#[template(path = "bot/tool_stop_session.txt", escape = "none")]
struct StopSessionTemplate<'a> {
    session_id: &'a str,
}

/// Stops an active run on a session.
#[derive(Debug, Clone)]
pub struct StopSessionTool {
    ctx: BotToolContext,
}

impl StopSessionTool {
    /// Creates a `stop_session` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for StopSessionTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("stop_session")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed("Stop an active run in a session.")
    }

    type Arguments = StopSessionArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let session_id = args.session.trim();
        if session_id.is_empty() {
            return Err(BotToolError::Invalid(
                "`session` must not be empty".to_string(),
            ));
        }
        self.ctx
            .sessions
            .stop_session(session_id.to_string())
            .await
            .map_err(BotToolError::Invalid)?;
        Ok(ToolResult::text(
            StopSessionTemplate { session_id }
                .render()
                .expect("stop_session template must render"),
        ))
    }
}

// ── 10. decide_approval ─────────────────────────────────────────

/// Arguments for `decide_approval(request_id, decision)`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct DecideApprovalArgs {
    /// Pending approval request id.
    pub request_id: String,
    /// Decision (`approve`, `deny { reason }`, or `escalate { note }`).
    pub decision: BotApprovalDecision,
}

#[derive(Template)]
#[template(path = "bot/tool_decide_approval.txt", escape = "none")]
struct DecideApprovalTemplate<'a> {
    request_id: &'a str,
    gate: &'a str,
    requester_id: &'a str,
    outcome: &'a str,
    detail: Option<&'a str>,
}

/// Resolves parked permission requests from delegated workers and supervised sessions.
#[derive(Debug, Clone)]
pub struct DecideApprovalTool {
    ctx: BotToolContext,
}

impl DecideApprovalTool {
    /// Creates a `decide_approval` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for DecideApprovalTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("decide_approval")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Approve, deny (with reason), or escalate (with note for the user) a parked permission request from a delegated worker or supervised session.",
        )
    }

    type Arguments = DecideApprovalArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let (outcome, detail) = match &args.decision {
            BotApprovalDecision::Approve => ("approved", None),
            BotApprovalDecision::Deny { reason } => ("denied", Some(reason.as_str())),
            BotApprovalDecision::Escalate { note } => ("escalated to user", Some(note.as_str())),
        };
        let item = self
            .ctx
            .approvals
            .decide_by_bot(
                &self.ctx.bot_id,
                args.request_id.trim(),
                args.decision.clone(),
            )
            .await?;

        Ok(ToolResult::text(
            DecideApprovalTemplate {
                request_id: &item.request_id,
                gate: item.gate.as_str(),
                requester_id: &item.requester_id,
                outcome,
                detail,
            }
            .render()
            .expect("decide_approval template must render"),
        ))
    }
}

// ── 11. schedule ────────────────────────────────────────────────

/// Arguments for `schedule`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ScheduleArgs {
    /// Create a one-time wake at an RFC 3339 UTC timestamp.
    CreateOnce {
        /// Short name for this schedule.
        name: String,
        /// RFC 3339 UTC fire timestamp (e.g. `2026-07-01T14:00:00Z`).
        run_at_utc: String,
        /// Wake prompt delivered to `bot_inbox`.
        prompt: String,
    },
    /// Create a recurring cron wake (5 or 6 fields, UTC).
    CreateCron {
        /// Short name for this schedule.
        name: String,
        /// Cron expression in UTC (5 or 6 fields).
        cron_utc: String,
        /// Wake prompt delivered to `bot_inbox`.
        prompt: String,
    },
    /// List configured schedules for this bot.
    List,
    /// Delete a schedule by id.
    Delete {
        /// Schedule id to remove.
        id: String,
    },
}

enum ScheduleToolView {
    Created {
        id: String,
        name: String,
        kind: String,
        next_fire_at: Option<String>,
    },
    Deleted {
        id: String,
    },
    Listed {
        items: Vec<BotScheduleItem>,
    },
}

#[derive(Template)]
#[template(path = "bot/tool_schedule.txt", escape = "none")]
struct ScheduleToolTemplate {
    outcome: ScheduleToolView,
}

/// Manages `once` and `cron` proactive wake triggers targeting `bot_inbox`.
#[derive(Debug, Clone)]
pub struct ScheduleTool {
    ctx: BotToolContext,
}

impl ScheduleTool {
    /// Creates a `schedule` tool bound to `ctx`.
    #[must_use]
    pub const fn new(ctx: BotToolContext) -> Self {
        Self { ctx }
    }
}

impl Tool for ScheduleTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("schedule")
    }

    fn description(&self) -> Cow<'static, str> {
        Cow::Borrowed(
            "Create (once or cron), list, or delete proactive wake schedules that deliver wake prompts to this bot's inbox.",
        )
    }

    type Arguments = ScheduleArgs;
    type Res = ToolResult;
    type Error = BotToolError;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> Result<Self::Res, Self::Error> {
        let outcome = match args {
            ScheduleArgs::CreateOnce {
                name,
                run_at_utc,
                prompt,
            } => {
                let ts = jiff::Timestamp::from_str(run_at_utc.trim()).map_err(|error| {
                    BotToolError::Invalid(format!("invalid run_at_utc: {error}"))
                })?;
                let id = crate::id::new();
                let run_at_str = ts.to_string();
                let item = self
                    .ctx
                    .db
                    .create_bot_schedule(NewBotSchedule {
                        id: &id,
                        bot_id: &self.ctx.bot_id,
                        name: name.trim(),
                        kind: "once",
                        cron_expression: None,
                        run_at: Some(&run_at_str),
                        prompt: prompt.trim(),
                        next_fire_at: Some(&run_at_str),
                    })
                    .await?;
                self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);
                ScheduleToolView::Created {
                    id: item.id,
                    name: item.name,
                    kind: item.kind,
                    next_fire_at: item.next_fire_at,
                }
            }
            ScheduleArgs::CreateCron {
                name,
                cron_utc,
                prompt,
            } => {
                let canonical =
                    normalize_cron_expression(cron_utc.trim()).map_err(BotToolError::Invalid)?;
                let next = TimeTriggerType::Cron
                    .next_fire(Some(&canonical), None, jiff::Timestamp::now())
                    .map_err(BotToolError::Invalid)?;
                let id = crate::id::new();
                let next_str = next.to_string();
                let item = self
                    .ctx
                    .db
                    .create_bot_schedule(NewBotSchedule {
                        id: &id,
                        bot_id: &self.ctx.bot_id,
                        name: name.trim(),
                        kind: "cron",
                        cron_expression: Some(&canonical),
                        run_at: None,
                        prompt: prompt.trim(),
                        next_fire_at: Some(&next_str),
                    })
                    .await?;
                self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);
                ScheduleToolView::Created {
                    id: item.id,
                    name: item.name,
                    kind: item.kind,
                    next_fire_at: item.next_fire_at,
                }
            }
            ScheduleArgs::List => {
                let items = self.ctx.db.list_bot_schedules(&self.ctx.bot_id).await?;
                ScheduleToolView::Listed { items }
            }
            ScheduleArgs::Delete { id } => {
                let deleted = self
                    .ctx
                    .db
                    .delete_bot_schedule(&self.ctx.bot_id, id.trim())
                    .await?;
                if !deleted {
                    return Err(BotToolError::Invalid(format!(
                        "schedule `{}` not found",
                        id.trim()
                    )));
                }
                self.ctx.ui_event_bus.publish(UiEventTopics::BOTS);
                ScheduleToolView::Deleted {
                    id: id.trim().to_string(),
                }
            }
        };

        Ok(ToolResult::text(
            ScheduleToolTemplate { outcome }
                .render()
                .expect("schedule template must render"),
        ))
    }
}
