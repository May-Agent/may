//! Resident May Bot runtime loop, event-driven inbox drain, `LiveContent`
//! memory index updates, `Moment::FinalTurn` reply guard, and schedule ticker.

use std::collections::HashMap;
use std::future::Future;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aither_agent::{
    Agent, AgentCheckpoint, AgentEvent, ContextBlock, ContextSource, ExternalContent, Hooks,
    Moment, RunOutcome, SourceId, TurnBoundaryAction, TurnBoundaryContext,
};
use aither_core::LanguageModel;
use aither_core::llm::Message;
use aither_core::llm::model::Parameters;
use arc_swap::ArcSwapOption;
use askama::Template;
use may_db::{BotRow, ChatDb, NewBotInboxEntry, NewBotRow, UpdateBotRow};
use may_harness::task_trigger::TimeTriggerType;
use may_protocol::bot::{
    BotApprovalDecision, BotApprovalItem, BotChatMessage, BotDelegateSummary, BotInboxEntry,
    BotInboxKind, BotMemoryCommit, BotMemoryNode, BotRunState, BotScheduleItem, BotSseEvent,
    BotSummary, BotTraceEntry, CreateBotRequest, SeenMemoryLedger, UpdateBotRequest,
    format_monotonic_utc,
};
use may_protocol::context_tag::tag;
use may_protocol::session_layout::MayHomePaths;
use may_repo_search::CollectionBackend;

use super::approvals::{BotApprovalError, BotApprovalRouter, BotEventPublishFn, BotWakeFn};
use super::delegates::{BotDelegateManager, DelegateError, DelegateRunner};
use super::memory::{
    BOT_MEMORY_INDEX_CONTENT, BOT_MEMORY_INDEX_SOURCE, BotMemoryCuratorQueue, BotMemoryError,
    BotMemoryRag, CuratorFactory, ensure_bot_memory_initialized, list_memory_commits_sync,
    read_memory_tree, render_hierarchical_memory_index, reset_seen_window,
};
use super::tools::{
    BotToolContext, CreateSessionTool, DecideApprovalTool, DelegateAgentTool, ReadAgentTool,
    RecallTool, ResumeAgentTool, ScheduleTool, SendMessageTool, SessionBridge, StopAgentTool,
    StopSessionTool, StoreFactTool,
};
use crate::agent_link::AgentMessaging;
use crate::notification::NotificationService;
use crate::sleep::SleepState;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// Maximum number of RAG memory hits injected per inbox event.
const MAX_RAG_HITS: usize = 5;

/// Poll interval for the bot schedule ticker.
const SCHEDULE_TICK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// Future returned by [`BotModelFactory::build_model`].
pub type BotModelFuture<'a, LLM> =
    Pin<Box<dyn Future<Output = anyhow::Result<(LLM, Parameters)>> + Send + 'a>>;

/// Resolves the LLM and request parameters for a resident bot.
pub trait BotModelFactory<LLM: LanguageModel + Clone + Sync + 'static>:
    Send + Sync + 'static
{
    /// Builds the model handle and default request parameters for `bot`.
    fn build_model(&self, bot: &BotRow) -> BotModelFuture<'_, LLM>;
}

/// Errors returned by [`BotService`] operations.
#[derive(Debug, thiserror::Error)]
pub enum BotServiceError {
    /// Bot was not found.
    #[error("bot `{0}` not found")]
    NotFound(String),
    /// Invalid request arguments.
    #[error("{0}")]
    Invalid(String),
    /// Database failure.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
    /// Memory failure.
    #[error(transparent)]
    Memory(#[from] BotMemoryError),
    /// Approval failure.
    #[error(transparent)]
    Approval(#[from] BotApprovalError),
    /// Delegate failure.
    #[error(transparent)]
    Delegate(#[from] DelegateError),
}

// ── Askama Templates ────────────────────────────────────────────

#[derive(Template)]
#[template(path = "bot/prompt_identity.md", escape = "none")]
struct BotIdentityTemplate<'a> {
    name: &'a str,
    agent_id: &'a str,
}

#[derive(Template)]
#[template(path = "bot/prompt_persona.md", escape = "none")]
struct BotPersonaTemplate<'a> {
    persona: &'a str,
}

#[derive(Template)]
#[template(path = "bot/prompt_tools.md", escape = "none")]
struct BotToolsTemplate;

#[derive(Template)]
#[template(path = "bot/prompt_time.md", escape = "none")]
struct BotTimeTemplate<'a> {
    timezone: &'a str,
}

struct RenderedInboxItem {
    ts_utc: String,
    kind_label: &'static str,
    sender_label: Option<String>,
    body: String,
}

#[derive(Template)]
#[template(path = "bot/inbox_batch.txt", escape = "none")]
struct InboxBatchTemplate<'a> {
    items: &'a [RenderedInboxItem],
}

#[derive(Template)]
#[template(path = "bot/final_turn_reply_reminder.txt", escape = "none")]
struct FinalTurnReplyReminderTemplate;

const fn inbox_kind_label(kind: BotInboxKind) -> &'static str {
    match kind {
        BotInboxKind::UserMessage => "User message",
        BotInboxKind::AgentMessage => "Agent message",
        BotInboxKind::DelegateCompleted => "Delegated worker completed",
        BotInboxKind::DelegateFailed => "Delegated worker failed",
        BotInboxKind::SessionEvent => "Supervised session event",
        BotInboxKind::ApprovalRequest => "Permission approval request",
        BotInboxKind::ScheduleWake => "Scheduled wake",
        BotInboxKind::TaskWake => "Task wake",
        BotInboxKind::WebhookWake => "Webhook wake",
    }
}

fn render_inbox_batch(entries: &[BotInboxEntry]) -> String {
    let items: Vec<RenderedInboxItem> = entries
        .iter()
        .map(|entry| {
            let sender_label = match (&entry.sender_name, &entry.sender_id) {
                (Some(name), Some(id)) if name != id => Some(format!("{name} ({id})")),
                (Some(name), _) => Some(name.clone()),
                (None, Some(id)) => Some(id.clone()),
                (None, None) => None,
            };
            RenderedInboxItem {
                ts_utc: entry.ts_utc.clone(),
                kind_label: inbox_kind_label(entry.kind),
                sender_label,
                body: entry.body.clone(),
            }
        })
        .collect();
    InboxBatchTemplate { items: &items }
        .render()
        .expect("inbox batch template must render")
}

// ── Hooks & ContextSource ───────────────────────────────────────

/// Hooks for a resident bot: interrupts cleanly at `on_turn_boundary` whenever
/// newly arrived `bot_inbox` entries are waiting, so thinking blocks stay
/// intact while the outer loop drains those entries and resumes.
struct BotHooks {
    bot_id: String,
    db: ChatDb,
    paused_for_inbox: Arc<AtomicBool>,
}

impl Hooks for BotHooks {
    fn on_turn_boundary(
        &self,
        _cx: &TurnBoundaryContext<'_>,
    ) -> impl Future<Output = TurnBoundaryAction> + Send {
        let db = self.db.clone();
        let bot_id = self.bot_id.clone();
        let paused = self.paused_for_inbox.clone();
        async move {
            let pending = db.count_unconsumed_bot_inbox(&bot_id).await.unwrap_or(0);
            if pending > 0 {
                paused.store(true, Ordering::Release);
                TurnBoundaryAction::EndTurn
            } else {
                TurnBoundaryAction::Continue
            }
        }
    }
}

/// Shared per-run state observed by [`BotContextSource`].
#[derive(Debug)]
struct BotRunContextState {
    consumed_user_message: bool,
    reminded_once: bool,
    pending_memory_recall: Option<String>,
}

/// Context source injecting `<memory-recall>` at turn tail, resetting
/// `SeenMemoryLedger` on compaction (`reassembled == true`), and enforcing the
/// one-shot `Moment::FinalTurn` user-reply guard.
struct BotContextSource {
    seen_memory: Arc<Mutex<SeenMemoryLedger>>,
    sent_to_user_flag: Arc<AtomicBool>,
    run_state: Arc<Mutex<BotRunContextState>>,
}

impl ContextSource for BotContextSource {
    fn id(&self) -> SourceId {
        SourceId::new("may_bot_context")
    }

    fn blocks(&mut self, moment: Moment) -> impl Future<Output = Vec<ContextBlock>> + Send {
        let blocks = match moment {
            Moment::Request { reassembled } => {
                if reassembled && let Ok(mut ledger) = self.seen_memory.lock() {
                    reset_seen_window(&mut ledger);
                }
                let recall = self
                    .run_state
                    .lock()
                    .ok()
                    .and_then(|mut s| s.pending_memory_recall.take());
                recall
                    .into_iter()
                    .map(|text| ContextBlock::tail("memory-recall", text))
                    .collect()
            }
            Moment::FinalTurn => {
                let should_remind = self.run_state.lock().is_ok_and(|mut s| {
                    if s.consumed_user_message
                        && !self.sent_to_user_flag.load(Ordering::Acquire)
                        && !s.reminded_once
                    {
                        s.reminded_once = true;
                        true
                    } else {
                        false
                    }
                });
                if should_remind {
                    vec![ContextBlock::stable(
                        "system-reminder",
                        FinalTurnReplyReminderTemplate
                            .render()
                            .expect("final turn reminder template must render"),
                    )]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        };
        std::future::ready(blocks)
    }

    fn peek(&self, moment: Moment) -> impl Future<Output = Vec<ContextBlock>> + Send {
        let blocks = match moment {
            Moment::Request { .. } => self
                .run_state
                .lock()
                .ok()
                .and_then(|s| s.pending_memory_recall.clone())
                .into_iter()
                .map(|text| ContextBlock::tail("memory-recall", text))
                .collect(),
            _ => Vec::new(),
        };
        std::future::ready(blocks)
    }
}

// ── Per-Bot Loop Handle ─────────────────────────────────────────

struct BotLoopHandle {
    wake_tx: async_channel::Sender<()>,
    stop_tx: async_channel::Sender<()>,
    done_rx: async_channel::Receiver<()>,
    rebuild_needed: Arc<AtomicBool>,
    run_state: Arc<Mutex<BotRunState>>,
}

/// Coordinates all resident May Bots, their event-driven inbox loops,
/// delegated workers, approval triage, hierarchical memory curation, RAG, and
/// proactive schedules.
pub struct BotService<LLM, B>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    B: CollectionBackend + Clone + 'static,
{
    db: ChatDb,
    may_dir: PathBuf,
    model_factory: Arc<dyn BotModelFactory<LLM>>,
    curator_factory: Arc<dyn CuratorFactory>,
    rag: BotMemoryRag<B>,
    approvals: BotApprovalRouter,
    delegates: BotDelegateManager,
    sessions: Arc<dyn SessionBridge>,
    messaging: Arc<ArcSwapOption<AgentMessaging>>,
    notifications: Option<Arc<NotificationService>>,
    sleep: Option<Arc<SleepState>>,
    ui_event_bus: UiEventBus,
    publish_event: BotEventPublishFn,
    wake_bot_fn: BotWakeFn,
    loops: Arc<Mutex<HashMap<String, BotLoopHandle>>>,
}

impl<LLM, B> std::fmt::Debug for BotService<LLM, B>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    B: CollectionBackend + Clone + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotService")
            .field("may_dir", &self.may_dir)
            .finish_non_exhaustive()
    }
}

/// Construction arguments for [`BotService::new`].
pub struct BotServiceDeps<LLM, B>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    B: CollectionBackend + Clone + 'static,
{
    /// Shared SQLite database.
    pub db: ChatDb,
    /// `~/.may` root directory.
    pub may_dir: PathBuf,
    /// Model factory for bots.
    pub model_factory: Arc<dyn BotModelFactory<LLM>>,
    /// Confined curator factory for `store_fact`.
    pub curator_factory: Arc<dyn CuratorFactory>,
    /// Delegated worker runner.
    pub delegate_runner: Arc<dyn DelegateRunner>,
    /// `repo-search` backend for hybrid memory RAG.
    pub search_backend: B,
    /// Session bridge (`create_session` / `stop_session`).
    pub sessions: Arc<dyn SessionBridge>,
    /// Optional session runtime service (for `UserWait` on escalated approvals).
    pub runtime_service: Option<Arc<crate::session::SessionRuntimeService>>,
    /// Optional web-push notification service.
    pub notifications: Option<Arc<NotificationService>>,
    /// Optional daemon-wide sleep state.
    pub sleep: Option<Arc<SleepState>>,
    /// UI event bus.
    pub ui_event_bus: UiEventBus,
    /// Live SSE event publisher (`bot_*` on `/api/events`).
    pub publish_event: BotEventPublishFn,
}

impl<LLM, B> std::fmt::Debug for BotServiceDeps<LLM, B>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    B: CollectionBackend + Clone + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotServiceDeps").finish_non_exhaustive()
    }
}

impl<LLM, B> BotService<LLM, B>
where
    LLM: LanguageModel + Clone + Sync + 'static,
    B: CollectionBackend + Clone + 'static,
{
    /// Builds a [`BotService`] and spawns its central wake router.
    #[must_use]
    pub fn new(deps: BotServiceDeps<LLM, B>) -> Arc<Self> {
        let (central_wake_tx, central_wake_rx) = async_channel::unbounded::<String>();
        let wake_bot_fn: BotWakeFn = Arc::new(move |bot_id: &str| {
            let _ = central_wake_tx.try_send(bot_id.to_string());
        });

        let mut approvals = BotApprovalRouter::new(
            deps.db.clone(),
            deps.ui_event_bus.clone(),
            wake_bot_fn.clone(),
            deps.publish_event.clone(),
        );
        if let Some(runtime) = deps.runtime_service {
            approvals = approvals.with_runtime_service(runtime);
        }
        if let Some(notifications) = deps.notifications.clone() {
            approvals = approvals.with_notifications(notifications);
        }
        if let Some(sleep) = deps.sleep.clone() {
            approvals = approvals.with_sleep(sleep);
        }

        let delegates = BotDelegateManager::new(
            deps.db.clone(),
            deps.may_dir.clone(),
            approvals.clone(),
            deps.delegate_runner,
            wake_bot_fn.clone(),
        );

        let loops: Arc<Mutex<HashMap<String, BotLoopHandle>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let service = Arc::new(Self {
            db: deps.db,
            may_dir: deps.may_dir,
            model_factory: deps.model_factory,
            curator_factory: deps.curator_factory,
            rag: BotMemoryRag::new(deps.search_backend),
            approvals,
            delegates,
            sessions: deps.sessions,
            messaging: Arc::new(ArcSwapOption::empty()),
            notifications: deps.notifications,
            sleep: deps.sleep,
            ui_event_bus: deps.ui_event_bus,
            publish_event: deps.publish_event,
            wake_bot_fn,
            loops: loops.clone(),
        });

        tokio::spawn(async move {
            while let Ok(bot_id) = central_wake_rx.recv().await {
                if let Ok(guard) = loops.lock()
                    && let Some(handle) = guard.get(&bot_id)
                {
                    let _ = handle.wake_tx.try_send(());
                }
            }
        });

        service
    }

    /// Attaches [`AgentMessaging`] after construction (breaking the daemon init cycle).
    pub fn set_messaging(&self, messaging: AgentMessaging) {
        self.messaging.store(Some(Arc::new(messaging)));
    }

    /// Returns the shared [`BotApprovalRouter`].
    #[must_use]
    pub fn approvals(&self) -> BotApprovalRouter {
        self.approvals.clone()
    }

    /// Returns the shared [`BotDelegateManager`].
    #[must_use]
    pub fn delegates(&self) -> BotDelegateManager {
        self.delegates.clone()
    }

    /// Returns the shared wake callback for wiring into `AgentDirectory` / `SessionPrompts`.
    #[must_use]
    pub fn wake_callback(&self) -> BotWakeFn {
        self.wake_bot_fn.clone()
    }

    /// Wakes `bot_id`'s resident loop.
    pub fn wake_bot(&self, bot_id: &str) {
        (self.wake_bot_fn)(bot_id);
    }

    /// Starts resident loops for all enabled bots persisted in SQLite.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if bots cannot be listed.
    pub async fn start_all(self: &Arc<Self>) -> Result<(), BotServiceError> {
        let bots = self.db.list_bots().await?;
        for bot in bots {
            if bot.enabled {
                self.ensure_bot_loop_started(&bot).await?;
            }
        }
        Ok(())
    }

    /// Stops every active resident bot loop and waits for each loop task to exit cleanly.
    pub async fn shutdown(&self) {
        let handles: Vec<BotLoopHandle> = {
            let Ok(mut guard) = self.loops.lock() else {
                return;
            };
            guard.drain().map(|(_, handle)| handle).collect()
        };
        for handle in &handles {
            let _ = handle.stop_tx.try_send(());
            handle.stop_tx.close();
        }
        for handle in handles {
            let _ = handle.done_rx.recv().await;
        }
    }

    /// Spawns the background proactive schedule ticker.
    pub fn spawn_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            loop {
                let _ = this.tick_schedules_once(jiff::Timestamp::now()).await;
                async_io::Timer::after(SCHEDULE_TICK_INTERVAL).await;
            }
        })
    }

    /// Runs a single pass of the bot schedule ticker at `now`, firing every due
    /// `once` and `cron` schedule into `bot_inbox` and waking each bot.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn tick_schedules_once(
        &self,
        now: jiff::Timestamp,
    ) -> Result<usize, BotServiceError> {
        let now_iso = format_monotonic_utc(now.as_millisecond());
        let due = self.db.list_due_bot_schedules(&now_iso).await?;
        let mut fired = 0usize;
        for sched in due {
            let Some(bot) = self.db.get_bot(&sched.bot_id).await? else {
                continue;
            };
            if !bot.enabled {
                continue;
            }
            let body = format!(
                "Scheduled wake `{}` ({}, {}) fired at {}:\n{}",
                sched.id, sched.name, sched.kind, now_iso, sched.prompt
            );
            let _ = self
                .db
                .enqueue_bot_inbox(NewBotInboxEntry {
                    bot_id: &sched.bot_id,
                    kind: BotInboxKind::ScheduleWake,
                    sender_id: Some(&sched.id),
                    sender_name: Some(&sched.name),
                    body: &body,
                    meta_json: None,
                })
                .await?;
            if sched.kind == "once" {
                self.db
                    .advance_bot_schedule(&sched.id, &now_iso, None, false)
                    .await?;
            } else if let Some(expr) = sched.cron_expression.as_deref() {
                let next_iso = TimeTriggerType::Cron
                    .next_fire(Some(expr), None, now)
                    .ok()
                    .map(|ts| format_monotonic_utc(ts.as_millisecond()));
                let keep_enabled = next_iso.is_some();
                self.db
                    .advance_bot_schedule(&sched.id, &now_iso, next_iso.as_deref(), keep_enabled)
                    .await?;
            } else {
                self.db
                    .advance_bot_schedule(&sched.id, &now_iso, None, false)
                    .await?;
            }
            self.wake_bot(&sched.bot_id);
            fired += 1;
        }
        Ok(fired)
    }

    /// Creates a new resident May Bot, initializes its `memory/` and `memory.git`,
    /// starts its loop when `enabled`, and publishes a `bot_state` event.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on invalid name or DB/filesystem error.
    pub async fn create_bot(
        self: &Arc<Self>,
        req: CreateBotRequest,
    ) -> Result<BotSummary, BotServiceError> {
        let name = req.name.trim();
        if name.is_empty() {
            return Err(BotServiceError::Invalid(
                "bot name must not be empty".to_string(),
            ));
        }
        let id = crate::id::new();
        let agent_id = crate::id::new();
        let row = self
            .db
            .create_bot(NewBotRow {
                id: &id,
                agent_id: &agent_id,
                name,
                persona: req.persona.as_deref(),
                model: req.model.as_deref(),
                profile_id: req.profile_id.as_deref(),
                enabled: req.enabled,
                approval_policy: &req.approval_policy,
            })
            .await?;

        let paths = MayHomePaths::new(self.may_dir.clone()).bot_paths(&row.id);
        ensure_bot_memory_initialized(&paths).await?;

        if row.enabled {
            self.ensure_bot_loop_started(&row).await?;
        }
        let summary = self.row_to_summary(row).await?;
        self.emit_state().await;
        Ok(summary)
    }

    /// Lists all bots with live runtime status and counters.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_bots(&self) -> Result<Vec<BotSummary>, BotServiceError> {
        let rows = self.db.list_bots().await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(self.row_to_summary(row).await?);
        }
        Ok(out)
    }

    /// Returns one bot by id, `agent_id`, or name.
    ///
    /// # Errors
    /// Returns [`BotServiceError::NotFound`] if missing.
    pub async fn get_bot(&self, id_or_name: &str) -> Result<BotSummary, BotServiceError> {
        let row = self.resolve_bot_row(id_or_name).await?;
        self.row_to_summary(row).await
    }

    /// Updates a bot's live configuration (name, persona, model, approval policy,
    /// enabled state) without restarting the daemon.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if not found or storage fails.
    pub async fn update_bot(
        self: &Arc<Self>,
        bot_id: &str,
        req: UpdateBotRequest,
    ) -> Result<BotSummary, BotServiceError> {
        let existing = self.resolve_bot_row(bot_id).await?;
        let needs_rebuild = req.name.is_some() || req.persona.is_some() || req.model.is_some();

        let updated = self
            .db
            .update_bot(
                &existing.id,
                UpdateBotRow {
                    name: req.name.as_deref(),
                    persona: req.persona.as_ref().map(|o| o.as_deref()),
                    model: req.model.as_ref().map(|o| o.as_deref()),
                    enabled: req.enabled,
                    approval_policy: req.approval_policy.as_ref(),
                },
            )
            .await?
            .ok_or_else(|| BotServiceError::NotFound(bot_id.to_string()))?;

        if updated.enabled {
            self.ensure_bot_loop_started(&updated).await?;
            if needs_rebuild
                && let Ok(guard) = self.loops.lock()
                && let Some(handle) = guard.get(&updated.id)
            {
                handle.rebuild_needed.store(true, Ordering::Release);
                let _ = handle.wake_tx.try_send(());
            }
        } else {
            self.stop_bot_loop(&updated.id);
        }

        let summary = self.row_to_summary(updated).await?;
        self.emit_state().await;
        Ok(summary)
    }

    /// Deletes a bot, stopping its resident loop.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if not found or storage fails.
    pub async fn delete_bot(&self, bot_id: &str) -> Result<(), BotServiceError> {
        let existing = self.resolve_bot_row(bot_id).await?;
        self.stop_bot_loop(&existing.id);
        let _ = self.db.delete_bot(&existing.id).await?;
        self.emit_state().await;
        Ok(())
    }

    /// Enqueues an inbound user message into `bot_inbox` with a strictly
    /// monotonic UTC timestamp, records it in user conversation history, emits
    /// `bot_chat`, and wakes the bot.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if bot not found, empty message, or DB error.
    pub async fn send_user_message(
        self: &Arc<Self>,
        bot_id: &str,
        content: &str,
    ) -> Result<(BotChatMessage, BotInboxEntry), BotServiceError> {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return Err(BotServiceError::Invalid(
                "message must not be empty".to_string(),
            ));
        }
        let bot = self.resolve_bot_row(bot_id).await?;
        if bot.enabled {
            self.ensure_bot_loop_started(&bot).await?;
        }
        let inbox = self
            .db
            .enqueue_bot_inbox(NewBotInboxEntry {
                bot_id: &bot.id,
                kind: BotInboxKind::UserMessage,
                sender_id: Some("user"),
                sender_name: Some("user"),
                body: trimmed,
                meta_json: None,
            })
            .await?;
        let chat = self
            .db
            .append_bot_chat_message(
                &bot.id,
                "user",
                trimmed,
                inbox.ts_ms,
                &inbox.ts_utc,
                Some(inbox.id),
            )
            .await?;
        (self.publish_event)(BotSseEvent::BotChat {
            message: chat.clone(),
        });
        self.ui_event_bus.publish(UiEventTopics::BOTS);
        self.wake_bot(&bot.id);
        Ok((chat, inbox))
    }

    /// Lists user-visible chat messages for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_messages(
        &self,
        bot_id: &str,
        limit: usize,
    ) -> Result<Vec<BotChatMessage>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let lim = i64::try_from(limit).unwrap_or(200);
        Ok(self.db.list_bot_chat_messages(&bot.id, lim).await?)
    }

    /// Lists internal activity trace entries for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_trace(
        &self,
        bot_id: &str,
        limit: usize,
    ) -> Result<Vec<BotTraceEntry>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let lim = i64::try_from(limit).unwrap_or(200);
        Ok(self.db.list_bot_trace_entries(&bot.id, lim).await?)
    }

    /// Lists recent inbox entries for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_inbox(
        &self,
        bot_id: &str,
        limit: usize,
    ) -> Result<Vec<BotInboxEntry>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let lim = i64::try_from(limit).unwrap_or(200);
        Ok(self.db.list_bot_inbox(&bot.id, lim).await?)
    }

    /// Lists delegated workers owned by `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_delegates(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotDelegateSummary>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self.db.list_bot_delegates(&bot.id).await?)
    }

    /// Stops a delegated worker owned by `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if missing or unauthorized.
    pub async fn stop_delegate(
        &self,
        bot_id: &str,
        agent_id: &str,
    ) -> Result<BotDelegateSummary, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self.delegates.stop(&bot.id, agent_id).await?)
    }

    /// Lists permission approval requests for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_approvals(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotApprovalItem>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self.db.list_bot_approvals(&bot.id).await?)
    }

    /// Resolves an escalated/pending approval via user action (REST / TUI / Web UI).
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if missing or already resolved.
    pub async fn resolve_approval_by_user(
        &self,
        bot_id: &str,
        request_id: &str,
        approved: bool,
        reason: Option<String>,
    ) -> Result<BotApprovalItem, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let existing = self
            .db
            .get_bot_approval(request_id)
            .await?
            .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
        if existing.bot_id != bot.id {
            return Err(BotApprovalError::Unauthorized(request_id.to_string()).into());
        }
        Ok(self
            .approvals
            .resolve_by_user(request_id, approved, reason)
            .await?)
    }

    /// Resolves an approval directly via bot decision.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] if missing or already resolved.
    pub async fn decide_approval_by_bot(
        &self,
        bot_id: &str,
        request_id: &str,
        decision: BotApprovalDecision,
    ) -> Result<BotApprovalItem, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self
            .approvals
            .decide_by_bot(&bot.id, request_id, decision)
            .await?)
    }

    /// Lists proactive schedules for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn list_schedules(
        &self,
        bot_id: &str,
    ) -> Result<Vec<BotScheduleItem>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self.db.list_bot_schedules(&bot.id).await?)
    }

    /// Deletes a proactive schedule belonging to `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on storage error.
    pub async fn delete_schedule(
        &self,
        bot_id: &str,
        schedule_id: &str,
    ) -> Result<bool, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        Ok(self.db.delete_bot_schedule(&bot.id, schedule_id).await?)
    }

    /// Reads the bot's hierarchical memory tree (`memory/`).
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on filesystem error.
    pub async fn memory_tree(&self, bot_id: &str) -> Result<BotMemoryNode, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let paths = MayHomePaths::new(self.may_dir.clone()).bot_paths(&bot.id);
        ensure_bot_memory_initialized(&paths).await?;
        let root = paths.memory_dir();
        Ok(blocking::unblock(move || read_memory_tree(&root)).await?)
    }

    /// Lists `memory.git` commit history (newest first) for `bot_id`.
    ///
    /// # Errors
    /// Returns [`BotServiceError`] on git error.
    pub async fn memory_history(
        &self,
        bot_id: &str,
        limit: usize,
    ) -> Result<Vec<BotMemoryCommit>, BotServiceError> {
        let bot = self.resolve_bot_row(bot_id).await?;
        let paths = MayHomePaths::new(self.may_dir.clone()).bot_paths(&bot.id);
        ensure_bot_memory_initialized(&paths).await?;
        let git_dir = paths.memory_git_dir();
        Ok(blocking::unblock(move || list_memory_commits_sync(&git_dir, limit)).await?)
    }

    /// Current live [`BotRunState`] for `bot_id`.
    #[must_use]
    pub fn run_state_of(&self, bot_id: &str) -> BotRunState {
        self.loops
            .lock()
            .ok()
            .and_then(|guard| {
                guard
                    .get(bot_id)
                    .and_then(|h| h.run_state.lock().ok().map(|s| *s))
            })
            .unwrap_or(BotRunState::Idle)
    }

    async fn resolve_bot_row(&self, id_or_name: &str) -> Result<BotRow, BotServiceError> {
        self.db
            .find_bot_by_identifier(id_or_name)
            .await?
            .ok_or_else(|| BotServiceError::NotFound(id_or_name.to_string()))
    }

    async fn row_to_summary(&self, row: BotRow) -> Result<BotSummary, BotServiceError> {
        let run_state = if row.enabled {
            self.run_state_of(&row.id)
        } else {
            BotRunState::Disabled
        };
        let pending_inbox = self.db.count_unconsumed_bot_inbox(&row.id).await?;
        let delegates = self.db.list_bot_delegates(&row.id).await?;
        let active_delegates = delegates
            .iter()
            .filter(|d| d.status == may_protocol::bot::BotDelegateStatus::Running)
            .count();
        let approvals = self.db.list_bot_approvals(&row.id).await?;
        let pending_approvals = approvals
            .iter()
            .filter(|a| {
                matches!(
                    a.status,
                    may_protocol::bot::BotApprovalStatus::PendingBot
                        | may_protocol::bot::BotApprovalStatus::Escalated
                )
            })
            .count();
        Ok(BotSummary {
            id: row.id,
            agent_id: row.agent_id,
            name: row.name,
            persona: row.persona,
            model: row.model,
            profile_id: row.profile_id,
            enabled: row.enabled,
            run_state,
            approval_policy: row.approval_policy,
            pending_inbox,
            pending_approvals,
            active_delegates,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }

    async fn emit_state(&self) {
        if let Ok(bots) = self.list_bots().await {
            (self.publish_event)(BotSseEvent::BotState { bots });
        }
        self.ui_event_bus.publish(UiEventTopics::BOTS);
    }

    fn stop_bot_loop(&self, bot_id: &str) {
        if let Ok(mut guard) = self.loops.lock()
            && let Some(handle) = guard.remove(bot_id)
        {
            let _ = handle.stop_tx.try_send(());
            handle.stop_tx.close();
        }
    }

    async fn ensure_bot_loop_started(
        self: &Arc<Self>,
        bot: &BotRow,
    ) -> Result<(), BotServiceError> {
        if self
            .loops
            .lock()
            .expect("bot loops lock")
            .contains_key(&bot.id)
        {
            return Ok(());
        }

        let paths = MayHomePaths::new(self.may_dir.clone()).bot_paths(&bot.id);
        ensure_bot_memory_initialized(&paths).await?;

        let (wake_tx, wake_rx) = async_channel::unbounded();
        let (stop_tx, stop_rx) = async_channel::bounded(1);
        let (done_tx, done_rx) = async_channel::bounded(1);
        let index_dirty = Arc::new(AtomicBool::new(false));
        let rebuild_needed = Arc::new(AtomicBool::new(false));
        let run_state = Arc::new(Mutex::new(BotRunState::Idle));

        let curator = {
            let dirty = index_dirty.clone();
            let publish = self.publish_event.clone();
            let ui_bus = self.ui_event_bus.clone();
            let wake_tx_c = wake_tx.clone();
            BotMemoryCuratorQueue::spawn(
                self.db.clone(),
                bot.id.clone(),
                bot.agent_id.clone(),
                paths.clone(),
                self.curator_factory.clone(),
                self.rag.clone(),
                move |committed_bot_id, sha| {
                    dirty.store(true, Ordering::Release);
                    let _ = wake_tx_c.try_send(());
                    (publish)(BotSseEvent::BotMemory {
                        bot_id: committed_bot_id,
                        commit_sha: sha,
                    });
                    ui_bus.publish(UiEventTopics::BOTS);
                },
            )
        };

        let handle = BotLoopHandle {
            wake_tx: wake_tx.clone(),
            stop_tx,
            done_rx,
            rebuild_needed: rebuild_needed.clone(),
            run_state: run_state.clone(),
        };

        {
            let mut guard = self.loops.lock().expect("bot loops lock");
            if guard.contains_key(&bot.id) {
                return Ok(());
            }
            guard.insert(bot.id.clone(), handle);
        }

        let this = self.clone();
        let bot_id = bot.id.clone();
        let curator_for_loop = curator.clone();
        tokio::spawn(async move {
            let _done_guard = done_tx;
            this.run_bot_loop(
                bot_id,
                paths,
                curator_for_loop,
                wake_rx,
                stop_rx,
                index_dirty,
                rebuild_needed,
                run_state,
            )
            .await;
        });

        let _ = wake_tx.try_send(());
        curator.wake();
        Ok(())
    }

    #[allow(clippy::too_many_arguments, clippy::too_many_lines)]
    async fn run_bot_loop(
        self: Arc<Self>,
        bot_id: String,
        paths: may_protocol::session_layout::BotPaths,
        curator: BotMemoryCuratorQueue,
        wake_rx: async_channel::Receiver<()>,
        stop_rx: async_channel::Receiver<()>,
        index_dirty: Arc<AtomicBool>,
        rebuild_needed: Arc<AtomicBool>,
        run_state: Arc<Mutex<BotRunState>>,
    ) {
        let seen_memory: Arc<Mutex<SeenMemoryLedger>> =
            Arc::new(Mutex::new(SeenMemoryLedger::default()));
        let sent_to_user_flag = Arc::new(AtomicBool::new(false));
        let paused_for_inbox = Arc::new(AtomicBool::new(false));
        let run_ctx_state = Arc::new(Mutex::new(BotRunContextState {
            consumed_user_message: false,
            reminded_once: false,
            pending_memory_recall: None,
        }));

        let initial_hashes = HashMap::new();
        let _ = self
            .rag
            .sync_incremental(&bot_id, &initial_hashes, &paths.memory_dir())
            .await;

        let mut agent: Option<Agent<LLM, BotHooks, LLM>> = None;

        loop {
            if stop_rx.is_closed() || stop_rx.try_recv().is_ok() {
                break;
            }

            let Some(bot_row) = self.db.get_bot(&bot_id).await.ok().flatten() else {
                break;
            };
            if !bot_row.enabled {
                break;
            }

            // Refresh LiveContent memory index whenever curator committed a new version.
            if index_dirty.swap(false, Ordering::AcqRel)
                && let Some(active_agent) = agent.as_mut()
            {
                let mem_root = paths.memory_dir();
                if let Ok(index_text) =
                    blocking::unblock(move || render_hierarchical_memory_index(&mem_root)).await
                {
                    active_agent
                        .context_mut()
                        .update_live(&BOT_MEMORY_INDEX_CONTENT, |update| {
                            update.external(
                                &tag::BOT_MEMORY_INDEX,
                                Some(ExternalContent::data(BOT_MEMORY_INDEX_SOURCE, index_text)),
                            );
                        });
                }
            }

            let pending_count = self
                .db
                .count_unconsumed_bot_inbox(&bot_id)
                .await
                .unwrap_or(0);
            if pending_count == 0 {
                futures_lite::future::or(
                    async {
                        let _ = wake_rx.recv().await;
                    },
                    async {
                        let _ = stop_rx.recv().await;
                    },
                )
                .await;
                continue;
            }

            // Rebuild agent on first turn or live config change (preserving persisted checkpoint!).
            if agent.is_none() || rebuild_needed.swap(false, Ordering::AcqRel) {
                match self
                    .build_bot_agent(
                        &bot_row,
                        &paths,
                        curator.clone(),
                        seen_memory.clone(),
                        sent_to_user_flag.clone(),
                        paused_for_inbox.clone(),
                        run_ctx_state.clone(),
                    )
                    .await
                {
                    Ok(built) => agent = Some(built),
                    Err(error) => {
                        tracing::error!(%bot_id, %error, "failed to build bot agent");
                        if let Ok(mut s) = run_state.lock() {
                            *s = BotRunState::Error;
                        }
                        self.record_trace(
                            &bot_id,
                            "error",
                            &format!("agent build failed: {error}"),
                        )
                        .await;
                        async_io::Timer::after(std::time::Duration::from_secs(2)).await;
                        continue;
                    }
                }
            }

            let active_agent = agent.as_mut().expect("agent just initialized");

            if let Ok(mut s) = run_state.lock() {
                *s = BotRunState::Running;
            }
            self.emit_state().await;

            sent_to_user_flag.store(false, Ordering::Release);
            let mut batch = self.db.drain_bot_inbox(&bot_id).await.unwrap_or_default();
            if batch.is_empty() {
                if let Ok(mut s) = run_state.lock() {
                    *s = BotRunState::Idle;
                }
                self.emit_state().await;
                continue;
            }
            let mut consumed_any_user = batch.iter().any(|e| e.kind == BotInboxKind::UserMessage);

            loop {
                if index_dirty.swap(false, Ordering::AcqRel) {
                    let mem_root = paths.memory_dir();
                    if let Ok(index_text) =
                        blocking::unblock(move || render_hierarchical_memory_index(&mem_root)).await
                    {
                        active_agent.context_mut().update_live(
                            &BOT_MEMORY_INDEX_CONTENT,
                            |update| {
                                update.external(
                                    &tag::BOT_MEMORY_INDEX,
                                    Some(ExternalContent::data(
                                        BOT_MEMORY_INDEX_SOURCE,
                                        index_text,
                                    )),
                                );
                            },
                        );
                    }
                }

                // Hybrid RAG query across inbound event text with context-ledger dedup.
                let rag_query = batch
                    .iter()
                    .map(|e| e.body.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                let recall_block = {
                    let mut ledger_snapshot = seen_memory.lock().map_or_default(|g| g.clone());
                    let recalled = self
                        .rag
                        .recall_for_event(
                            &bot_id,
                            &rag_query,
                            &paths.memory_dir(),
                            &mut ledger_snapshot,
                            MAX_RAG_HITS,
                        )
                        .await
                        .ok()
                        .flatten();
                    if let Ok(mut guard) = seen_memory.lock() {
                        *guard = ledger_snapshot;
                    }
                    recalled
                };

                if let Ok(mut ctx_s) = run_ctx_state.lock() {
                    ctx_s.consumed_user_message = consumed_any_user;
                    ctx_s.reminded_once = false;
                    ctx_s.pending_memory_recall = recall_block;
                }

                // Push enqueue-time UTC-stamped inbox batch directly as Message::user
                // and drive with agent.resume() so NO drain-time re-stamping occurs.
                let rendered = render_inbox_batch(&batch);
                active_agent.push_message(Message::user(rendered));
                paused_for_inbox.store(false, Ordering::Release);

                let mut text_buf = String::new();
                let mut run_outcome: Option<RunOutcome> = None;
                let mut tool_traces: Vec<String> = Vec::new();
                let mut tool_names: HashMap<String, String> = HashMap::new();

                let run = active_agent.resume();
                let driven = run
                    .drive((), std::future::pending::<()>(), |(), timed| {
                        match timed.event {
                            AgentEvent::BlockDelta {
                                kind: aither_agent::OutputKind::Text,
                                text,
                                ..
                            } => {
                                // Plain assistant text stays internal run trace ONLY (never user chat!).
                                text_buf.push_str(&text);
                            }
                            AgentEvent::ToolCallStarted { call, name, .. } => {
                                tool_names.insert(call.as_str().to_owned(), name.clone());
                                tool_traces.push(format!("tool `{name}` started"));
                            }
                            AgentEvent::ToolCallEnded { call, result, .. } => {
                                let name = tool_names
                                    .remove(call.as_str())
                                    .unwrap_or_else(|| call.as_str().to_owned());
                                let status = if result.is_error() { "error" } else { "ok" };
                                tool_traces.push(format!("tool `{name}` finished ({status})"));
                            }
                            AgentEvent::RunEnded { outcome, .. } => {
                                run_outcome = Some(outcome);
                            }
                            _ => {}
                        }
                        std::future::ready(ControlFlow::<(), _>::Continue(()))
                    })
                    .await;

                for line in tool_traces {
                    self.record_trace(&bot_id, "tool", &line).await;
                }
                if !text_buf.trim().is_empty() {
                    self.record_trace(&bot_id, "text", text_buf.trim()).await;
                }
                if let Err(error) = driven {
                    self.record_trace(&bot_id, "error", &error.to_string())
                        .await;
                } else if let Some(RunOutcome::Failed { error }) = run_outcome {
                    self.record_trace(&bot_id, "error", &error.to_string())
                        .await;
                }

                // Persist checkpoint + SeenMemoryLedger after every turn boundary.
                if let Ok(cp) = active_agent.export_checkpoint().await {
                    let ledger = seen_memory.lock().map_or_default(|g| g.clone());
                    let _ = self
                        .db
                        .upsert_bot_context_snapshot(&bot_id, &cp, &ledger)
                        .await;
                }

                if stop_rx.is_closed() || stop_rx.try_recv().is_ok() {
                    break;
                }

                // Drain any newly arrived inbox entries immediately while keeping
                // the warm agent and thinking blocks intact!
                let next_batch = self.db.drain_bot_inbox(&bot_id).await.unwrap_or_default();
                if next_batch.is_empty() {
                    break;
                }
                if next_batch
                    .iter()
                    .any(|e| e.kind == BotInboxKind::UserMessage)
                {
                    consumed_any_user = true;
                }
                batch = next_batch;
            }

            if let Ok(mut s) = run_state.lock() {
                *s = BotRunState::Idle;
            }
            self.emit_state().await;
        }

        if let Ok(mut s) = run_state.lock() {
            *s = BotRunState::Disabled;
        }
        let _ = self.loops.lock().map(|mut g| g.remove(&bot_id));
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_bot_agent(
        &self,
        bot: &BotRow,
        paths: &may_protocol::session_layout::BotPaths,
        curator: BotMemoryCuratorQueue,
        seen_memory: Arc<Mutex<SeenMemoryLedger>>,
        sent_to_user_flag: Arc<AtomicBool>,
        paused_for_inbox: Arc<AtomicBool>,
        run_ctx_state: Arc<Mutex<BotRunContextState>>,
    ) -> anyhow::Result<Agent<LLM, BotHooks, LLM>> {
        let (llm, params) = self.model_factory.build_model(bot).await?;
        let mem_root = paths.memory_dir();
        let initial_index = {
            let root = mem_root.clone();
            blocking::unblock(move || render_hierarchical_memory_index(&root)).await?
        };

        let tool_ctx = BotToolContext {
            bot_id: bot.id.clone(),
            bot_agent_id: bot.agent_id.clone(),
            bot_name: bot.name.clone(),
            memory_root: mem_root,
            db: self.db.clone(),
            messaging: self.messaging.load_full().map(|a| (*a).clone()),
            delegates: self.delegates.clone(),
            approvals: self.approvals.clone(),
            curator,
            sessions: self.sessions.clone(),
            notifications: self.notifications.clone(),
            sleep: self.sleep.clone(),
            seen_memory: seen_memory.clone(),
            sent_to_user_flag: sent_to_user_flag.clone(),
            wake_bot: self.wake_bot_fn.clone(),
            publish_event: self.publish_event.clone(),
            ui_event_bus: self.ui_event_bus.clone(),
        };

        let hooks = BotHooks {
            bot_id: bot.id.clone(),
            db: self.db.clone(),
            paused_for_inbox,
        };
        let context_source = BotContextSource {
            seen_memory: seen_memory.clone(),
            sent_to_user_flag,
            run_state: run_ctx_state,
        };

        // Non-bash-first agent: NEVER register `terminal` or `monitor`.
        let mut agent = Agent::builder(llm)
            .system_layout(tag::bot_layout())
            .request_parameters(params)
            .hooks(hooks)
            .context_source(context_source)
            .tool(SendMessageTool::new(tool_ctx.clone()))
            .tool(StoreFactTool::new(tool_ctx.clone()))
            .tool(RecallTool::new(tool_ctx.clone()))
            .tool(DelegateAgentTool::new(tool_ctx.clone()))
            .tool(StopAgentTool::new(tool_ctx.clone()))
            .tool(ResumeAgentTool::new(tool_ctx.clone()))
            .tool(ReadAgentTool::new(tool_ctx.clone()))
            .tool(CreateSessionTool::new(tool_ctx.clone()))
            .tool(StopSessionTool::new(tool_ctx.clone()))
            .tool(DecideApprovalTool::new(tool_ctx.clone()))
            .tool(ScheduleTool::new(tool_ctx))
            .build();

        let tz = jiff::tz::TimeZone::system()
            .iana_name()
            .unwrap_or("UTC")
            .to_string();
        {
            let instructions = agent.context_mut().instructions_mut();
            instructions.insert(
                &tag::BOT_IDENTITY,
                BotIdentityTemplate {
                    name: &bot.name,
                    agent_id: &bot.agent_id,
                }
                .render()
                .expect("bot identity template must render"),
            );
            if let Some(persona) = bot
                .persona
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
            {
                instructions.insert(
                    &tag::BOT_PERSONA,
                    BotPersonaTemplate { persona }
                        .render()
                        .expect("bot persona template must render"),
                );
            }
            instructions.insert(
                &tag::BOT_TOOLS,
                BotToolsTemplate
                    .render()
                    .expect("bot tools template must render"),
            );
            instructions.insert(
                &tag::BOT_TIME,
                BotTimeTemplate { timezone: &tz }
                    .render()
                    .expect("bot time template must render"),
            );
        }

        if let Ok(Some(stored)) = self.db.get_bot_context_snapshot(&bot.id).await {
            if let Err(error) = agent.restore_checkpoint(stored.checkpoint) {
                tracing::warn!(bot_id = %bot.id, %error, "failed to restore bot checkpoint; starting fresh");
            } else if let Ok(mut guard) = seen_memory.lock() {
                *guard = stored.seen_memory;
            }
        }

        agent
            .context_mut()
            .update_live(&BOT_MEMORY_INDEX_CONTENT, |update| {
                update.external(
                    &tag::BOT_MEMORY_INDEX,
                    Some(ExternalContent::data(
                        BOT_MEMORY_INDEX_SOURCE,
                        initial_index,
                    )),
                );
            });

        Ok(agent)
    }

    async fn record_trace(&self, bot_id: &str, kind: &str, content: &str) {
        if let Ok(entry) = self.db.append_bot_trace_entry(bot_id, kind, content).await {
            (self.publish_event)(BotSseEvent::BotTrace { entry });
        }
    }
}

/// Helper verifying that a built bot agent exposes ONLY its narrow native tools
/// and NEVER `terminal` or `monitor`.
#[must_use]
pub const fn bot_native_tool_names() -> &'static [&'static str] {
    &[
        "send_message",
        "store_fact",
        "recall",
        "delegate_agent",
        "stop_agent",
        "resume_agent",
        "read_agent",
        "create_session",
        "stop_session",
        "decide_approval",
        "schedule",
    ]
}

/// No-op [`SessionBridge`] used in standalone unit tests.
#[derive(Debug, Default, Clone)]
pub struct NoopSessionBridge;

impl SessionBridge for NoopSessionBridge {
    fn create_session(
        &self,
        _bot_id: String,
        _prompt: String,
        _title: Option<String>,
        _profile_id: Option<String>,
        _model: Option<String>,
        _cwd: Option<String>,
        _repo: Option<String>,
    ) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send + '_>> {
        Box::pin(async { Err("session bridge is unavailable in this runtime".to_string()) })
    }

    fn stop_session(
        &self,
        _session_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Err("session bridge is unavailable in this runtime".to_string()) })
    }
}

/// No-op [`DelegateRunner`] used in unit tests.
#[derive(Debug, Default, Clone)]
pub struct NoopDelegateRunner;

impl DelegateRunner for NoopDelegateRunner {
    fn run_worker(
        &self,
        _bot_id: String,
        _agent_id: String,
        _name: String,
        _definition: String,
        _work_dir: PathBuf,
        _grants: aither_sandbox::Grants,
        _prompt: Option<String>,
        checkpoint: Option<AgentCheckpoint>,
        _approvals: BotApprovalRouter,
        _pause: may_harness::harness::approval_pause::ApprovalPause,
        _steer_rx: async_channel::Receiver<String>,
        _cancel_out: Arc<Mutex<Option<aither_agent::CancelHandle>>>,
        _active_command: Arc<Mutex<Option<String>>>,
    ) -> Pin<Box<dyn Future<Output = super::delegates::DelegateRunResult> + Send + 'static>> {
        Box::pin(async move {
            super::delegates::DelegateRunResult {
                status: may_protocol::bot::BotDelegateStatus::Completed,
                summary: "noop delegate completed".to_string(),
                prompt_tokens: 0,
                completion_tokens: 0,
                checkpoint,
            }
        })
    }
}
