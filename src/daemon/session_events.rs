//! The per-session event stream: `GET /api/sessions/{id}/events`.
//!
//! One stream carries the session's live turn frames (`session_turn`,
//! `session_agent`, `session_suggestion`, `session_recap`) and diffs of its
//! runtime, background work, artifacts and linked issues, all in the
//! session's [`TurnHub`] index space. Every frame's SSE `id` is its cursor
//! (`{epoch}.{index}`): a client reads the session, subscribes with
//! `?after=` its `cursor`, and sees exactly what changed since. A cursor
//! this daemon cannot continue from — malformed, another daemon's epoch or
//! an index already dropped — gets `reset` first, then live events.
//!
//! Change producers notify [`DaemonState::publish_session_change`]; a
//! dispatcher recomputes each piece once per burst, diffs it against what
//! the session last published and emits only real differences as frames.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use futures_lite::StreamExt;
use serde::Serialize;
use skyzen::Result as SkyResult;
use skyzen::extract::Query;
use skyzen::header::{HeaderMap, HeaderName};
use skyzen::responder::Sse;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};

use super::error::bad_request_error;
use super::helpers::send_event;
use super::state::DaemonState;
use super::types::{ArtifactEntry, SessionRuntimeResponse};
use crate::client::background::SessionBackgroundSnapshot;
use crate::session::CancelReason;
use crate::tui::sidebar::IssueBrief;

/// How long a burst of change notices is held so one recompute serves it.
const COALESCE: Duration = Duration::from_millis(50);

/// `Last-Event-ID`, the SSE reconnection header.
const LAST_EVENT_ID: HeaderName = HeaderName::from_static("last-event-id");

/// One piece of a session's state the stream diffs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionChange {
    /// The persisted runtime summary.
    Runtime,
    /// Background jobs and subagent runs.
    Background,
    /// The `artifacts/` directory listing.
    Artifacts,
    /// Issues the session's tasks link.
    Issues,
    /// New `ci_findings` rows.
    Warning,
    /// The session's PRs, their states and live checks.
    Prs,
}

impl SessionChange {
    const ALL: [Self; 6] = [
        Self::Runtime,
        Self::Background,
        Self::Artifacts,
        Self::Issues,
        Self::Warning,
        Self::Prs,
    ];

    const fn bit(self) -> u8 {
        1 << self as u8
    }
}

/// A `publish_session_change` notice on the session-change channel.
#[derive(Debug)]
pub struct SessionChangeNotice {
    pub(crate) session_id: String,
    pub(crate) kind: SessionChange,
}

pub type SessionChangeSender = async_channel::Sender<SessionChangeNotice>;
pub type SessionChangeReceiver = async_channel::Receiver<SessionChangeNotice>;

/// Queue `kind` for the session's diff publisher; a closed dispatcher
/// drops it the way a dead bus drops a notification.
pub fn notify_change(tx: &SessionChangeSender, session_id: &str, kind: SessionChange) {
    let _ = tx.try_send(SessionChangeNotice {
        session_id: session_id.to_string(),
        kind,
    });
}

/// The cursor of `GET /api/sessions/{id}` and every SSE `id` on this
/// stream: `{epoch}.{index}`.
pub fn format_cursor(epoch: u64, index: u64) -> String {
    format!("{epoch}.{index}")
}

/// The frame index `raw` continues from, when it is a well-formed cursor
/// of this daemon's epoch.
pub(super) fn parse_cursor(raw: &str, epoch: u64) -> Option<u64> {
    let (cursor_epoch, index) = raw.split_once('.')?;
    if cursor_epoch.parse::<u64>().ok()? != epoch {
        return None;
    }
    index.parse().ok()
}

#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct SessionEventsQuery {
    #[serde(default)]
    after: Option<String>,
    /// The subscriber is the one-shot client the session's turns belong to:
    /// its connection ending stops the running turn.
    #[serde(default)]
    owns_turn: bool,
}

/// How often an owning stream beats while the session is quiet.
const OWNER_HEARTBEAT: Duration = Duration::from_secs(1);

/// `GET /api/sessions/{id}/events`.
pub(super) async fn stream_session_events(
    params: Params,
    Query(query): Query<SessionEventsQuery>,
    headers: HeaderMap,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Sse> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    state.ensure_session_publisher();
    // `after` wins over `Last-Event-ID` when both arrive.
    let header_after = headers
        .get(LAST_EVENT_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let after = query.after.or(header_after);
    let (subscription, reset) = after
        .as_deref()
        .and_then(|raw| parse_cursor(raw, state.session_events_epoch))
        .map_or_else(
            || (state.turn_hub.subscribe_live(&session_id), after.is_some()),
            |index| {
                let subscription = state.turn_hub.subscribe(&session_id, index);
                if subscription.gap {
                    drop(subscription);
                    (state.turn_hub.subscribe_live(&session_id), true)
                } else {
                    (subscription, false)
                }
            },
        );

    let (sender, sse) = Sse::channel();
    let epoch = state.session_events_epoch;
    let owns_turn = query.owns_turn;
    let task_state = state.clone();
    state.tokio_handle.spawn(crate::panic_boundary::round(
        "session event stream",
        async move {
        let client_gone = async {
            if reset {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Reset<'a> {
                    session_id: &'a str,
                }
                if !send_event(
                    &sender,
                    "reset",
                    &Reset {
                        session_id: &session_id,
                    },
                )
                .await
                {
                    return true;
                }
            }
            // A send fails once the client's connection is closed; an owner's
            // stream beats while quiet so the close shows without a frame.
            let mut heartbeat = async_io::Timer::interval(OWNER_HEARTBEAT);
            loop {
                let frames = tokio::select! {
                    frames = subscription.next() => match frames {
                        Some(frames) => frames,
                        None => return false,
                    },
                    _ = heartbeat.next(), if owns_turn => {
                        if sender
                            .send(skyzen::responder::sse::Event::comment(""))
                            .await
                            .is_err()
                        {
                            return true;
                        }
                        continue;
                    }
                };
                for frame in frames {
                    let sent = sender
                        .send(
                            skyzen::responder::sse::Event::data(&*frame.data)
                                .event(frame.kind)
                                .id(format_cursor(epoch, frame.index)),
                        )
                        .await
                        .is_ok();
                    if !sent {
                        return true;
                    }
                }
            }
        }
        .await;
        // Only a turn still running belongs to the client; a one-shot run
        // that exits after its turn ended leaves the session as it is.
        if owns_turn && client_gone && task_state.runtime_service.runs().is_held(&session_id) {
            tracing::info!(%session_id, "the client owning the turn disconnected; stopping the session");
            if let Err(error) =
                super::chat::stop_session(&task_state, &session_id, CancelReason::ClientDisconnected)
                    .await
            {
                tracing::error!(%session_id, %error, "failed to stop the session of a disconnected client");
            }
        }
        },
    ));
    Ok(sse)
}

/// Spawns the session-change dispatcher: one task draining the channel,
/// recomputing each flagged piece once per burst and publishing only real
/// diffs into the session's [`TurnHub`] channel.
pub fn spawn(state: Arc<DaemonState>, rx: SessionChangeReceiver) {
    let handle = state.tokio_handle.clone();
    handle.spawn(run(state, rx));
}

#[derive(Default)]
struct PendingChanges(u8);

impl PendingChanges {
    const fn insert(&mut self, kind: SessionChange) {
        self.0 |= kind.bit();
    }

    fn take(&mut self) -> impl Iterator<Item = SessionChange> {
        let bits = std::mem::take(&mut self.0);
        SessionChange::ALL
            .into_iter()
            .filter(move |kind| bits & kind.bit() != 0)
    }
}

/// What the session's stream already published, per piece.
#[derive(Default)]
struct Published {
    /// JSON of the last `runtime.changed` payload's `runtime` value.
    runtime: Option<String>,
    background: SessionBackgroundSnapshot,
    artifacts: Vec<ArtifactEntry>,
    issues: Vec<IssueBrief>,
    /// Highest `ci_findings.id` sent as `warning`.
    warning_id: i64,
    /// JSON of the last `prs.changed` payload's `prs` value.
    prs: Option<String>,
}

async fn run(state: Arc<DaemonState>, rx: SessionChangeReceiver) {
    let mut pending: HashMap<String, PendingChanges> = HashMap::new();
    let mut published: HashMap<String, Published> = HashMap::new();
    while let Ok(notice) = rx.recv().await {
        pending
            .entry(notice.session_id)
            .or_default()
            .insert(notice.kind);
        async_io::Timer::after(COALESCE).await;
        while let Ok(notice) = rx.try_recv() {
            pending
                .entry(notice.session_id)
                .or_default()
                .insert(notice.kind);
        }
        Box::pin(crate::panic_boundary::round(
            "session event dispatcher",
            async {
                for (session_id, mut kinds) in pending.drain() {
                    for kind in kinds.take() {
                        publish_piece(
                            &state,
                            &session_id,
                            kind,
                            published.entry(session_id.clone()).or_default(),
                        )
                        .await;
                    }
                }
            },
        ))
        .await;
    }
}

/// Recomputes `kind` for `session_id`, diffs it against `published` and
/// emits one frame per real change. Read failures keep the last published
/// value and try again on the next notice.
#[allow(clippy::items_after_statements)]
async fn publish_piece(
    state: &Arc<DaemonState>,
    session_id: &str,
    kind: SessionChange,
    published: &mut Published,
) {
    match kind {
        SessionChange::Runtime => {
            let runtime = match state.runtime_service.get(session_id).await {
                Ok(runtime) => runtime,
                Err(error) => {
                    tracing::warn!(session_id, %error, "session runtime reload failed");
                    return;
                }
            };
            let runtime = SessionRuntimeResponse::from(runtime);
            let json = serde_json::to_string(&runtime).expect("runtime serializes");
            if published.runtime.as_deref() == Some(json.as_str()) {
                return;
            }
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct RuntimeChanged<'a> {
                session_id: &'a str,
                runtime: &'a SessionRuntimeResponse,
            }
            publish(
                state,
                session_id,
                "runtime.changed",
                &RuntimeChanged {
                    session_id,
                    runtime: &runtime,
                },
            );
            published.runtime = Some(json);
        }
        SessionChange::Background => {
            let snapshot = state.session_background.snapshot(session_id).await;
            let (job_upserts, job_removed) =
                diff(&published.background.jobs, &snapshot.jobs, |job| &job.pid);
            for pid in job_removed {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct JobRemoved<'a> {
                    session_id: &'a str,
                    pid: u32,
                }
                publish(
                    state,
                    session_id,
                    "job.removed",
                    &JobRemoved {
                        session_id,
                        pid: *pid,
                    },
                );
            }
            for job in job_upserts {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct JobUpserted<'a> {
                    session_id: &'a str,
                    job: &'a crate::client::background::WireJob,
                }
                publish(
                    state,
                    session_id,
                    "job.upserted",
                    &JobUpserted { session_id, job },
                );
            }
            let (sub_upserts, sub_removed) = diff(
                &published.background.subagents,
                &snapshot.subagents,
                |run| &run.run_id,
            );
            for run_id in sub_removed {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct SubagentRemoved<'a> {
                    session_id: &'a str,
                    run_id: u64,
                }
                publish(
                    state,
                    session_id,
                    "subagent.removed",
                    &SubagentRemoved {
                        session_id,
                        run_id: *run_id,
                    },
                );
            }
            for run in sub_upserts {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct SubagentUpserted<'a> {
                    session_id: &'a str,
                    subagent: &'a crate::client::background::WireSubagentRun,
                }
                publish(
                    state,
                    session_id,
                    "subagent.upserted",
                    &SubagentUpserted {
                        session_id,
                        subagent: run,
                    },
                );
            }
            published.background = snapshot;
        }
        SessionChange::Artifacts => {
            let artifacts =
                match super::artifacts::collect_session_artifacts(state, session_id).await {
                    Ok(artifacts) => artifacts,
                    Err(error) => {
                        tracing::debug!(session_id, %error, "artifact collection failed");
                        return;
                    }
                };
            let (upserts, removed) = diff(&published.artifacts, &artifacts, |a| a.path.as_str());
            for path in removed {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct ArtifactRemoved<'a> {
                    session_id: &'a str,
                    path: &'a str,
                }
                publish(
                    state,
                    session_id,
                    "artifact.removed",
                    &ArtifactRemoved { session_id, path },
                );
            }
            for artifact in upserts {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct ArtifactUpserted<'a> {
                    session_id: &'a str,
                    artifact: &'a ArtifactEntry,
                }
                publish(
                    state,
                    session_id,
                    "artifact.upserted",
                    &ArtifactUpserted {
                        session_id,
                        artifact,
                    },
                );
            }
            published.artifacts = artifacts;
        }
        SessionChange::Issues => {
            let Some(issues) = load_session_issues(state, session_id).await else {
                return;
            };
            let (upserts, removed) = diff(&published.issues, &issues, |issue| &issue.number);
            for key in removed {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct IssueRemoved<'a> {
                    session_id: &'a str,
                    key: i64,
                }
                publish(
                    state,
                    session_id,
                    "issue.removed",
                    &IssueRemoved {
                        session_id,
                        key: *key,
                    },
                );
            }
            for issue in upserts {
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct IssueUpserted<'a> {
                    session_id: &'a str,
                    issue: &'a IssueBrief,
                }
                publish(
                    state,
                    session_id,
                    "issue.upserted",
                    &IssueUpserted { session_id, issue },
                );
            }
            published.issues = issues;
        }
        SessionChange::Prs => {
            let prs = match super::sessions::session_prs(state, session_id).await {
                Ok(prs) => prs,
                Err(error) => {
                    tracing::warn!(session_id, %error, "failed to load the session's PRs");
                    return;
                }
            };
            let json = serde_json::to_string(&prs).expect("PRs serialize");
            if published.prs.as_deref() == Some(json.as_str()) {
                return;
            }
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct PrsChanged<'a> {
                session_id: &'a str,
                prs: &'a [super::types::ApiPr],
            }
            publish(
                state,
                session_id,
                "prs.changed",
                &PrsChanged {
                    session_id,
                    prs: &prs,
                },
            );
            published.prs = Some(json);
        }
        SessionChange::Warning => {
            let epoch_ms =
                i64::try_from(state.session_events_epoch / 1_000_000).unwrap_or(i64::MAX);
            let findings = match state
                .db()
                .ci_findings_after(session_id, published.warning_id)
                .await
            {
                Ok(findings) => findings,
                Err(error) => {
                    tracing::error!(session_id, %error, "failed to load the session's warnings");
                    return;
                }
            };
            for finding in findings {
                published.warning_id = finding.id;
                // Rows older than this daemon were live before any client
                // could read the session's cursor for them.
                if finding.created_at_ms < epoch_ms {
                    continue;
                }
                #[derive(Serialize)]
                #[serde(rename_all = "camelCase")]
                struct Warning<'a> {
                    session_id: &'a str,
                    message: &'a str,
                }
                publish(
                    state,
                    session_id,
                    "warning",
                    &Warning {
                        session_id,
                        message: &finding.warning,
                    },
                );
            }
        }
    }
}

/// `GET /api/sessions/{id}/issues`: the linked-issue list the
/// `session_issues` snapshot and `issue.*` deltas track, for the stream's
/// initial read.
pub(super) async fn get_session_issues(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<crate::client::SessionIssuesFrame>> {
    let session_id = params.get("id").map_err(bad_request_error)?.to_string();
    if session_id.is_empty() {
        return Err(bad_request_error("session id is required"));
    }
    let issues = load_session_issues(&state, &session_id)
        .await
        .ok_or_else(|| {
            super::error::internal_error_message("no session paths for linked issues")
        })?;
    Ok(Json(crate::client::SessionIssuesFrame {
        session_id,
        issues,
    }))
}

async fn load_session_issues(state: &DaemonState, session_id: &str) -> Option<Vec<IssueBrief>> {
    let paths = match super::helpers::load_session_paths(state, session_id).await {
        Ok(paths) => paths,
        Err(error) => {
            tracing::debug!(session_id, %error, "no session paths for linked issues");
            return None;
        }
    };
    match super::sessions::linked_issue_briefs(state, session_id, &paths).await {
        Ok(issues) => Some(issues),
        Err(error) => {
            tracing::warn!(session_id, %error, "failed to look up linked issues");
            None
        }
    }
}

fn publish<T: Serialize + Sync>(
    state: &DaemonState,
    session_id: &str,
    kind: &'static str,
    data: &T,
) {
    match serde_json::to_string(data) {
        Ok(json) => state.turn_hub.publish(session_id, kind, json.into()),
        Err(error) => tracing::error!(session_id, kind, %error, "failed to encode a session frame"),
    }
}

/// Items of `current` absent or changed under `key` (the upserts), then
/// keys of `previous` absent from `current` (the removals).
pub(super) fn diff<'a, 'b, K: ?Sized + Eq, T: PartialEq>(
    previous: &'b [T],
    current: &'a [T],
    key: impl for<'x> Fn(&'x T) -> &'x K,
) -> (Vec<&'a T>, Vec<&'b K>) {
    let upserts = current
        .iter()
        .filter(|item| {
            !previous
                .iter()
                .any(|old| key(old) == key(item) && old == *item)
        })
        .collect();
    let removals = previous
        .iter()
        .map(&key)
        .filter(|old_key| !current.iter().any(|item| key(item) == *old_key))
        .collect();
    (upserts, removals)
}
