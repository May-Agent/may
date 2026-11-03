//! Sidecar model calls against a session's conversation.
//!
//! An aux run replays the session's persisted history through its own model
//! on a throwaway request: it never touches the transcript, the parked
//! agent, or the session's tools. Callers (prompt suggestion, recap) own
//! eligibility and message construction; [`AuxTasks`] owns identity and
//! cancellation so a superseded or stale result can never be applied —
//! a result may be applied only while [`AuxTasks::is_current`] still holds
//! for its request id.
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

struct AuxTask {
    request_id: u64,
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
        make_task: impl FnOnce(u64) -> F,
        rt: &tokio::runtime::Handle,
    ) -> u64
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        let request_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let handle = rt.spawn(make_task(request_id));
        let previous = self
            .tasks()
            .insert(session_id.to_string(), AuxTask { request_id, handle });
        if let Some(previous) = previous {
            previous.handle.abort();
        }
        request_id
    }

    /// Aborts and forgets `session_id`'s in-flight aux task, if any.
    pub fn cancel(&self, session_id: &str) {
        let task = self.tasks().remove(session_id);
        if let Some(task) = task {
            task.handle.abort();
        }
    }

    /// Forgets `session_id`'s task while `request_id` still owns it. A
    /// finishing task releases its slot through this without being aborted.
    pub fn clear(&self, session_id: &str, request_id: u64) {
        if let std::collections::hash_map::Entry::Occupied(entry) =
            self.tasks().entry(session_id.to_string())
            && entry.get().request_id == request_id
        {
            entry.remove();
        }
    }

    /// Whether `request_id` still owns `session_id`'s aux slot. A result is
    /// applied only while this holds.
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

/// The session's persisted conversation as replay messages — the same
/// prefix an agent rebuilt from history sees, so provider prefix caches
/// still hit.
pub async fn history_messages(
    state: &Arc<DaemonState>,
    session_id: &str,
) -> Result<Vec<aither_core::llm::Message>, AuxError> {
    let history = crate::session::load_history_from_db(&state.db(), session_id)
        .await
        .map_err(|error| AuxError::Session(error.to_string()))?;
    history
        .to_aither_messages()
        .map_err(|error| AuxError::Session(error.to_string()))
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
    use aither_core::LanguageModel as _;
    use aither_core::llm::{Event, LLMRequest};
    use futures_lite::StreamExt as _;

    let worker = super::hosts::idle_agent(state, session_id)
        .await
        .map_err(|error| AuxError::Session(error.to_string()))?;
    let llm = worker.btw_llm().cloned().ok_or(AuxError::NoModel)?;
    let mut stream = std::pin::pin!(llm.respond(LLMRequest::new(messages)));
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
            |_| async move {
                // Abort drops the future and its captured sender.
                let _dropped_tx = dropped_tx;
                std::future::pending::<()>().await;
            },
            &rt,
        );
        assert!(registry.is_current("s", first));
        let second = registry.start("s", |_| std::future::pending(), &rt);
        assert!(registry.is_current("s", second));
        assert!(!registry.is_current("s", first));
        dropped_rx
            .await
            .expect_err("the first task was not aborted");
        registry.cancel("s");
        assert!(!registry.is_current("s", second));
    }

    #[tokio::test]
    async fn clear_releases_only_the_owning_request() {
        let rt = tokio::runtime::Handle::current();
        let registry = AuxTasks::default();
        let request_id = registry.start("s", |_| std::future::pending(), &rt);
        registry.clear("s", request_id + 1);
        assert!(registry.is_current("s", request_id));
        registry.clear("s", request_id);
        assert!(!registry.is_current("s", request_id));
        assert_ne!(
            registry.start("s", |_| std::future::pending(), &rt),
            request_id
        );
    }
}
