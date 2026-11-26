//! Permission triage and escalation router for May Bots.
//!
//! Delegated workers and bot-supervised sessions route every permission gate
//! (`unsafe`, `ssh`, `publish`, `workspace`) through [`BotApprovalRouter`]:
//! - Under [`GatePolicy::AutoApprove`], requests approve at once.
//! - Under [`GatePolicy::BotTriage`], the requester is held under
//!   [`ApprovalPause`] WITHOUT emitting `UserWait` or push notifications while
//!   the owning bot evaluates it via `decide_approval`.
//! - Only when the bot calls `Escalate { note }` (or under
//!   [`GatePolicy::AlwaysEscalate`]) is the request promoted to a user-visible
//!   `UserWait` approval card + push notification carrying `note`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aither_core::llm::{Approval, ApprovalRequest, Approver, ApproverFailure, ToolCallId};
use aither_sandbox::{DomainApprover, DomainRequest};
use may_db::{ChatDb, NewBotApproval, NewBotInboxEntry};
use may_harness::harness::approval_pause::ApprovalPause;
use may_protocol::bot::{
    BotApprovalDecision, BotApprovalGate, BotApprovalItem, BotApprovalStatus, BotInboxKind,
    BotSseEvent, GatePolicy,
};
use may_protocol::layout_consts::SSH_KIND;

use crate::agent::permission::UNSANDBOXED_KIND;
use crate::notification::{NotificationPayload, NotificationService};
use crate::session::{SessionRuntimeService, UserAsk, UserWait};
use crate::sleep::SleepState;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// Outcome returned to a parked requester once its approval resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalOutcome {
    /// Approved by policy, bot, or escalated human user.
    Approved,
    /// Denied with a human-readable explanation.
    Denied(String),
}

impl ApprovalOutcome {
    /// Converts to an [`aither_core::llm::Approval`].
    #[must_use]
    pub fn into_aither(self) -> Approval {
        match self {
            Self::Approved => Approval::Approved,
            Self::Denied(reason) => Approval::Denied { reason },
        }
    }

    /// Whether approved.
    #[must_use]
    pub const fn is_approved(&self) -> bool {
        matches!(self, Self::Approved)
    }
}

/// Errors returned when deciding or resolving a bot approval.
#[derive(Debug, thiserror::Error)]
pub enum BotApprovalError {
    /// No pending approval matches the id for that bot.
    #[error("no pending approval request `{0}` found")]
    NotFound(String),
    /// Approval belongs to another bot.
    #[error("approval request `{0}` does not belong to this bot")]
    Unauthorized(String),
    /// Approval was already resolved.
    #[error("approval request `{0}` is already resolved")]
    AlreadyResolved(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
}

struct ParkedApproval {
    bot_id: String,
    gate: BotApprovalGate,
    requester_kind: String,
    requester_id: String,
    subject: String,
    tx: async_channel::Sender<ApprovalOutcome>,
}

/// Shared callback that wakes a bot's resident inbox loop.
pub type BotWakeFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Shared callback publishing a live [`BotSseEvent`] onto `/api/events`.
pub type BotEventPublishFn = Arc<dyn Fn(BotSseEvent) + Send + Sync>;

/// Coordinates permission triage across all bots, delegated workers, and
/// bot-supervised sessions.
#[derive(Clone)]
pub struct BotApprovalRouter {
    db: ChatDb,
    runtime_service: Option<Arc<SessionRuntimeService>>,
    notification_service: Option<Arc<NotificationService>>,
    sleep: Option<Arc<SleepState>>,
    ui_event_bus: UiEventBus,
    parked: Arc<Mutex<HashMap<String, ParkedApproval>>>,
    wake_bot: BotWakeFn,
    publish_event: BotEventPublishFn,
}

impl std::fmt::Debug for BotApprovalRouter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotApprovalRouter").finish_non_exhaustive()
    }
}

impl BotApprovalRouter {
    /// Creates a new router wired to `db`, `ui_event_bus`, and bot wake/event callbacks.
    #[must_use]
    pub fn new(
        db: ChatDb,
        ui_event_bus: UiEventBus,
        wake_bot: BotWakeFn,
        publish_event: BotEventPublishFn,
    ) -> Self {
        Self {
            db,
            runtime_service: None,
            notification_service: None,
            sleep: None,
            ui_event_bus,
            parked: Arc::new(Mutex::new(HashMap::new())),
            wake_bot,
            publish_event,
        }
    }

    /// Attaches the session runtime service so escalated requests announce `UserWait`.
    #[must_use]
    pub fn with_runtime_service(mut self, runtime: Arc<SessionRuntimeService>) -> Self {
        self.runtime_service = Some(runtime);
        self
    }

    /// Attaches `NotificationService` so escalated requests push native notifications carrying `note`.
    #[must_use]
    pub fn with_notifications(mut self, notifications: Arc<NotificationService>) -> Self {
        self.notification_service = Some(notifications);
        self
    }

    /// Attaches `SleepState` so push notifications honor sleep mode.
    #[must_use]
    pub fn with_sleep(mut self, sleep: Arc<SleepState>) -> Self {
        self.sleep = Some(sleep);
        self
    }

    /// Submits a permission gate request from a delegated worker or supervised session
    /// and waits under `pause` until resolved by policy, the bot, or an escalated user.
    ///
    /// # Errors
    /// Returns [`BotApprovalError`] on storage failure.
    ///
    /// # Panics
    /// Panics if the internal parked-approval mutex is poisoned.
    #[allow(clippy::too_many_arguments)]
    pub async fn request(
        &self,
        bot_id: &str,
        requester_kind: &str,
        requester_id: &str,
        gate: BotApprovalGate,
        subject: &str,
        reason: Option<&str>,
        pause: &ApprovalPause,
        call: Option<ToolCallId>,
    ) -> Result<ApprovalOutcome, BotApprovalError> {
        let _pause_hold = pause.hold(call);

        let policy = self
            .db
            .get_bot(bot_id)
            .await?
            .map_or(GatePolicy::BotTriage, |row| {
                row.approval_policy.for_gate(gate)
            });

        let request_id = crate::id::new();

        if policy == GatePolicy::AutoApprove {
            let item = self
                .db
                .create_bot_approval(NewBotApproval {
                    request_id: &request_id,
                    bot_id,
                    requester_kind,
                    requester_id,
                    gate,
                    subject,
                    reason,
                    status: BotApprovalStatus::Approved,
                    decision_note: Some("auto-approved by bot policy"),
                })
                .await?;
            self.emit_approval(item);
            return Ok(ApprovalOutcome::Approved);
        }

        let (tx, rx) = async_channel::bounded(1);
        {
            self.parked
                .lock()
                .expect("bot approval parked lock")
                .insert(
                    request_id.clone(),
                    ParkedApproval {
                        bot_id: bot_id.to_string(),
                        gate,
                        requester_kind: requester_kind.to_string(),
                        requester_id: requester_id.to_string(),
                        subject: subject.to_string(),
                        tx,
                    },
                );
        }

        let initial_status = if policy == GatePolicy::AlwaysEscalate {
            BotApprovalStatus::Escalated
        } else {
            BotApprovalStatus::PendingBot
        };
        let initial_note = (policy == GatePolicy::AlwaysEscalate)
            .then_some("escalated directly by bot approval policy");

        let item = self
            .db
            .create_bot_approval(NewBotApproval {
                request_id: &request_id,
                bot_id,
                requester_kind,
                requester_id,
                gate,
                subject,
                reason,
                status: initial_status,
                decision_note: initial_note,
            })
            .await?;
        self.emit_approval(item.clone());

        if policy == GatePolicy::AlwaysEscalate {
            self.promote_to_user_wait(&item, initial_note.unwrap_or_default())
                .await;
        } else {
            // BotTriage: enqueue to bot_inbox WITHOUT emitting UserWait or push notifications!
            let body = format!(
                "Approval request `{request_id}` ({gate}) from {requester_kind} `{requester_id}`:\nSubject: {subject}{}",
                reason.map_or_else(String::new, |r| format!("\nReason: {r}"))
            );
            let meta = serde_json::json!({
                "request_id": request_id,
                "gate": gate.as_str(),
                "requester_kind": requester_kind,
                "requester_id": requester_id,
            })
            .to_string();
            let _ = self
                .db
                .enqueue_bot_inbox(NewBotInboxEntry {
                    bot_id,
                    kind: BotInboxKind::ApprovalRequest,
                    sender_id: Some(requester_id),
                    sender_name: Some(requester_kind),
                    body: &body,
                    meta_json: Some(&meta),
                })
                .await?;
            (self.wake_bot)(bot_id);
        }

        let outcome = rx
            .recv()
            .await
            .unwrap_or_else(|_| ApprovalOutcome::Denied("approval request cancelled".to_string()));
        Ok(outcome)
    }

    /// Resolves a parked approval request via the bot's `decide_approval` tool.
    ///
    /// # Errors
    /// Returns [`BotApprovalError`] if unknown, unauthorized, or already resolved.
    pub async fn decide_by_bot(
        &self,
        bot_id: &str,
        request_id: &str,
        decision: BotApprovalDecision,
    ) -> Result<BotApprovalItem, BotApprovalError> {
        let existing = self
            .db
            .get_bot_approval(request_id)
            .await?
            .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
        if existing.bot_id != bot_id {
            return Err(BotApprovalError::Unauthorized(request_id.to_string()));
        }
        if matches!(
            existing.status,
            BotApprovalStatus::Approved | BotApprovalStatus::Denied
        ) {
            return Err(BotApprovalError::AlreadyResolved(request_id.to_string()));
        }

        match decision {
            BotApprovalDecision::Approve => {
                let updated = self
                    .db
                    .update_bot_approval_status(
                        request_id,
                        BotApprovalStatus::Approved,
                        Some("approved by bot"),
                    )
                    .await?
                    .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
                self.wake_parked(request_id, ApprovalOutcome::Approved);
                self.emit_approval(updated.clone());
                Ok(updated)
            }
            BotApprovalDecision::Deny { reason } => {
                let updated = self
                    .db
                    .update_bot_approval_status(
                        request_id,
                        BotApprovalStatus::Denied,
                        Some(&reason),
                    )
                    .await?
                    .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
                self.wake_parked(request_id, ApprovalOutcome::Denied(reason));
                self.emit_approval(updated.clone());
                Ok(updated)
            }
            BotApprovalDecision::Escalate { note } => {
                let updated = self
                    .db
                    .update_bot_approval_status(
                        request_id,
                        BotApprovalStatus::Escalated,
                        Some(&note),
                    )
                    .await?
                    .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
                self.emit_approval(updated.clone());
                self.promote_to_user_wait(&updated, &note).await;
                Ok(updated)
            }
        }
    }

    /// Resolves an escalated (or pending) approval from a human user action (REST / TUI / Web UI).
    ///
    /// # Errors
    /// Returns [`BotApprovalError`] if unknown or already resolved.
    pub async fn resolve_by_user(
        &self,
        request_id: &str,
        approved: bool,
        reason: Option<String>,
    ) -> Result<BotApprovalItem, BotApprovalError> {
        let existing = self
            .db
            .get_bot_approval(request_id)
            .await?
            .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
        if matches!(
            existing.status,
            BotApprovalStatus::Approved | BotApprovalStatus::Denied
        ) {
            return Err(BotApprovalError::AlreadyResolved(request_id.to_string()));
        }
        let status = if approved {
            BotApprovalStatus::Approved
        } else {
            BotApprovalStatus::Denied
        };
        let note = if approved {
            Some("approved by user".to_string())
        } else {
            Some(reason.unwrap_or_else(|| "denied by user".to_string()))
        };
        let updated = self
            .db
            .update_bot_approval_status(request_id, status, note.as_deref())
            .await?
            .ok_or_else(|| BotApprovalError::NotFound(request_id.to_string()))?;
        let outcome = if approved {
            ApprovalOutcome::Approved
        } else {
            ApprovalOutcome::Denied(note.unwrap_or_else(|| "denied by user".to_string()))
        };
        self.wake_parked(request_id, outcome);
        self.emit_approval(updated.clone());
        Ok(updated)
    }

    /// Returns whether `request_id` is currently parked waiting for a decision.
    ///
    /// # Panics
    /// Panics if the internal parked-approval mutex is poisoned.
    #[must_use]
    pub fn is_parked(&self, request_id: &str) -> bool {
        self.parked
            .lock()
            .expect("bot approval parked lock")
            .contains_key(request_id)
    }

    /// Builds an [`Approver`] for a top-level delegated worker owned by `bot_id`,
    /// routing `unsafe` and `ssh` gates to `decide_approval` rather than hard-refusing.
    #[must_use]
    pub fn approver_for_delegate(
        &self,
        bot_id: impl Into<String>,
        agent_id: impl Into<String>,
        pause: ApprovalPause,
    ) -> DelegatedWorkerApprover {
        DelegatedWorkerApprover {
            router: self.clone(),
            bot_id: bot_id.into(),
            agent_id: agent_id.into(),
            pause,
        }
    }

    fn wake_parked(&self, request_id: &str, outcome: ApprovalOutcome) {
        let entry = self
            .parked
            .lock()
            .expect("bot approval parked lock")
            .remove(request_id);
        if let Some(parked) = entry {
            let _ = parked.tx.try_send(outcome);
            let _ = (
                &parked.bot_id,
                parked.gate,
                &parked.requester_kind,
                &parked.requester_id,
                &parked.subject,
            );
        }
    }

    fn emit_approval(&self, approval: BotApprovalItem) {
        (self.publish_event)(BotSseEvent::BotApproval { approval });
        self.ui_event_bus.publish(UiEventTopics::BOTS);
    }

    async fn promote_to_user_wait(&self, item: &BotApprovalItem, note: &str) {
        let summary = if note.trim().is_empty() {
            format!("[{}] {}", item.gate, item.subject)
        } else {
            format!(
                "[{}] {} — Bot note: {}",
                item.gate,
                item.subject,
                note.trim()
            )
        };

        if let Some(runtime) = &self.runtime_service {
            let ask = match item.gate {
                BotApprovalGate::Workspace => UserAsk::Workspace {
                    path: summary.clone(),
                },
                BotApprovalGate::Publish => UserAsk::permission(
                    may_protocol::permission::PermissionRequestMode::Publish,
                    &summary,
                ),
                BotApprovalGate::Unsafe => UserAsk::permission(
                    may_protocol::permission::PermissionRequestMode::Unsafe,
                    &summary,
                ),
                BotApprovalGate::Ssh => UserAsk::permission(
                    may_protocol::permission::PermissionRequestMode::Ssh,
                    &summary,
                ),
            };
            runtime.announce_user_wait(UserWait {
                session_id: item.bot_id.clone(),
                ask,
            });
        }

        if let Some(notifications) = &self.notification_service {
            let asleep = self.sleep.as_ref().is_some_and(|sleep| sleep.is_asleep());
            if !asleep {
                let _ = notifications
                    .send_payload(NotificationPayload {
                        notification_id: None,
                        title: format!("Bot approval escalated ({})", item.gate),
                        body: summary,
                        session_id: Some(item.bot_id.clone()),
                        profile_id: None,
                    })
                    .await;
            }
        }
    }
}

/// [`Approver`] for a top-level delegated worker: routes `unsandboxed` and `ssh`
/// gates through [`BotApprovalRouter`] instead of hard-refusing SSH like ordinary
/// ephemeral subagents.
#[derive(Debug, Clone)]
pub struct DelegatedWorkerApprover {
    router: BotApprovalRouter,
    bot_id: String,
    agent_id: String,
    pause: ApprovalPause,
}

impl Approver for DelegatedWorkerApprover {
    async fn approve(&self, request: ApprovalRequest) -> Result<Approval, ApproverFailure> {
        let gate = match request.kind().as_str() {
            UNSANDBOXED_KIND => BotApprovalGate::Unsafe,
            SSH_KIND => BotApprovalGate::Ssh,
            other => {
                return Ok(Approval::Denied {
                    reason: format!("nothing approves a `{other}` request here"),
                });
            }
        };
        let call = request.call().map(|identity| identity.call().clone());
        let subject = if gate == BotApprovalGate::Ssh {
            request
                .reason()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| request.subject())
        } else {
            request.subject()
        };
        match self
            .router
            .request(
                &self.bot_id,
                "delegate",
                &self.agent_id,
                gate,
                subject,
                request.reason(),
                &self.pause,
                call,
            )
            .await
        {
            Ok(outcome) => Ok(outcome.into_aither()),
            Err(error) => Err(ApproverFailure::new(error)),
        }
    }
}

impl DomainApprover for DelegatedWorkerApprover {
    fn approve(&self, _request: &DomainRequest) -> impl std::future::Future<Output = bool> + Send {
        std::future::ready(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use may_db::NewBotRow;
    use may_protocol::bot::BotApprovalPolicy;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[tokio::test]
    async fn bot_triage_holds_without_user_wait_and_escalates_on_decide_approval() {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let bus = UiEventBus::new();
        let runtime = Arc::new(SessionRuntimeService::new(
            db.clone(),
            bus.clone(),
            may_protocol::session_layout::MayHomePaths::new(dir.path().to_path_buf()),
        ));
        let mut user_waits = runtime.subscribe_user_waits();

        let policy = BotApprovalPolicy::default();
        db.create_bot(NewBotRow {
            id: "bot-a",
            agent_id: "bold-cedar-hawk",
            name: "Triage Bot",
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: &policy,
        })
        .await
        .expect("bot");

        let wakes = Arc::new(AtomicUsize::new(0));
        let wakes_clone = wakes.clone();
        let router = BotApprovalRouter::new(
            db.clone(),
            bus,
            Arc::new(move |_| {
                wakes_clone.fetch_add(1, Ordering::SeqCst);
            }),
            Arc::new(|_| {}),
        )
        .with_runtime_service(runtime);

        let pause = ApprovalPause::new();
        let req_router = router.clone();
        let req_pause = pause.clone();
        let waiter = tokio::spawn(async move {
            req_router
                .request(
                    "bot-a",
                    "delegate",
                    "swift-pine-wolf",
                    BotApprovalGate::Ssh,
                    "prod-box.internal",
                    Some("deploy release"),
                    &req_pause,
                    None,
                )
                .await
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(
            wakes.load(Ordering::SeqCst),
            1,
            "bot must be woken for triage"
        );
        assert!(
            user_waits.try_recv().is_err(),
            "UserWait must NOT fire while bot is triaging"
        );

        let pending = db.list_bot_approvals("bot-a").await.expect("list");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].status, BotApprovalStatus::PendingBot);
        let req_id = pending[0].request_id.clone();

        // Bot escalates with note -> emits UserWait carrying note!
        let escalated = router
            .decide_by_bot(
                "bot-a",
                &req_id,
                BotApprovalDecision::Escalate {
                    note: "Production SSH deploy needs human sign-off".to_string(),
                },
            )
            .await
            .expect("escalate");
        assert_eq!(escalated.status, BotApprovalStatus::Escalated);
        let wait_event = user_waits
            .try_recv()
            .expect("UserWait MUST fire after Escalate");
        assert_eq!(wait_event.session_id, "bot-a");

        // Human approves -> parked waiter resumes with Approved.
        router
            .resolve_by_user(&req_id, true, None)
            .await
            .expect("human approve");
        let outcome = waiter.await.expect("join").expect("outcome");
        assert_eq!(outcome, ApprovalOutcome::Approved);
    }
}
