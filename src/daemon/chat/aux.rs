//! Sidecar model calls against a session's conversation.
//!
//! An aux run replays the session's persisted history through its own model
//! on a throwaway request: it never touches the transcript, the parked
//! agent, or the session's tools. Callers (prompt suggestion, recap) own
//! eligibility and message construction; [`AuxTasks`] owns identity and
//! cancellation so a superseded or stale result can never be applied —
//! a result is published only through [`AuxTasks::commit`], which checks
//! ownership and publishes under one lock, and [`AuxTasks::cancel`]
//! publishes its own frame under that same lock, so a cancelled task's
//! result can never follow the cancellation.
//!
//! Cancellation is cooperative: replacing or cancelling a session's slot
//! aborts the task, and every cancellation path the issue describes (new
//! user message, turn start, session close, feature off) funnels through
//! [`AuxTasks::cancel`] or a superseding [`AuxTasks::start`].

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::de::DeserializeOwned;

use crate::daemon::state::DaemonState;

/// In-flight aux tasks, at most one per session.
#[derive(Default)]
pub struct AuxTasks {
    tasks: Mutex<HashMap<String, AuxTask>>,
    next_id: AtomicU64,
}

/// What an aux task generates, and so which setting governs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuxKind {
    /// A prompt suggestion, governed by `[suggestions] enabled`.
    Suggestion,
    /// A recap the idle timer started, governed by `[recap] auto`.
    AutoRecap,
    /// A recap the user asked for.
    ManualRecap,
}

struct AuxTask {
    request_id: u64,
    kind: AuxKind,
    handle: tokio::task::JoinHandle<()>,
}

impl std::fmt::Debug for AuxTasks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuxTasks").finish_non_exhaustive()
    }
}

impl AuxTasks {
    fn tasks(&self) -> std::sync::MutexGuard<'_, HashMap<String, AuxTask>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Claims `session_id`'s aux slot for the task `make_task` builds with
    /// the issued request id, aborting whatever it replaces.
    pub fn start<F>(
        &self,
        session_id: &str,
        kind: AuxKind,
        make_task: impl FnOnce(u64) -> F,
        rt: &tokio::runtime::Handle,
    ) -> u64
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        // The slot is claimed under the lock the task's commit takes, so a
        // task that finishes at once still finds itself the owner.
        let mut tasks = self.tasks();
        let handle = rt.spawn(make_task(request_id));
        let previous = tasks.insert(
            session_id.to_string(),
            AuxTask {
                request_id,
                kind,
                handle,
            },
        );
        drop(tasks);
        if let Some(previous) = previous {
            previous.handle.abort();
        }
        request_id
    }

    /// Aborts and forgets `session_id`'s in-flight aux task, if any, then
    /// runs `publish` (the cancellation's own frame), all under the lock
    /// [`Self::commit`] takes: a result either published before this call or
    /// is rejected after it.
    pub fn cancel(&self, session_id: &str, publish: impl FnOnce()) {
        let mut tasks = self.tasks();
        if let Some(task) = tasks.remove(session_id) {
            task.handle.abort();
        }
        publish();
        drop(tasks);
    }

    /// Aborts every in-flight task of `kind` and runs `publish` with each
    /// one's session, under the lock [`Self::commit`] takes: the setting
    /// governing `kind` went off, and no such result may follow.
    pub fn cancel_kind(&self, kind: AuxKind, mut publish: impl FnMut(&str)) {
        let mut tasks = self.tasks();
        let sessions: Vec<String> = tasks
            .iter()
            .filter(|(_, task)| task.kind == kind)
            .map(|(session_id, _)| session_id.clone())
            .collect();
        for session_id in sessions {
            if let Some(task) = tasks.remove(&session_id) {
                task.handle.abort();
            }
            publish(&session_id);
        }
        drop(tasks);
    }

    /// Publishes the result of `request_id` through `publish` and releases
    /// its slot, while `request_id` still owns `session_id`'s slot; the
    /// check and the publication are one step under the lock. Returns
    /// whether the task owned the slot.
    pub fn commit(&self, session_id: &str, request_id: u64, publish: impl FnOnce()) -> bool {
        let mut tasks = self.tasks();
        let owns = tasks
            .get(session_id)
            .is_some_and(|task| task.request_id == request_id);
        if owns {
            publish();
            tasks.remove(session_id);
        }
        owns
    }

    /// Whether `request_id` currently owns `session_id`'s aux slot.
    #[cfg(test)]
    pub fn is_current(&self, session_id: &str, request_id: u64) -> bool {
        self.tasks()
            .get(session_id)
            .is_some_and(|task| task.request_id == request_id)
    }
}

/// Why an aux run failed.
#[derive(Debug, thiserror::Error)]
pub enum AuxError {
    /// The session or its persisted history could not be prepared.
    #[error("session preparation failed: {0}")]
    Session(String),
    /// The session's model is unavailable.
    #[error("the agent has no model to call")]
    NoModel,
    /// The provider call failed.
    #[error("model call failed: {0}")]
    Model(String),
    /// The run exceeded its timeout.
    #[error("aux run timed out")]
    Timeout,
}

/// The warm prompt-cache entry of `session_id`'s idle agent. `None` when the
/// session has no idle agent or its cache is cold: an automatic aux run
/// would pay the full prefix price, so it does not start.
pub fn warm_cache(state: &DaemonState, session_id: &str) -> Option<aither_agent::CacheWarmth> {
    state
        .parked_agents
        .cache_warmth(session_id)
        .filter(aither_agent::CacheWarmth::is_warm)
}

/// The messages the session's idle agent would send next: its system
/// prefix, compaction handoff and kept turns. Read without changing the
/// agent, so the request shares the main agent's cached prefix and carries
/// nothing a compaction replaced.
pub async fn context_messages(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<Vec<aither_core::llm::Message>, AuxError> {
    let worker = super::hosts::idle_agent(state, session_id)
        .await
        .map_err(|error| AuxError::Session(error.to_string()))?;
    let handle = worker.context_messages();
    while let Some(event) = handle.recv().await {
        if let crate::agent::TuiEvent::ContextMessages(result) = event {
            return result.map_err(|error| AuxError::Session(error.to_string()));
        }
    }
    Err(AuxError::Session(
        "the agent stopped before it gave its context".to_string(),
    ))
}

/// The languages `session_id`'s agent was configured with.
pub async fn languages(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<aither_agent::Languages, AuxError> {
    let worker = super::hosts::idle_agent(state, session_id)
        .await
        .map_err(|error| AuxError::Session(error.to_string()))?;
    worker.languages().ok_or(AuxError::NoModel)
}

/// Streams `messages` through the session's own model via its parked agent
/// and collects the text. The call is a side request like `/btw`: nothing
/// enters the conversation. Fails as [`AuxError::Timeout`] after `timeout`.
pub async fn run(
    state: &Arc<DaemonState>,
    session_id: &str,
    messages: Vec<aither_core::llm::Message>,
    timeout: Duration,
) -> Result<String, AuxError> {
    tokio::time::timeout(timeout, Box::pin(respond(state, session_id, messages)))
        .await
        .unwrap_or(Err(AuxError::Timeout))
}

async fn respond(
    state: &Arc<DaemonState>,
    session_id: &str,
    messages: Vec<aither_core::llm::Message>,
) -> Result<String, AuxError> {
    use aither_core::llm::Event;
    use futures_lite::StreamExt as _;

    let worker = super::hosts::idle_agent(state, session_id)
        .await
        .map_err(|error| AuxError::Session(error.to_string()))?;
    let llm = worker.btw_llm().cloned().ok_or(AuxError::NoModel)?;
    let mut stream = std::pin::pin!(llm.respond(messages));
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            Ok(Event::Text(chunk)) => text.push_str(&chunk),
            Ok(_) => {}
            Err(error) => return Err(AuxError::Model(error.to_string())),
        }
    }
    Ok(text)
}

/// Parses an aux response body: trimmed JSON of at most `max_chars`,
/// nothing else. `T` should derive `serde::Deserialize` with
/// `deny_unknown_fields`. Malformed output is `None` — the caller treats it
/// as a generation failure, never a fallback.
pub fn parse_strict<T: DeserializeOwned>(text: &str, max_chars: usize) -> Option<T> {
    let text = text.trim();
    if text.is_empty()
        || text.chars().count() > max_chars
        || text.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\n' | '\t')) || matches!(ch, '\u{2028}' | '\u{2029}')
        })
    {
        return None;
    }
    serde_json::from_str(text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Answer {
        suggestion: Option<String>,
    }

    #[test]
    fn parse_strict_accepts_bounded_json() {
        let answer = parse_strict::<Answer>(r#"{"suggestion":"run the tests"}"#, 240);
        assert_eq!(
            answer,
            Some(Answer {
                suggestion: Some("run the tests".to_string())
            })
        );
        let null = parse_strict::<Answer>(r#"{"suggestion":null}"#, 240);
        assert_eq!(null, Some(Answer { suggestion: None }));
    }

    #[test]
    fn parse_strict_rejects_malformed_output() {
        for text in [
            r#"{"suggestion":"x","extra":1}"#, // unknown field
            r#"{"suggestion":"x"} trailing"#,  // trailing garbage
            "not json",
            "",
            "   ",
            "{\"suggestion\":\"x\u{2028}\"}", // control char
        ] {
            assert_eq!(parse_strict::<Answer>(text, 240), None, "input: {text:?}");
        }
        assert_eq!(parse_strict::<Answer>(r#"{"suggestion":"x"}"#, 3), None);
        // Newlines and tabs survive: structured answers may legitimately wrap.
        assert!(parse_strict::<Answer>("{\n  \"suggestion\": null\n}", 240).is_some());
    }

    #[tokio::test]
    async fn start_supersedes_and_aborts_the_previous_task() {
        let rt = tokio::runtime::Handle::current();
        let registry = AuxTasks::default();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let first = registry.start(
            "s",
            AuxKind::Suggestion,
            |_| async move {
                // Abort drops the future and its captured sender.
                let _dropped_tx = dropped_tx;
                std::future::pending::<()>().await;
            },
            &rt,
        );
        assert!(registry.is_current("s", first));
        let second = registry.start("s", AuxKind::Suggestion, |_| std::future::pending(), &rt);
        assert!(registry.is_current("s", second));
        assert!(!registry.is_current("s", first));
        dropped_rx
            .await
            .expect_err("the first task was not aborted");
        registry.cancel("s", || {});
        assert!(!registry.is_current("s", second));
    }

    #[tokio::test]
    async fn commit_releases_only_the_owning_request() {
        let rt = tokio::runtime::Handle::current();
        let registry = AuxTasks::default();
        let request_id = registry.start("s", AuxKind::Suggestion, |_| std::future::pending(), &rt);
        assert!(!registry.commit("s", request_id + 1, || panic!("not the owner")));
        assert!(registry.is_current("s", request_id));
        assert!(registry.commit("s", request_id, || {}));
        assert!(!registry.is_current("s", request_id));
        assert_ne!(
            registry.start("s", AuxKind::Suggestion, |_| std::future::pending(), &rt),
            request_id
        );
    }

    fn logged(log: &Mutex<Vec<&'static str>>, entry: &'static str) {
        log.lock().expect("log").push(entry);
    }

    #[tokio::test]
    async fn a_result_never_follows_the_cancellation_it_raced() {
        let rt = tokio::runtime::Handle::current();
        // Interleave the two on real threads many times: whichever wins,
        // the cancellation's frame is last, and a rejected commit
        // publishes nothing.
        for _ in 0..500 {
            let registry = Arc::new(AuxTasks::default());
            let log = Arc::new(Mutex::new(Vec::new()));
            let request_id =
                registry.start("s", AuxKind::Suggestion, |_| std::future::pending(), &rt);
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let committing = {
                let (registry, log, barrier) = (registry.clone(), log.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    registry.commit("s", request_id, || logged(&log, "ready"))
                })
            };
            barrier.wait();
            registry.cancel("s", || logged(&log, "cleared"));
            let committed = committing.join().expect("commit thread");
            let log = log.lock().expect("log");
            assert_eq!(log.last(), Some(&"cleared"), "{log:?}");
            assert_eq!(log.len(), if committed { 2 } else { 1 }, "{log:?}");
            drop(log);
        }
    }

    #[tokio::test]
    async fn a_commit_after_a_cancel_is_rejected() {
        let rt = tokio::runtime::Handle::current();
        let registry = AuxTasks::default();
        let log = Mutex::new(Vec::new());
        let request_id = registry.start("s", AuxKind::Suggestion, |_| std::future::pending(), &rt);
        registry.cancel("s", || logged(&log, "cleared"));
        assert!(!registry.commit("s", request_id, || logged(&log, "ready")));
        assert_eq!(*log.lock().expect("log"), ["cleared"]);
    }
}
