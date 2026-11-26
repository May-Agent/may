//! Experimental repository-first and cross-machine agent swarm coordination.
//!
//! Coordinates working sessions sharing a canonical `ForgeRepo` across git
//! worktrees, same-owner Tailscale peers, and Codespaces cloud homes (falling
//! back to `<machine>:<worktree_root>` for local-only checkouts). Elects a
//! single-writer Board Host per active swarm while mirroring lightweight
//! `(board_epoch, max_seq, members)` summaries over `/api/events` SSE so
//! request-time `SwarmConditions` and dynamic `board` `CommandSet` checks run
//! with zero cross-machine RTT.

mod conditions;
pub mod federation;
mod tool;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use askama::Template;

use may_protocol::session_runtime::PendingMessageState;
use may_protocol::xml::XmlBlock;

use crate::agent_link::{
    AgentLinkError, AgentMessaging, AttributeText, UntrustedText, split_qualified_target,
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
pub use federation::{
    AnySwarmTransport, IncumbentBoardHost, PeerSwarmDeliverRequest, PeerSwarmDeliverResponse,
    PeerSwarmErrorBody, PeerSwarmPostRequest, PeerSwarmPostResponse, PeerSwarmPrRequest,
    PeerSwarmReadRequest, PeerSwarmReadResponse, PeerSwarmStateResponse, PeerSwarmWorld,
    RemoteWorkingSession, SwarmBoardSummary, SwarmBoardsSsePayload, SwarmMember, SwarmTransport,
    WireSwarmMessage, WireSwarmReadMode, display_group_key, elect_board_host,
    group_matches_pr_repo, is_forge_group_key, resolve_board_host, resolve_session_group_key,
};
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

/// Active board snapshot for one repository or local worktree group.
#[derive(Debug, Clone)]
pub struct ActiveBoard {
    /// Canonical group key (`github:owner/repo` or `<machine>:<worktree_root>`).
    pub workdir: String,
    /// Elected authoritative Board Host machine (`""` when local-only without transport).
    pub host_machine: String,
    /// True when this daemon's SQLite owns the authoritative single-writer board.
    pub is_local_host: bool,
    /// Monotonic generation of this board.
    pub board_epoch: u64,
    /// Highest message sequence number known on this board (`0` when empty).
    pub max_seq: u64,
    /// Working sessions currently attached to this board across participating machines.
    pub members: Vec<SwarmMember>,
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
    /// `board` was called when the session's repository/checkout has no active swarm.
    #[error(
        "no active swarm board for this repository (`[experimental] swarm` requires > 1 working session)"
    )]
    NoActiveBoard,
    /// Message file was empty.
    #[error("refusing to post an empty board message from {0}")]
    EmptyMessage(PathBuf),
    /// `@mention` matched multiple members with the same title or bare id.
    #[error("{0}")]
    AmbiguousMention(String),
    /// Remote Board Host RPC failed.
    #[error("remote swarm board host error: {0}")]
    Remote(String),
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
    transport: Option<AnySwarmTransport>,
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
    address: &'a str,
    activity: crate::agent_link::Activity,
    suspended: bool,
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

#[derive(Debug, Clone)]
struct CandidateMember {
    member: SwarmMember,
    voter_machine: String,
    is_local: bool,
}

fn has_member_or_voter(candidates: &[CandidateMember], machine: &str) -> bool {
    candidates
        .iter()
        .any(|c| c.voter_machine == machine || (c.member.machine == machine && !c.member.suspended))
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
            transport: None,
            snapshot: Arc::new(ArcSwap::from_pointee(ActiveSwarmSnapshot::default())),
            reconcile_lock: Arc::new(async_lock::Mutex::new(())),
        }
    }

    /// Attaches a cross-machine [`SwarmTransport`] enabling multi-host Board Host
    /// election, SSE summary mirroring, and remote board RPCs.
    #[must_use]
    pub fn with_transport(mut self, transport: impl SwarmTransport) -> Self {
        self.transport = Some(AnySwarmTransport::new(transport));
        self
    }

    /// Attaches an already-wrapped [`AnySwarmTransport`].
    #[must_use]
    pub fn with_any_transport(mut self, transport: AnySwarmTransport) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Whether cross-machine peer transport is attached.
    #[must_use]
    pub const fn has_transport(&self) -> bool {
        self.transport.is_some()
    }

    /// Normalized machine name of this daemon (`""` when no transport is attached).
    #[must_use]
    pub fn local_machine(&self) -> String {
        self.transport
            .as_ref()
            .map_or_else(String::new, AnySwarmTransport::local_machine)
    }

    /// Cursor key used for a local session on locally hosted boards.
    #[must_use]
    pub fn local_cursor_key(&self, session_id: &str) -> String {
        let machine = self.local_machine();
        if machine.is_empty() {
            session_id.to_string()
        } else {
            format!("{machine}:{session_id}")
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

    /// Active board for `session_id` from the latest reconciled or SSE-mirrored
    /// in-memory snapshot (zero network RTT).
    #[must_use]
    pub fn active_board_for(&self, session_id: &str) -> Option<ActiveBoard> {
        if !self.is_enabled() {
            return None;
        }
        self.snapshot.load().board_for_session(session_id).cloned()
    }

    /// Authoritative board summaries currently hosted on this daemon, emitted on
    /// `/api/events?topics=swarm` and `GET /api/peers/swarm/state`.
    #[must_use]
    pub fn hosted_board_summaries(&self) -> Vec<SwarmBoardSummary> {
        if !self.is_enabled() {
            return Vec::new();
        }
        let local_machine = self.local_machine();
        let snap = self.snapshot.load();
        snap.boards
            .values()
            .filter(|b| b.is_local_host)
            .map(|b| SwarmBoardSummary {
                group_key: b.workdir.clone(),
                host_machine: if b.host_machine.is_empty() {
                    local_machine.clone()
                } else {
                    b.host_machine.clone()
                },
                board_epoch: b.board_epoch,
                max_seq: b.max_seq,
                members: b.members.clone(),
            })
            .collect()
    }

    /// Returns local non-archived working sessions formatted for `GET /api/peers/swarm/state`.
    ///
    /// # Errors
    /// Returns [`SwarmError`] on storage or `tasks.md` read failure.
    pub async fn local_working_sessions(
        &self,
        local_machine: &str,
    ) -> Result<Vec<RemoteWorkingSession>, SwarmError> {
        if !self.is_enabled() {
            return Ok(Vec::new());
        }
        let db = self.db();
        let ((live_agents, working_ids), session_rows) = futures_lite::future::zip(
            futures_lite::future::zip(self.messaging.directory.live(), db.working_session_ids()),
            db.list_sessions(),
        )
        .await;
        let live_agents = live_agents?;
        let working_ids = working_ids?;
        let session_rows = session_rows?;
        let rows_by_id: BTreeMap<&str, &crate::db::entity::sessions::Model> = session_rows
            .iter()
            .map(|row| (row.id.as_str(), row))
            .collect();

        let mut out = Vec::new();
        for agent in live_agents {
            if !working_ids.contains(&agent.session_id) {
                continue;
            }
            let Some(row) = rows_by_id.get(agent.session_id.as_str()) else {
                continue;
            };
            let Some(group_key) =
                resolve_session_group_key(row, &self.may_dir, Some(local_machine))
            else {
                continue;
            };
            out.push(RemoteWorkingSession {
                machine: local_machine.to_string(),
                voter_machine: local_machine.to_string(),
                session_id: agent.session_id,
                name: agent.name,
                activity: agent.activity,
                suspended: false,
                roots: agent.roots,
                group_key,
            });
        }
        Ok(out)
    }

    /// Applies an SSE `"swarm"` frame pushed from `host_machine` directly into the
    /// local zero-RTT snapshot so `SwarmConditions` and `BoardCommandSet` see new
    /// `max_seq` / `board_epoch` values immediately without polling.
    pub async fn apply_mirrored_boards(&self, host_machine: &str, boards: Vec<SwarmBoardSummary>) {
        if !self.is_enabled() {
            return;
        }
        let local_machine = self.local_machine();
        if host_machine == local_machine {
            return;
        }
        for summary in &boards {
            let _ = self
                .db()
                .observe_swarm_epoch(&summary.group_key, summary.board_epoch)
                .await;
        }
        let _guard = self.reconcile_lock.lock().await;
        let current = (**self.snapshot.load()).clone();
        let mut next = ActiveSwarmSnapshot {
            boards: current
                .boards
                .into_iter()
                .filter(|(_, b)| b.is_local_host || b.host_machine != host_machine)
                .collect(),
            session_to_workdir: BTreeMap::new(),
        };
        for summary in boards {
            if next
                .boards
                .get(&summary.group_key)
                .is_some_and(|existing| existing.is_local_host)
            {
                continue;
            }
            next.boards.insert(
                summary.group_key.clone(),
                ActiveBoard {
                    workdir: summary.group_key,
                    host_machine: summary.host_machine,
                    is_local_host: false,
                    board_epoch: summary.board_epoch,
                    max_seq: summary.max_seq,
                    members: summary.members,
                },
            );
        }
        // Preserve local session bindings for boards that remain active in case the
        // remote host's SSE snapshot has not reconciled a newly joined local session yet.
        for (session_id, group_key) in current.session_to_workdir {
            if next.boards.contains_key(&group_key) {
                next.session_to_workdir.insert(session_id, group_key);
            }
        }
        for (group_key, board) in &next.boards {
            for member in &board.members {
                if member.machine == local_machine {
                    next.session_to_workdir
                        .insert(member.session_id.clone(), group_key.clone());
                }
            }
        }
        self.snapshot.store(Arc::new(next));
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

    /// Reconciles active swarm boards against live local and remote working sessions,
    /// deterministic Board Host election, and the `[experimental] swarm` switch.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when reading sessions or updating board rows fails.
    pub async fn reconcile(&self) -> Result<(), SwarmError> {
        let _guard = self.reconcile_lock.lock().await;
        let db = self.db();

        let prev_snapshot = self.snapshot.load_full();
        let prev_hosted = self.hosted_board_summaries();

        if !self.is_enabled() {
            let existing = db.list_swarm_boards().await?;
            let mut touched_any = false;
            for board in existing {
                if let Some((epoch, touched)) =
                    self.destroy_board_and_steers(&board.workdir).await?
                {
                    if !touched.is_empty() {
                        touched_any = true;
                    }
                    self.broadcast_dissolved_to_peers(&board.workdir, epoch, &prev_hosted)
                        .await;
                }
            }
            db.purge_all_buffered_cloud_notices().await?;
            self.snapshot
                .store(Arc::new(ActiveSwarmSnapshot::default()));
            if touched_any {
                self.messaging
                    .runtime_service
                    .ui_events()
                    .publish(UiEventTopics::SESSION_RUNTIMES);
            }
            if !prev_hosted.is_empty()
                && let Some(transport) = &self.transport
            {
                transport.notify_hosted_boards_changed();
            }
            return Ok(());
        }

        let local_draining = self
            .transport
            .as_ref()
            .is_some_and(AnySwarmTransport::is_draining);
        let peer_world = if let Some(transport) = &self.transport {
            Some(transport.poll_world().await)
        } else {
            None
        };
        let local_machine = peer_world
            .as_ref()
            .map_or_else(String::new, |w| w.local_machine.clone());
        let local_machine_opt = (!local_machine.is_empty()).then_some(local_machine.as_str());

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

        let mut grouped: BTreeMap<String, Vec<CandidateMember>> = BTreeMap::new();
        for agent in live_agents {
            if !working_ids.contains(&agent.session_id) {
                continue;
            }
            let Some(row) = rows_by_id.get(agent.session_id.as_str()) else {
                continue;
            };
            let Some(group_key) = resolve_session_group_key(row, &self.may_dir, local_machine_opt)
            else {
                continue;
            };
            let address = if local_machine.is_empty() {
                agent.session_id.clone()
            } else {
                format!("{local_machine}:{}", agent.session_id)
            };
            grouped.entry(group_key).or_default().push(CandidateMember {
                member: SwarmMember {
                    machine: local_machine.clone(),
                    session_id: agent.session_id,
                    address,
                    name: agent.name,
                    activity: agent.activity,
                    suspended: false,
                    roots: agent.roots,
                },
                voter_machine: local_machine.clone(),
                is_local: true,
            });
        }

        let mut remote_boards_by_group: BTreeMap<String, Vec<SwarmBoardSummary>> = BTreeMap::new();
        if let Some(world) = peer_world {
            for summary in world.remote_boards {
                let _ = db
                    .observe_swarm_epoch(&summary.group_key, summary.board_epoch)
                    .await;
                remote_boards_by_group
                    .entry(summary.group_key.clone())
                    .or_default()
                    .push(summary);
            }
            for remote in world.remote_sessions {
                if remote.machine == local_machine {
                    continue;
                }
                let address = format!("{}:{}", remote.machine, remote.session_id);
                grouped
                    .entry(remote.group_key)
                    .or_default()
                    .push(CandidateMember {
                        member: SwarmMember {
                            machine: remote.machine,
                            session_id: remote.session_id,
                            address,
                            name: remote.name,
                            activity: remote.activity,
                            suspended: remote.suspended,
                            roots: remote.roots,
                        },
                        voter_machine: remote.voter_machine,
                        is_local: false,
                    });
            }
        }

        // Deduplicate candidates by `(machine, session_id)` and sort deterministically.
        for candidates in grouped.values_mut() {
            candidates.sort_by(|a, b| {
                a.member
                    .machine
                    .cmp(&b.member.machine)
                    .then_with(|| a.member.name.cmp(&b.member.name))
                    .then_with(|| a.member.session_id.cmp(&b.member.session_id))
            });
            candidates.dedup_by(|a, b| {
                a.member.machine == b.member.machine && a.member.session_id == b.member.session_id
            });
            // When no cross-machine peer participates in a board and no transport is
            // needed to disambiguate, keep bare `session_id` addresses on purely
            // single-daemon setups without transport; with transport, keep `<machine>:<id>`
            // whenever multiple machines participate, or bare `<id>` when all members
            // sit on this machine alone so single-machine ergonomics stay terse.
            let multi_machine = candidates.first().is_some_and(|first| {
                candidates
                    .iter()
                    .any(|c| c.member.machine != first.member.machine)
            });
            if !multi_machine {
                for c in candidates.iter_mut() {
                    if c.is_local {
                        c.member.address.clone_from(&c.member.session_id);
                    }
                }
            }
        }

        let existing_by_workdir: BTreeMap<&str, &crate::db::SwarmBoardRow> = existing_boards
            .iter()
            .map(|b| (b.workdir.as_str(), b))
            .collect();

        let mut resolved_hosts: BTreeMap<String, Option<String>> = BTreeMap::new();
        for (group_key, candidates) in &grouped {
            if candidates.len() <= 1 {
                continue;
            }
            if local_machine.is_empty() {
                resolved_hosts.insert(group_key.clone(), Some(String::new()));
                continue;
            }
            let mut incumbents = Vec::new();
            if !local_draining
                && let Some(local_board) = existing_by_workdir.get(group_key.as_str())
                && has_member_or_voter(candidates, &local_machine)
            {
                incumbents.push(IncumbentBoardHost {
                    machine: local_machine.as_str(),
                    board_epoch: local_board.board_epoch,
                });
            }
            if let Some(remotes) = remote_boards_by_group.get(group_key) {
                for remote in remotes {
                    if !remote.host_machine.is_empty()
                        && remote.host_machine != local_machine
                        && has_member_or_voter(candidates, &remote.host_machine)
                    {
                        incumbents.push(IncumbentBoardHost {
                            machine: remote.host_machine.as_str(),
                            board_epoch: remote.board_epoch,
                        });
                    }
                }
            }
            let voters = candidates
                .iter()
                .filter(|c| !(local_draining && c.voter_machine == local_machine))
                .map(|c| c.voter_machine.as_str());
            let resolved = resolve_board_host(group_key, incumbents, voters);
            resolved_hosts.insert(group_key.clone(), resolved);
        }

        // Destroy any locally persisted board that fell to `<= 1` members, OR
        // step down cleanly when Board Host ownership moved while `> 1` working
        // members remain (e.g. local drain or host departure).
        let mut touched_any = false;
        for board in &existing_boards {
            let fell_below_quorum = grouped.get(&board.workdir).is_none_or(|c| c.len() <= 1);
            let should_keep_locally = !local_draining
                && !fell_below_quorum
                && (local_machine.is_empty()
                    || resolved_hosts
                        .get(&board.workdir)
                        .and_then(|h| h.as_deref())
                        == Some(local_machine.as_str()));
            if !should_keep_locally {
                if fell_below_quorum {
                    if let Some((epoch, touched)) =
                        self.destroy_board_and_steers(&board.workdir).await?
                    {
                        if !touched.is_empty() {
                            touched_any = true;
                        }
                        self.broadcast_dissolved_to_peers(&board.workdir, epoch, &prev_hosted)
                            .await;
                    }
                } else {
                    let _ = db.step_down_swarm_board(&board.workdir).await?;
                }
            }
        }

        // Also purge buffered cloud notices if a remotely hosted board known to
        // this daemon dissolved without delivering a `Dissolved` signal.
        for prev_group in prev_snapshot.boards.keys() {
            if grouped.get(prev_group).is_none_or(|c| c.len() <= 1) {
                db.purge_buffered_cloud_notices_for_board(prev_group)
                    .await?;
            }
        }

        let mut next_snapshot = ActiveSwarmSnapshot::default();
        for (group_key, candidates) in grouped {
            if candidates.len() <= 1 {
                continue;
            }
            let elected_host = if local_machine.is_empty() {
                String::new()
            } else {
                let Some(host) = resolved_hosts
                    .get(&group_key)
                    .and_then(std::clone::Clone::clone)
                else {
                    continue;
                };
                host
            };
            let is_local_host =
                !local_draining && (local_machine.is_empty() || elected_host == local_machine);
            let members: Vec<SwarmMember> = candidates.iter().map(|c| c.member.clone()).collect();

            if is_local_host {
                let (board_row, created) = db.open_or_get_swarm_board(&group_key).await?;
                if created {
                    let names: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
                    let display_dir = display_group_key(&group_key);
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
                            workdir: &group_key,
                            session_id: None,
                            sender: "swarm",
                            kind: SwarmMessageKind::Lifecycle,
                            body: &summary,
                        })
                        .await?;
                }
                let max_seq = db.board_max_seq(&group_key).await?;
                for c in &candidates {
                    if c.is_local {
                        next_snapshot
                            .session_to_workdir
                            .insert(c.member.session_id.clone(), group_key.clone());
                    }
                }
                next_snapshot.boards.insert(
                    group_key.clone(),
                    ActiveBoard {
                        workdir: group_key,
                        host_machine: elected_host,
                        is_local_host: true,
                        board_epoch: board_row.board_epoch,
                        max_seq,
                        members,
                    },
                );
            } else {
                let (board_epoch, max_seq, effective_members) = remote_boards_by_group
                    .get(&group_key)
                    .and_then(|list| list.iter().find(|m| m.host_machine == elected_host))
                    .map_or((1, 0, members), |mirrored| {
                        (
                            mirrored.board_epoch.max(1),
                            mirrored.max_seq,
                            mirrored.members.clone(),
                        )
                    });
                let _ = db.observe_swarm_epoch(&group_key, board_epoch).await;
                for c in &candidates {
                    if c.is_local {
                        next_snapshot
                            .session_to_workdir
                            .insert(c.member.session_id.clone(), group_key.clone());
                    }
                }
                next_snapshot.boards.insert(
                    group_key.clone(),
                    ActiveBoard {
                        workdir: group_key,
                        host_machine: elected_host,
                        is_local_host: false,
                        board_epoch,
                        max_seq,
                        members: effective_members,
                    },
                );
            }
        }

        self.snapshot.store(Arc::new(next_snapshot));
        if touched_any {
            self.messaging
                .runtime_service
                .ui_events()
                .publish(UiEventTopics::SESSION_RUNTIMES);
        }
        let next_hosted = self.hosted_board_summaries();
        if next_hosted != prev_hosted
            && let Some(transport) = &self.transport
        {
            transport.notify_hosted_boards_changed();
        }
        Ok(())
    }

    async fn broadcast_dissolved_to_peers(
        &self,
        group_key: &str,
        board_epoch: u64,
        prev_hosted: &[SwarmBoardSummary],
    ) {
        let Some(transport) = &self.transport else {
            return;
        };
        let local_machine = transport.local_machine();
        let Some(summary) = prev_hosted.iter().find(|b| b.group_key == group_key) else {
            return;
        };
        let mut remote_machines: BTreeSet<&str> = BTreeSet::new();
        for m in &summary.members {
            if !m.machine.is_empty() && m.machine != local_machine {
                remote_machines.insert(m.machine.as_str());
            }
        }
        for machine in remote_machines {
            let _ = transport
                .deliver_to_peer(
                    machine,
                    PeerSwarmDeliverRequest::Dissolved {
                        group_key: group_key.to_string(),
                        board_epoch,
                    },
                )
                .await;
        }
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

    /// Reads entries from `session_id`'s active swarm board, routing to the
    /// remote Board Host when hosted on another machine and syncing the local
    /// read cursor for zero-RTT unread indicators.
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

        let result = if board.is_local_host {
            let reader_key = self.local_cursor_key(session_id);
            self.db()
                .read_swarm_messages(&board.workdir, &reader_key, mode, cap)
                .await?
                .ok_or(SwarmError::NoActiveBoard)?
        } else {
            let Some(transport) = &self.transport else {
                return Err(SwarmError::NoActiveBoard);
            };
            let reader_key = self.local_cursor_key(session_id);
            let resp = transport
                .read_remote_board(
                    &board.host_machine,
                    PeerSwarmReadRequest {
                        group_key: board.workdir.clone(),
                        reader_key,
                        mode: mode.into(),
                        limit: cap,
                    },
                )
                .await?;
            let (read_res, cursor) = resp.into_read_result(&board.workdir);
            let _ = self
                .db()
                .set_swarm_cursor(&board.workdir, session_id, cursor)
                .await;
            self.bump_cached_max_seq(&board.workdir, read_res.board_epoch, read_res.max_seq);
            read_res
        };

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
        let local_machine = self.local_machine();
        let display_dir = display_group_key(&board.workdir);
        let members: Vec<RenderedMember<'_>> = board
            .members
            .iter()
            .map(|member| RenderedMember {
                name: &member.name,
                address: &member.address,
                activity: member.activity,
                suspended: member.suspended,
                roots: &member.roots,
                is_self: member.machine == local_machine && member.session_id == session_id,
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

    /// Posts `body` onto `session_id`'s active swarm board, routing through the
    /// authoritative Board Host so `epoch:seq` is assigned by a single writer.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when no board is active, a `@mention` is
    /// ambiguous, or delivery fails.
    pub async fn post_message(&self, session_id: &str, body: &str) -> Result<String, SwarmError> {
        self.reconcile().await?;
        let board = self
            .active_board_for(session_id)
            .ok_or(SwarmError::NoActiveBoard)?;
        let local_machine = self.local_machine();
        let sender_name = board
            .members
            .iter()
            .find(|m| m.machine == local_machine && m.session_id == session_id)
            .map_or(session_id, |m| m.name.as_str())
            .to_string();

        let outcome = if board.is_local_host {
            self.execute_host_post(&board, &local_machine, session_id, &sender_name, body)
                .await?
        } else {
            let Some(transport) = &self.transport else {
                return Err(SwarmError::NoActiveBoard);
            };
            let resp = transport
                .post_remote_board(
                    &board.host_machine,
                    PeerSwarmPostRequest {
                        group_key: board.workdir.clone(),
                        sender_machine: local_machine,
                        sender_session_id: session_id.to_string(),
                        sender_name,
                        body: body.to_string(),
                    },
                )
                .await?;
            // Author's own post advances their local cursor when contiguous.
            if let Ok(cur) = self.db().get_swarm_cursor(&board.workdir, session_id).await
                && cur + 1 == resp.seq
            {
                let _ = self
                    .db()
                    .set_swarm_cursor(&board.workdir, session_id, resp.seq)
                    .await;
            }
            self.bump_cached_max_seq(&board.workdir, resp.board_epoch, resp.seq);
            resp
        };

        Ok(SwarmPostedTemplate {
            board_epoch: outcome.board_epoch,
            seq: outcome.seq,
            woken: outcome.woken,
            queued_capped: outcome.queued_capped,
        }
        .render()?
        .trim_end()
        .to_string())
    }

    async fn execute_host_post(
        &self,
        board: &ActiveBoard,
        sender_machine: &str,
        sender_session_id: &str,
        sender_name: &str,
        body: &str,
    ) -> Result<PeerSwarmPostResponse, SwarmError> {
        let recipients = resolve_mentions(body, &board.members, sender_machine, sender_session_id)?;
        let author_key = if sender_machine.is_empty() {
            sender_session_id.to_string()
        } else {
            format!("{sender_machine}:{sender_session_id}")
        };
        let sender_display_id = board
            .members
            .iter()
            .find(|m| m.machine == sender_machine && m.session_id == sender_session_id)
            .map_or(author_key.as_str(), |m| m.address.as_str());

        let row = self
            .db()
            .append_swarm_message(NewSwarmMessage {
                workdir: &board.workdir,
                session_id: Some(&author_key),
                sender: sender_name,
                kind: SwarmMessageKind::Post,
                body,
            })
            .await?
            .ok_or(SwarmError::NoActiveBoard)?;

        self.bump_cached_max_seq(&board.workdir, row.board_epoch, row.seq);
        if let Some(transport) = &self.transport {
            transport.notify_hosted_boards_changed();
        }

        let local_machine = self.local_machine();
        let wake_limit = self.config.load().experimental.swarm_mention_wake_limit;
        let mut woken = Vec::new();
        let mut queued_capped = Vec::new();

        for peer in recipients {
            if peer.machine == local_machine {
                let outcome = self
                    .deliver_local_mention(
                        &board.workdir,
                        row.board_epoch,
                        row.seq,
                        &peer.session_id,
                        sender_name,
                        sender_display_id,
                        body,
                        wake_limit,
                    )
                    .await?;
                if outcome.woken {
                    woken.push(format!("{} [id {}]", peer.name, peer.address));
                } else {
                    queued_capped.push(format!("{} [id {}]", peer.name, peer.address));
                }
            } else if let Some(transport) = &self.transport {
                let sender_addr = if sender_machine.is_empty() {
                    sender_session_id.to_string()
                } else {
                    format!("{sender_machine}:{sender_session_id}")
                };
                match transport
                    .deliver_to_peer(
                        &peer.machine,
                        PeerSwarmDeliverRequest::Mention {
                            group_key: board.workdir.clone(),
                            board_epoch: row.board_epoch,
                            seq: row.seq,
                            target_session_id: peer.session_id.clone(),
                            sender_name: sender_name.to_string(),
                            sender_address: sender_addr,
                            body: body.to_string(),
                        },
                    )
                    .await
                {
                    Ok(resp) if resp.woken => {
                        woken.push(format!("{} [id {}]", peer.name, peer.address));
                    }
                    Ok(_) => {
                        queued_capped.push(format!("{} [id {}]", peer.name, peer.address));
                    }
                    Err(error) => {
                        tracing::warn!(%error, peer = %peer.address, "failed to deliver cross-machine swarm @mention");
                    }
                }
            }
        }

        Ok(PeerSwarmPostResponse {
            board_epoch: row.board_epoch,
            seq: row.seq,
            woken,
            queued_capped,
        })
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal mention helper carries distinct message coordinates and sender fields"
    )]
    async fn deliver_local_mention(
        &self,
        group_key: &str,
        board_epoch: u64,
        seq: u64,
        target_session_id: &str,
        sender_name: &str,
        sender_id: &str,
        body: &str,
        wake_limit: u32,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        let should_wake = self
            .db()
            .record_swarm_mention_wake(target_session_id, wake_limit)
            .await?;
        let xml = SwarmMentionTemplate {
            sender: AttributeText(sender_name),
            sender_id: AttributeText(sender_id),
            board_epoch,
            seq,
            workdir: AttributeText(group_key),
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
            .enqueue(target_session_id, pending, PendingUserMessageMode::Steer)
            .await
            .map_err(AgentLinkError::Db)?;

        if should_wake {
            self.messaging.directory.wake(target_session_id);
        }
        Ok(PeerSwarmDeliverResponse {
            woken: should_wake,
            capped: !should_wake,
        })
    }

    /// Serves `POST /api/peers/swarm/read` on this Board Host.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when the board is absent or storage fails.
    pub async fn serve_host_read(
        &self,
        req: PeerSwarmReadRequest,
    ) -> Result<PeerSwarmReadResponse, SwarmError> {
        let mode: SwarmReadMode = req.mode.into();
        let res = self
            .db()
            .read_swarm_messages(&req.group_key, &req.reader_key, mode, req.limit.max(1))
            .await?
            .ok_or(SwarmError::NoActiveBoard)?;
        let cursor = self
            .db()
            .get_swarm_cursor(&req.group_key, &req.reader_key)
            .await?;
        Ok(PeerSwarmReadResponse {
            board_epoch: res.board_epoch,
            messages: res
                .messages
                .into_iter()
                .map(WireSwarmMessage::from_row)
                .collect(),
            next_seq: res.next_seq,
            max_seq: res.max_seq,
            cursor,
        })
    }

    /// Serves `POST /api/peers/swarm/post` on this Board Host.
    ///
    /// # Errors
    /// Returns [`SwarmError`] when the board is absent, a `@mention` is ambiguous,
    /// or storage fails.
    pub async fn serve_host_post(
        &self,
        req: PeerSwarmPostRequest,
    ) -> Result<PeerSwarmPostResponse, SwarmError> {
        self.reconcile().await?;
        let board = self
            .snapshot
            .load()
            .boards
            .get(&req.group_key)
            .filter(|b| b.is_local_host)
            .cloned()
            .ok_or(SwarmError::NoActiveBoard)?;
        self.execute_host_post(
            &board,
            &req.sender_machine,
            &req.sender_session_id,
            &req.sender_name,
            &req.body,
        )
        .await
    }

    /// Serves `POST /api/peers/swarm/pr` on this Board Host.
    ///
    /// # Errors
    /// Returns [`SwarmError`] on storage failure.
    pub async fn serve_host_pr(&self, req: PeerSwarmPrRequest) -> Result<(), SwarmError> {
        self.reconcile().await?;
        let Some(board) = self
            .snapshot
            .load()
            .boards
            .get(&req.group_key)
            .filter(|b| b.is_local_host)
            .cloned()
        else {
            return Ok(());
        };
        self.append_and_fan_out_pr(
            &board,
            &req.actor_machine,
            &req.actor_session_id,
            &req.actor_name,
            &req.backend,
            req.number,
            &req.title,
            req.head_sha.as_deref(),
            req.merge_commit_sha.as_deref(),
            &req.kind,
            req.detail.as_deref(),
            &req.changed_files,
        )
        .await;
        Ok(())
    }

    /// Serves `POST /api/peers/swarm/deliver` on a member's local daemon.
    ///
    /// # Errors
    /// Returns [`SwarmError`] on storage or XML framing failure.
    pub async fn receive_peer_delivery(
        &self,
        req: PeerSwarmDeliverRequest,
    ) -> Result<PeerSwarmDeliverResponse, SwarmError> {
        match req {
            PeerSwarmDeliverRequest::Mention {
                group_key,
                board_epoch,
                seq,
                target_session_id,
                sender_name,
                sender_address,
                body,
            } => {
                let wake_limit = self.config.load().experimental.swarm_mention_wake_limit;
                self.deliver_local_mention(
                    &group_key,
                    board_epoch,
                    seq,
                    &target_session_id,
                    &sender_name,
                    &sender_address,
                    &body,
                    wake_limit,
                )
                .await
            }
            PeerSwarmDeliverRequest::Notice {
                group_key,
                board_epoch,
                seq,
                target_session_id,
                kind,
                body,
            } => {
                let woken = self
                    .deliver_local_notice(
                        &group_key,
                        board_epoch,
                        seq,
                        &target_session_id,
                        if kind == "PR" { "PR" } else { "notice" },
                        &body,
                    )
                    .await;
                Ok(PeerSwarmDeliverResponse {
                    woken,
                    capped: false,
                })
            }
            PeerSwarmDeliverRequest::Dissolved {
                group_key,
                board_epoch: _,
            } => {
                let escaped_workdir = AttributeText(&group_key).to_string();
                let target_attr = format!("workdir=\"{escaped_workdir}\"");
                let group_owned = group_key.clone();
                let touched = self
                    .db()
                    .rewrite_pending_steers(move |xml| {
                        if !xml.contains(&target_attr) {
                            return DissolvedSteerAction::Keep;
                        }
                        if xml.contains("<board-notice ") {
                            return DissolvedSteerAction::Drop;
                        }
                        if xml.contains("<board-message ")
                            && let Some(rewritten) = mark_mention_xml_dissolved(xml, &group_owned)
                        {
                            return DissolvedSteerAction::RewriteXml(rewritten);
                        }
                        DissolvedSteerAction::Keep
                    })
                    .await?;
                self.db()
                    .purge_buffered_cloud_notices_for_board(&group_key)
                    .await?;
                if !touched.is_empty() {
                    self.messaging
                        .runtime_service
                        .ui_events()
                        .publish(UiEventTopics::SESSION_RUNTIMES);
                }
                Ok(PeerSwarmDeliverResponse::default())
            }
        }
    }

    fn bump_cached_max_seq(&self, group_key: &str, board_epoch: u64, seq: u64) {
        let mut current = (**self.snapshot.load()).clone();
        if let Some(board) = current.boards.get_mut(group_key)
            && board.board_epoch == board_epoch
            && seq > board.max_seq
        {
            board.max_seq = seq;
            self.snapshot.store(Arc::new(current));
        }
    }

    /// Announces a PR lifecycle event onto the actor's swarm board (routing to
    /// the remote Board Host when needed).
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

        let actor_checkout = self
            .db()
            .get_session(actor_session_id)
            .await
            .ok()
            .flatten()
            .and_then(|row| match SessionDirectory::of(&row, &self.may_dir) {
                SessionDirectory::Checkout(path) => Some(path),
                SessionDirectory::Sandbox(_) => None,
            });
        let may_config = self.config.load();
        if !group_matches_pr_repo(
            &board.workdir,
            repo_key,
            actor_checkout.as_deref(),
            workspace_config,
            &may_config,
        ) {
            return;
        }

        let local_machine = self.local_machine();
        let actor_name = board
            .members
            .iter()
            .find(|m| m.machine == local_machine && m.session_id == actor_session_id)
            .map_or(actor_session_id, |m| m.name.as_str())
            .to_string();

        if board.is_local_host {
            self.append_and_fan_out_pr(
                &board,
                &local_machine,
                actor_session_id,
                &actor_name,
                &pr.backend,
                pr.number,
                &pr.title,
                pr.head_sha.as_deref(),
                pr.merge_commit_sha.as_deref(),
                kind,
                detail,
                &changed_files,
            )
            .await;
        } else if let Some(transport) = &self.transport {
            let _ = transport
                .announce_remote_pr(
                    &board.host_machine,
                    PeerSwarmPrRequest {
                        group_key: board.workdir.clone(),
                        actor_machine: local_machine,
                        actor_session_id: actor_session_id.to_string(),
                        actor_name,
                        backend: pr.backend.clone(),
                        number: pr.number,
                        title: pr.title.clone(),
                        head_sha: pr.head_sha.clone(),
                        merge_commit_sha: pr.merge_commit_sha.clone(),
                        kind: kind.to_string(),
                        detail: detail.map(str::to_string),
                        changed_files,
                    },
                )
                .await;
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "PR fan-out helper receives normalized wire fields from local and remote callers"
    )]
    async fn append_and_fan_out_pr(
        &self,
        board: &ActiveBoard,
        actor_machine: &str,
        actor_session_id: &str,
        actor_name: &str,
        backend: &str,
        number: i64,
        title: &str,
        head_sha: Option<&str>,
        merge_commit_sha: Option<&str>,
        kind: &str,
        detail: Option<&str>,
        changed_files: &[String],
    ) {
        let head_short = head_sha
            .and_then(|sha| {
                sha.char_indices()
                    .nth(8)
                    .map(|(idx, _)| &sha[..idx])
                    .or(Some(sha))
            })
            .unwrap_or("HEAD");
        let merge_short = merge_commit_sha
            .or(head_sha)
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

        let actor_display_id = board
            .members
            .iter()
            .find(|m| m.machine == actor_machine && m.session_id == actor_session_id)
            .map_or(actor_session_id, |m| m.address.as_str());
        let author_key = if actor_machine.is_empty() {
            actor_session_id.to_string()
        } else {
            format!("{actor_machine}:{actor_session_id}")
        };

        let shown_count = changed_files.len().min(MAX_INLINE_PR_FILES);
        let extra_files = changed_files.len().saturating_sub(shown_count);
        let Ok(body) = SwarmPrEntryTemplate {
            actor: actor_name,
            actor_id: actor_display_id,
            sigil: pr_sigil(backend),
            number,
            title,
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
                session_id: Some(&author_key),
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

        self.bump_cached_max_seq(&board.workdir, row.board_epoch, row.seq);
        if let Some(transport) = &self.transport {
            transport.notify_hosted_boards_changed();
        }

        if !should_push {
            return;
        }

        let local_machine = self.local_machine();
        for peer in &board.members {
            if peer.machine == actor_machine && peer.session_id == actor_session_id {
                continue;
            }
            if peer.machine == local_machine {
                let _ = self
                    .deliver_local_notice(
                        &board.workdir,
                        row.board_epoch,
                        row.seq,
                        &peer.session_id,
                        "PR",
                        &body,
                    )
                    .await;
            } else if let Some(transport) = &self.transport {
                let _ = transport
                    .deliver_to_peer(
                        &peer.machine,
                        PeerSwarmDeliverRequest::Notice {
                            group_key: board.workdir.clone(),
                            board_epoch: row.board_epoch,
                            seq: row.seq,
                            target_session_id: peer.session_id.clone(),
                            kind: "PR".to_string(),
                            body: body.clone(),
                        },
                    )
                    .await;
            }
        }
    }

    async fn deliver_local_notice(
        &self,
        group_key: &str,
        board_epoch: u64,
        seq: u64,
        target_session_id: &str,
        kind: &'static str,
        body: &str,
    ) -> bool {
        let Ok(notice_xml) = SwarmNoticeTemplate {
            board_epoch,
            seq,
            workdir: AttributeText(group_key),
            kind,
            body: UntrustedText(body),
        }
        .render() else {
            return false;
        };
        let Ok(hidden) = XmlBlock::parse(notice_xml.trim_end()) else {
            tracing::warn!(session_id = %target_session_id, "failed to frame swarm PR notice XML");
            return false;
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
            .enqueue(target_session_id, pending, PendingUserMessageMode::Steer)
            .await
        {
            tracing::warn!(%error, session_id = %target_session_id, "failed to enqueue swarm PR notice");
            return false;
        }
        let activity = self
            .messaging
            .directory
            .live()
            .await
            .ok()
            .and_then(|list| {
                list.into_iter()
                    .find(|a| a.session_id == target_session_id)
                    .map(|a| a.activity)
            })
            .unwrap_or(crate::agent_link::Activity::Idle);
        match self.messaging.is_cold(target_session_id, activity).await {
            Ok(false) => {
                self.messaging.directory.wake(target_session_id);
                true
            }
            Ok(true) => false,
            Err(error) => {
                tracing::warn!(%error, session_id = %target_session_id, "failed to inspect cold state for swarm PR notice");
                false
            }
        }
    }
}

pub(crate) fn checkout_matches_repo_key(
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
    if let crate::trust::RepoKind::Forge { repo } = crate::trust::repo_kind(worktree)
        && repo.cache_key().eq_ignore_ascii_case(repo_key)
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

/// Scans `body` for `@<machine>:<session-id>`, `@<session-id>`, and `@<member-name>`
/// (longest-prefix match on a word/punctuation boundary), ignoring self-mentions
/// and failing fast if a bare id or title matches multiple members on the board.
fn resolve_mentions<'a>(
    body: &str,
    members: &'a [SwarmMember],
    sender_machine: &str,
    sender_id: &str,
) -> Result<Vec<&'a SwarmMember>, SwarmError> {
    let mut resolved: Vec<&'a SwarmMember> = Vec::new();
    let mut seen: BTreeSet<(&'a str, &'a str)> = BTreeSet::new();

    let is_self = |m: &SwarmMember| m.machine == sender_machine && m.session_id == sender_id;

    let mut prev_char: Option<char> = None;
    for (byte_idx, ch) in body.char_indices() {
        if ch == '@' && is_mention_prefix_boundary(prev_char) {
            let after_at = &body[byte_idx + ch.len_utf8()..];
            if !after_at.is_empty() {
                // 1. Machine-qualified `@<machine>:<session-id>` match.
                let mut matched = false;
                for member in members {
                    if !member.machine.is_empty() {
                        let qualified = format!("{}:{}", member.machine, member.session_id);
                        if let Some(rest) = after_at.strip_prefix(qualified.as_str())
                            && is_mention_suffix_boundary(rest.chars().next())
                        {
                            if !is_self(member)
                                && seen.insert((&member.machine, &member.session_id))
                            {
                                resolved.push(member);
                            }
                            matched = true;
                            break;
                        }
                    }
                }
                // 2. Bare `@<session-id>` match (if unambiguous across machines).
                if !matched {
                    let bare_matches: Vec<&'a SwarmMember> = members
                        .iter()
                        .filter(|member| {
                            after_at
                                .strip_prefix(member.session_id.as_str())
                                .is_some_and(|rest| is_mention_suffix_boundary(rest.chars().next()))
                        })
                        .collect();
                    if bare_matches.len() > 1 {
                        let candidates = bare_matches
                            .iter()
                            .map(|m| format!("@{} ({})", m.address, m.name))
                            .collect();
                        let message = SwarmAmbiguousMentionTemplate {
                            mention: bare_matches[0].session_id.as_str(),
                            candidates,
                        }
                        .render()?
                        .trim_end()
                        .to_string();
                        return Err(SwarmError::AmbiguousMention(message));
                    }
                    if let [member] = bare_matches.as_slice() {
                        // Guard against `@<machine>:<id>` where `<machine>` happens to equal
                        // another member's bare `session_id` followed by `:`.
                        let rest = &after_at[member.session_id.len()..];
                        let looks_qualified = rest.starts_with(':')
                            && split_qualified_target(
                                after_at.split_whitespace().next().unwrap_or(""),
                            )
                            .is_some();
                        if !looks_qualified {
                            if !is_self(member)
                                && seen.insert((&member.machine, &member.session_id))
                            {
                                resolved.push(member);
                            }
                            matched = true;
                        }
                    }
                }
                // 3. Longest-prefix match against member display names.
                if !matched {
                    let mut best_len = 0_usize;
                    let mut best_matches: Vec<&'a SwarmMember> = Vec::new();
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
                            .map(|m| format!("@{} ({})", m.address, m.name))
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
                        && !is_self(member)
                        && seen.insert((&member.machine, &member.session_id))
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
