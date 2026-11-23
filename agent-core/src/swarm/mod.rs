//! Experimental per-worktree agent swarm coordination.
//!
//! Manages shared boards, pull-by-default message history, `@mention` wakeups
//! with loop breaking, and PR/merge-queue notifications.

mod conditions;
mod tool;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use askama::Template;

use may_protocol::session_runtime::PendingMessageState;
use may_protocol::xml::XmlBlock;

use crate::agent_link::{
    AgentLinkError, AgentMessaging, AgentStatus, AttributeText, UntrustedText,
};
use crate::config::MayConfig;
use crate::db::{
    ChatDb, ChatDbError, DissolvedSteerAction, NewSwarmMessage, PrRow, SwarmMessageKind,
    SwarmReadMode, pr_sigil,
};
use crate::session::{PendingUserMessage, PendingUserMessageMode, SessionDirectory};
use crate::ui_event_bus::UiEventTopics;
use crate::workspace::WorkspaceConfig;

pub use conditions::{SwarmConditions, SwarmSeenState};
pub use tool::{BoardArgs, BoardCommandSet, BoardTool};

/// Convenience constructor for [`BoardTool`].
#[must_use]
pub const fn tool_for_session(
    swarm: SwarmService,
    session_id: String,
    sandbox_dir: PathBuf,
    workspace_mount: Option<aither_sandbox::MountRoot>,
) -> BoardTool {
    BoardTool::new(swarm, session_id, sandbox_dir, workspace_mount)
}

/// Max changed file paths listed inline in a PR board entry before `(+N more)`.
const MAX_INLINE_PR_FILES: usize = 10;

/// PR transition payload announced to an active swarm board.
#[derive(Debug)]
pub struct SwarmPrAnnouncement<'a> {
    /// Session that owns or updated the PR.
    pub actor_session_id: &'a str,
    /// Repository key of the PR's project.
    pub repo_key: &'a str,
    /// Session workspace configuration used to resolve repository identity.
    pub workspace_config: &'a WorkspaceConfig,
    /// Updated PR row.
    pub pr: &'a PrRow,
    /// Event kind (`opened`, `queued`, `merged`, `closed`, `check_failed`, etc.).
    pub kind: &'a str,
    /// Optional event detail string.
    pub detail: Option<&'a str>,
    /// Changed file paths between `base_sha` and `head_sha`.
    pub changed_files: Vec<String>,
}
/// Active board snapshot for one normalized worktree root.
#[derive(Debug, Clone)]
pub struct ActiveBoard {
    /// Normalized worktree root path.
    pub workdir: String,
    /// Monotonic generation of this board.
    pub board_epoch: u64,
    /// Working sessions currently attached to this board.
    pub members: Vec<AgentStatus>,
}

#[derive(Debug, Clone, Default)]
struct ActiveSwarmSnapshot {
    boards: BTreeMap<String, ActiveBoard>,
    session_to_workdir: BTreeMap<String, String>,
}

impl ActiveSwarmSnapshot {
    fn board_for_session(&self, session_id: &str) -> Option<&ActiveBoard> {
        let workdir = self.session_to_workdir.get(session_id)?;
        self.boards.get(workdir)
    }
}

/// Errors produced by swarm coordination and `board` operations.
#[derive(Debug, thiserror::Error)]
pub enum SwarmError {
    /// Database read or write failed.
    #[error(transparent)]
    Db(#[from] ChatDbError),
    /// Agent directory lookup or delivery failed.
    #[error(transparent)]
    AgentLink(#[from] AgentLinkError),
    /// `board` was called when the session's checkout has no active swarm.
    #[error(
        "no active swarm board in this checkout (`[experimental] swarm` requires > 1 working session in the same worktree)"
    )]
    NoActiveBoard,
    /// Message file was empty.
    #[error("refusing to post an empty board message from {0}")]
    EmptyMessage(PathBuf),
    /// `@mention` matched multiple members with the same title.
    #[error("{0}")]
    AmbiguousMention(String),
    /// Template rendering failed.
    #[error(transparent)]
    Template(#[from] askama::Error),
}

/// Shared daemon-side service reconciling swarm boards and routing board operations.
#[derive(Clone)]
pub struct SwarmService {
    messaging: AgentMessaging,
    may_dir: PathBuf,
    config: Arc<ArcSwap<MayConfig>>,
    snapshot: Arc<ArcSwap<ActiveSwarmSnapshot>>,
    reconcile_lock: Arc<async_lock::Mutex<()>>,
}

impl std::fmt::Debug for SwarmService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SwarmService")
            .field("may_dir", &self.may_dir)
            .finish_non_exhaustive()
    }
}

#[derive(Template)]
#[template(path = "swarm_read.txt", escape = "none")]
struct SwarmReadTemplate<'a> {
    board_epoch: u64,
    max_seq: u64,
    unread_only: bool,
    messages: Vec<RenderedMessage<'a>>,
    next_seq: Option<u64>,
}

struct RenderedMessage<'a> {
    board_epoch: u64,
    seq: u64,
    kind: &'static str,
    sender: &'a str,
    session_id: Option<&'a str>,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "swarm_posted.txt", escape = "none")]
struct SwarmPostedTemplate {
    board_epoch: u64,
    seq: u64,
    woken: Vec<String>,
    queued_capped: Vec<String>,
}

#[derive(Template)]
#[template(path = "swarm_members.txt", escape = "none")]
struct SwarmMembersTemplate<'a> {
    workdir: &'a str,
    board_epoch: u64,
    members: Vec<RenderedMember<'a>>,
}

struct RenderedMember<'a> {
    name: &'a str,
    session_id: &'a str,
    activity: crate::agent_link::Activity,
    roots: &'a [String],
    is_self: bool,
}

#[derive(Template)]
#[template(path = "swarm_mention.txt", escape = "none")]
struct SwarmMentionTemplate<'a> {
    sender: AttributeText<'a>,
    sender_id: AttributeText<'a>,
    board_epoch: u64,
    seq: u64,
    workdir: AttributeText<'a>,
    dissolved: bool,
    wake_capped: bool,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "swarm_notice.txt", escape = "none")]
struct SwarmNoticeTemplate<'a> {
    board_epoch: u64,
    seq: u64,
    workdir: AttributeText<'a>,
    kind: &'static str,
    body: UntrustedText<'a>,
}

#[derive(Template)]
#[template(path = "swarm_ambiguous_mention.txt", escape = "none")]
struct SwarmAmbiguousMentionTemplate<'a> {
    mention: &'a str,
    candidates: Vec<String>,
}

#[derive(Template)]
#[template(path = "swarm_lifecycle_opened.txt", escape = "none")]
struct SwarmLifecycleOpenedTemplate<'a> {
    board_epoch: u64,
    workdir: &'a str,
    member_count: usize,
    names: Vec<&'a str>,
}

enum PrSwarmAction<'a> {
    Opened,
    Queued,
    Merged { sha: &'a str },
    Cancelled { reason: String },
    Timeline { event: String },
}

#[derive(Template)]
#[template(path = "swarm_pr_entry.txt", escape = "none")]
struct SwarmPrEntryTemplate<'a> {
    actor: &'a str,
    actor_id: &'a str,
    sigil: char,
    number: i64,
    title: &'a str,
    head_sha: &'a str,
    action: PrSwarmAction<'a>,
    files: &'a [String],
    extra_files: usize,
}

impl SwarmService {
    /// Builds the swarm coordination service backed by `messaging` and live `config`.
    #[must_use]
    pub fn new(
        messaging: AgentMessaging,
        may_dir: PathBuf,
        config: Arc<ArcSwap<MayConfig>>,
    ) -> Self {
        Self {
            messaging,
            may_dir,
            config,
            snapshot: Arc::new(ArcSwap::from_pointee(ActiveSwarmSnapshot::default())),
            reconcile_lock: Arc::new(async_lock::Mutex::new(())),
        }
    }

    /// Database handle shared with the session runtime.
    #[must_use]
    pub fn db(&self) -> ChatDb {
        self.messaging.runtime_service.db()
    }

    /// Whether `[experimental] swarm` is enabled in the live configuration.
    #[must_use]
    pub fn is_enabled(&self) -> bool {
        self.config.load().experimental.swarm
    }

    /// Active board for `session_id` from the latest reconciled in-memory snapshot.
    #[must_use]
    pub fn active_board_for(&self, session_id: &str) -> Option<ActiveBoard> {
        if !self.is_enabled() {
            return None;
        }
        self.snapshot.load().board_for_session(session_id).cloned()
    }

    /// Normalizes a session checkout path to its git worktree root when inside
    /// a git repository, or its canonical path otherwise.
    #[must_use]
    pub fn normalize_checkout(path: &Path) -> String {
        let root = crate::repo_search::worktree_root(path)
            .unwrap_or_else(|| dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()));
        dunce::canonicalize(&root)
            .unwrap_or(root)
            .to_string_lossy()
            .into_owned()
    }

    /// Reconciles active swarm boards against live working sessions and the
    /// `[experimental] swarm` switch. Safe to call concurrently and idempotently.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when reading sessions or updating board rows fails.
    pub async fn reconcile(&self) -> Result<(), SwarmError> {
        let _guard = self.reconcile_lock.lock().await;
        let db = self.db();

        if !self.is_enabled() {
            let existing = db.list_swarm_boards().await?;
            let mut touched_any = false;
            for board in existing {
                if let Some((_, touched)) = self.destroy_board_and_steers(&board.workdir).await?
                    && !touched.is_empty()
                {
                    touched_any = true;
                }
            }
            self.snapshot
                .store(Arc::new(ActiveSwarmSnapshot::default()));
            if touched_any {
                self.messaging
                    .runtime_service
                    .ui_events()
                    .publish(UiEventTopics::SESSION_RUNTIMES);
            }
            return Ok(());
        }

        let ((live_agents, working_ids), (session_rows, existing_boards)) =
            futures_lite::future::zip(
                futures_lite::future::zip(
                    self.messaging.directory.live(),
                    db.working_session_ids(),
                ),
                futures_lite::future::zip(db.list_sessions(), db.list_swarm_boards()),
            )
            .await;
        let live_agents = live_agents?;
        let working_ids = working_ids?;
        let session_rows = session_rows?;
        let existing_boards = existing_boards?;

        let rows_by_id: BTreeMap<&str, &crate::db::entity::sessions::Model> = session_rows
            .iter()
            .map(|row| (row.id.as_str(), row))
            .collect();

        let mut grouped: BTreeMap<String, Vec<AgentStatus>> = BTreeMap::new();
        for agent in live_agents {
            if !working_ids.contains(&agent.session_id) {
                continue;
            }
            let Some(row) = rows_by_id.get(agent.session_id.as_str()) else {
                continue;
            };
            let SessionDirectory::Checkout(checkout) = SessionDirectory::of(row, &self.may_dir)
            else {
                continue;
            };
            let key = Self::normalize_checkout(&checkout);
            grouped.entry(key).or_default().push(agent);
        }

        let mut touched_any = false;
        for board in &existing_boards {
            let active_count = grouped.get(&board.workdir).map_or(0, Vec::len);
            if active_count <= 1
                && let Some((_, touched)) = self.destroy_board_and_steers(&board.workdir).await?
                && !touched.is_empty()
            {
                touched_any = true;
            }
        }

        let mut next_snapshot = ActiveSwarmSnapshot::default();
        for (workdir, members) in grouped {
            if members.len() <= 1 {
                continue;
            }
            let (board_row, created) = db.open_or_get_swarm_board(&workdir).await?;
            if created {
                let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
                let display_dir = crate::fs_utils::display_path(Path::new(&workdir));
                let summary = SwarmLifecycleOpenedTemplate {
                    board_epoch: board_row.board_epoch,
                    workdir: &display_dir,
                    member_count: members.len(),
                    names,
                }
                .render()?
                .trim_end()
                .to_string();
                let _ = db
                    .append_swarm_message(NewSwarmMessage {
                        workdir: &workdir,
                        session_id: None,
                        sender: "swarm",
                        kind: SwarmMessageKind::Lifecycle,
                        body: &summary,
                    })
                    .await?;
            }
            for member in &members {
                next_snapshot
                    .session_to_workdir
                    .insert(member.session_id.clone(), workdir.clone());
            }
            next_snapshot.boards.insert(
                workdir.clone(),
                ActiveBoard {
                    workdir,
                    board_epoch: board_row.board_epoch,
                    members,
                },
            );
        }

        self.snapshot.store(Arc::new(next_snapshot));
        if touched_any {
            self.messaging
                .runtime_service
                .ui_events()
                .publish(UiEventTopics::SESSION_RUNTIMES);
        }
        Ok(())
    }

    async fn destroy_board_and_steers(
        &self,
        workdir: &str,
    ) -> Result<Option<(u64, BTreeSet<String>)>, ChatDbError> {
        let escaped_workdir = AttributeText(workdir).to_string();
        let target_attr = format!("workdir=\"{escaped_workdir}\"");
        self.db()
            .destroy_swarm_board(workdir, move |_epoch, xml| {
                if !xml.contains(&target_attr) {
                    return DissolvedSteerAction::Keep;
                }
                if xml.contains("<board-notice ") {
                    return DissolvedSteerAction::Drop;
                }
                if xml.contains("<board-message ")
                    && let Some(rewritten) = mark_mention_xml_dissolved(xml, workdir)
                {
                    return DissolvedSteerAction::RewriteXml(rewritten);
                }
                DissolvedSteerAction::Keep
            })
            .await
    }

    /// Reads entries from `session_id`'s active swarm board.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when no board is active or the read fails.
    pub async fn read_board(
        &self,
        session_id: &str,
        mode: SwarmReadMode,
        limit: Option<usize>,
    ) -> Result<String, SwarmError> {
        self.reconcile().await?;
        let board = self
            .active_board_for(session_id)
            .ok_or(SwarmError::NoActiveBoard)?;
        let cap = limit
            .unwrap_or_else(|| self.config.load().experimental.swarm_read_limit)
            .max(1);
        let result = self
            .db()
            .read_swarm_messages(&board.workdir, session_id, mode, cap)
            .await?
            .ok_or(SwarmError::NoActiveBoard)?;

        let rendered: Vec<RenderedMessage<'_>> = result
            .messages
            .iter()
            .map(|msg| RenderedMessage {
                board_epoch: msg.board_epoch,
                seq: msg.seq,
                kind: msg.kind.as_str(),
                sender: &msg.sender,
                session_id: msg.session_id.as_deref(),
                body: UntrustedText(&msg.body),
            })
            .collect();

        Ok(SwarmReadTemplate {
            board_epoch: result.board_epoch,
            max_seq: result.max_seq,
            unread_only: matches!(mode, SwarmReadMode::Unread),
            messages: rendered,
            next_seq: result.next_seq,
        }
        .render()?
        .trim_end()
        .to_string())
    }

    /// Lists the working sessions currently attached to `session_id`'s board.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when no board is active.
    pub async fn list_members(&self, session_id: &str) -> Result<String, SwarmError> {
        self.reconcile().await?;
        let board = self
            .active_board_for(session_id)
            .ok_or(SwarmError::NoActiveBoard)?;
        let display_dir = crate::fs_utils::display_path(Path::new(&board.workdir));
        let members: Vec<RenderedMember<'_>> = board
            .members
            .iter()
            .map(|member| RenderedMember {
                name: &member.name,
                session_id: &member.session_id,
                activity: member.activity,
                roots: &member.roots,
                is_self: member.session_id == session_id,
            })
            .collect();
        Ok(SwarmMembersTemplate {
            workdir: &display_dir,
            board_epoch: board.board_epoch,
            members,
        }
        .render()?
        .trim_end()
        .to_string())
    }

    /// Posts `body` onto `session_id`'s active swarm board.
    ///
    /// Delivers framed steers to any `@mention`ed peers, waking even cold
    /// recipients unless their unattended `@mention` wake cap has been reached.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when no board is active, a `@mention` is
    /// ambiguous, or storage fails.
    pub async fn post_message(&self, session_id: &str, body: &str) -> Result<String, SwarmError> {
        self.reconcile().await?;
        let board = self
            .active_board_for(session_id)
            .ok_or(SwarmError::NoActiveBoard)?;

        let sender = board
            .members
            .iter()
            .find(|m| m.session_id == session_id)
            .map_or(session_id, |m| m.name.as_str());

        let recipients = resolve_mentions(body, &board.members, session_id)?;

        let row = self
            .db()
            .append_swarm_message(NewSwarmMessage {
                workdir: &board.workdir,
                session_id: Some(session_id),
                sender,
                kind: SwarmMessageKind::Post,
                body,
            })
            .await?
            .ok_or(SwarmError::NoActiveBoard)?;

        let wake_limit = self.config.load().experimental.swarm_mention_wake_limit;
        let mut woken = Vec::new();
        let mut queued_capped = Vec::new();

        for peer in recipients {
            let should_wake = self
                .db()
                .record_swarm_mention_wake(&peer.session_id, wake_limit)
                .await?;
            let xml = SwarmMentionTemplate {
                sender: AttributeText(sender),
                sender_id: AttributeText(session_id),
                board_epoch: row.board_epoch,
                seq: row.seq,
                workdir: AttributeText(&board.workdir),
                dissolved: false,
                wake_capped: !should_wake,
                body: UntrustedText(body),
            }
            .render()?;
            let hidden = XmlBlock::parse(xml.trim_end()).map_err(AgentLinkError::from)?;
            let pending = PendingUserMessage {
                id: None,
                state: PendingMessageState::default(),
                error: None,
                content: String::new(),
                attachments: Vec::new(),
                hidden_context_xml: Some(hidden),
                created_at: jiff::Timestamp::now().to_string(),
                delivery_mode: PendingUserMessageMode::Steer,
            };
            self.messaging
                .runtime_service
                .enqueue(&peer.session_id, pending, PendingUserMessageMode::Steer)
                .await
                .map_err(AgentLinkError::Db)?;

            if should_wake {
                self.messaging.directory.wake(&peer.session_id);
                woken.push(format!("{} [id {}]", peer.name, peer.session_id));
            } else {
                queued_capped.push(format!("{} [id {}]", peer.name, peer.session_id));
            }
        }

        Ok(SwarmPostedTemplate {
            board_epoch: row.board_epoch,
            seq: row.seq,
            woken,
            queued_capped,
        }
        .render()?
        .trim_end()
        .to_string())
    }

    /// Announces a PR lifecycle event onto the actor's swarm board.
    ///
    /// Emits when the PR's `repo_key` matches the swarm checkout's repository,
    /// pushing actionable transitions to warm peers.
    pub async fn announce_pr_event(&self, event: SwarmPrAnnouncement<'_>) {
        let SwarmPrAnnouncement {
            actor_session_id,
            repo_key,
            workspace_config,
            pr,
            kind,
            detail,
            changed_files,
        } = event;
        if !self.is_enabled() {
            return;
        }
        if let Err(error) = self.reconcile().await {
            tracing::warn!(%error, session_id = %actor_session_id, "failed to reconcile swarm before PR announcement");
            return;
        }
        let Some(board) = self.active_board_for(actor_session_id) else {
            return;
        };

        let may_config = self.config.load();
        if !checkout_matches_repo_key(
            Path::new(&board.workdir),
            repo_key,
            workspace_config,
            &may_config,
        ) {
            return;
        }

        let head_short = pr
            .head_sha
            .as_deref()
            .and_then(|sha| {
                sha.char_indices()
                    .nth(8)
                    .map(|(idx, _)| &sha[..idx])
                    .or(Some(sha))
            })
            .unwrap_or("HEAD");
        let merge_short = pr
            .merge_commit_sha
            .as_deref()
            .or(pr.head_sha.as_deref())
            .and_then(|sha| {
                sha.char_indices()
                    .nth(8)
                    .map(|(idx, _)| &sha[..idx])
                    .or(Some(sha))
            })
            .unwrap_or("HEAD");

        let (action, should_push) = match kind {
            "opened" => (PrSwarmAction::Opened, true),
            "queued" => (PrSwarmAction::Queued, true),
            "merged" => (PrSwarmAction::Merged { sha: merge_short }, true),
            "closed" => (
                PrSwarmAction::Cancelled {
                    reason: detail.map_or_else(|| "closed".to_string(), |d| format!("closed: {d}")),
                },
                true,
            ),
            "check_failed" => (
                PrSwarmAction::Cancelled {
                    reason: detail.map_or_else(
                        || "check failed".to_string(),
                        |d| format!("check failed: {d}"),
                    ),
                },
                true,
            ),
            "removed_from_queue" => (
                PrSwarmAction::Cancelled {
                    reason: detail.map_or_else(
                        || "removed from merge queue".to_string(),
                        |d| format!("removed from merge queue: {d}"),
                    ),
                },
                true,
            ),
            "rejected" => (
                PrSwarmAction::Cancelled {
                    reason: detail.map_or_else(
                        || "merge conflict / rejected".to_string(),
                        |d| format!("rejected: {d}"),
                    ),
                },
                true,
            ),
            "requeued" | "watch_timeout" | "updated" => (
                PrSwarmAction::Timeline {
                    event: detail.map_or_else(|| kind.to_string(), |d| format!("{kind}: {d}")),
                },
                false,
            ),
            _ => return,
        };

        let actor_name = board
            .members
            .iter()
            .find(|m| m.session_id == actor_session_id)
            .map_or(actor_session_id, |m| m.name.as_str());

        let shown_count = changed_files.len().min(MAX_INLINE_PR_FILES);
        let extra_files = changed_files.len().saturating_sub(shown_count);
        let Ok(body) = SwarmPrEntryTemplate {
            actor: actor_name,
            actor_id: actor_session_id,
            sigil: pr_sigil(&pr.backend),
            number: pr.number,
            title: &pr.title,
            head_sha: head_short,
            action,
            files: &changed_files[..shown_count],
            extra_files,
        }
        .render() else {
            return;
        };
        let body = body.trim_end().to_string();

        let row = match self
            .db()
            .append_swarm_message(NewSwarmMessage {
                workdir: &board.workdir,
                session_id: Some(actor_session_id),
                sender: actor_name,
                kind: SwarmMessageKind::Pr,
                body: &body,
            })
            .await
        {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, "failed to append PR event to swarm board");
                return;
            }
        };

        if !should_push {
            return;
        }

        for peer in &board.members {
            if peer.session_id == actor_session_id {
                continue;
            }
            let Ok(notice_xml) = SwarmNoticeTemplate {
                board_epoch: row.board_epoch,
                seq: row.seq,
                workdir: AttributeText(&board.workdir),
                kind: "PR",
                body: UntrustedText(&body),
            }
            .render() else {
                continue;
            };
            let Ok(hidden) = XmlBlock::parse(notice_xml.trim_end()) else {
                tracing::warn!(session_id = %peer.session_id, "failed to frame swarm PR notice XML");
                continue;
            };
            let pending = PendingUserMessage {
                id: None,
                state: PendingMessageState::default(),
                error: None,
                content: String::new(),
                attachments: Vec::new(),
                hidden_context_xml: Some(hidden),
                created_at: jiff::Timestamp::now().to_string(),
                delivery_mode: PendingUserMessageMode::Steer,
            };
            if let Err(error) = self
                .messaging
                .runtime_service
                .enqueue(&peer.session_id, pending, PendingUserMessageMode::Steer)
                .await
            {
                tracing::warn!(%error, session_id = %peer.session_id, "failed to enqueue swarm PR notice");
                continue;
            }
            match self
                .messaging
                .is_cold(&peer.session_id, peer.activity)
                .await
            {
                Ok(false) => self.messaging.directory.wake(&peer.session_id),
                Ok(true) => {}
                Err(error) => {
                    tracing::warn!(%error, session_id = %peer.session_id, "failed to inspect cold state for swarm PR notice");
                }
            }
        }
    }
}

fn checkout_matches_repo_key(
    worktree: &Path,
    repo_key: &str,
    workspace_config: &WorkspaceConfig,
    may_config: &MayConfig,
) -> bool {
    if let Ok(Some(source)) =
        crate::repo_clone::CloneSource::of(worktree, workspace_config, may_config)
        && source.key() == repo_key
    {
        return true;
    }
    if let Ok(identity) = crate::issue::repository_identity(worktree)
        && identity.to_string_lossy() == repo_key
    {
        return true;
    }
    false
}

fn is_mention_prefix_boundary(prev: Option<char>) -> bool {
    prev.is_none_or(|ch| {
        ch.is_whitespace() || matches!(ch, '(' | '[' | '{' | ',' | ';' | ':' | '"' | '\'')
    })
}

fn is_mention_suffix_boundary(next: Option<char>) -> bool {
    next.is_none_or(|ch| {
        ch.is_whitespace()
            || matches!(
                ch,
                '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\''
            )
    })
}

/// Scans `body` for `@<session-id>` and `@<member-name>` (longest-prefix match
/// on a word/punctuation boundary), ignoring self-mentions and failing fast if a
/// title matches multiple members on the board.
fn resolve_mentions<'a>(
    body: &str,
    members: &'a [AgentStatus],
    sender_id: &str,
) -> Result<Vec<&'a AgentStatus>, SwarmError> {
    let mut resolved: Vec<&'a AgentStatus> = Vec::new();
    let mut seen_ids: BTreeSet<&'a str> = BTreeSet::new();

    let mut prev_char: Option<char> = None;
    for (byte_idx, ch) in body.char_indices() {
        if ch == '@' && is_mention_prefix_boundary(prev_char) {
            let after_at = &body[byte_idx + ch.len_utf8()..];
            if !after_at.is_empty() {
                // 1. Exact session_id match on a suffix boundary.
                let mut id_matched = false;
                for member in members {
                    if let Some(rest) = after_at.strip_prefix(member.session_id.as_str())
                        && is_mention_suffix_boundary(rest.chars().next())
                    {
                        if member.session_id != sender_id && seen_ids.insert(&member.session_id) {
                            resolved.push(member);
                        }
                        id_matched = true;
                        break;
                    }
                }
                if !id_matched {
                    // 2. Longest-prefix match against member names.
                    let mut best_len = 0_usize;
                    let mut best_matches: Vec<&'a AgentStatus> = Vec::new();
                    for member in members {
                        if member.name.is_empty() {
                            continue;
                        }
                        if let Some(rest) = after_at.strip_prefix(member.name.as_str())
                            && is_mention_suffix_boundary(rest.chars().next())
                        {
                            let name_len = member.name.len();
                            if name_len > best_len {
                                best_len = name_len;
                                best_matches.clear();
                                best_matches.push(member);
                            } else if name_len == best_len {
                                best_matches.push(member);
                            }
                        }
                    }
                    if best_matches.len() > 1 {
                        let mention = &after_at[..best_len];
                        let candidates = best_matches
                            .iter()
                            .map(|m| format!("@{} ({})", m.session_id, m.name))
                            .collect();
                        let message = SwarmAmbiguousMentionTemplate {
                            mention,
                            candidates,
                        }
                        .render()?
                        .trim_end()
                        .to_string();
                        return Err(SwarmError::AmbiguousMention(message));
                    }
                    if let [member] = best_matches.as_slice()
                        && member.session_id != sender_id
                        && seen_ids.insert(&member.session_id)
                    {
                        resolved.push(member);
                    }
                }
            }
        }
        prev_char = Some(ch);
    }

    Ok(resolved)
}

fn extract_xml_attr<'a>(tag_header: &'a str, key: &str) -> Option<&'a str> {
    let pattern = format!("{key}=\"");
    let start = tag_header.find(&pattern)? + pattern.len();
    let rest = &tag_header[start..];
    let end = rest.find('"')?;
    Some(&rest[..end])
}

/// Re-renders an unconsumed `<board-message>` XML block with `dissolved = true`
/// when its swarm board is destroyed before the recipient runs a turn.
fn mark_mention_xml_dissolved(xml: &str, workdir: &str) -> Option<XmlBlock> {
    let open_idx = xml.find("<board-message ")?;
    let header_end = xml[open_idx..].find('>')? + open_idx;
    let header = &xml[open_idx..=header_end];
    let close_tag = "</board-message>";
    let body_start = header_end + 1;
    let close_idx = xml[body_start..].rfind(close_tag)? + body_start;
    let raw_inner = &xml[body_start..close_idx];
    let inner_body = raw_inner.strip_prefix('\n').unwrap_or(raw_inner);
    let inner_body = inner_body.strip_suffix('\n').unwrap_or(inner_body);

    let sender = extract_xml_attr(header, "from")?;
    let sender_id = extract_xml_attr(header, "id")?;
    let board_epoch: u64 = extract_xml_attr(header, "epoch")?.parse().ok()?;
    let seq: u64 = extract_xml_attr(header, "seq")?.parse().ok()?;
    let wake_capped = xml.contains("Automatic @mention wakeups for this session reached");

    let rendered = SwarmMentionTemplate {
        sender: AttributeText(sender),
        sender_id: AttributeText(sender_id),
        board_epoch,
        seq,
        workdir: AttributeText(workdir),
        dissolved: true,
        wake_capped,
        body: UntrustedText(inner_body),
    }
    .render()
    .ok()?;
    XmlBlock::parse(rendered.trim_end()).ok()
}

#[cfg(test)]
mod tests;
