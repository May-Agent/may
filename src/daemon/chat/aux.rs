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

use schemars::JsonSchema;
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
    /// The model answered, but not with an acceptable output.
    #[error("model output rejected: {0}")]
    Output(String),
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

/// Generates a `T` from `messages` through the session's own model via its
/// parked agent. The call is a side request like `/btw`: nothing enters the
/// conversation. Fails as [`AuxError::Timeout`] after `timeout`.
pub async fn generate<T: AuxOutput>(
    state: &Arc<DaemonState>,
    session_id: &str,
    messages: Vec<aither_core::llm::Message>,
    timeout: Duration,
) -> Result<T, AuxError> {
    let call = async {
        let worker = super::hosts::idle_agent(state, session_id)
            .await
            .map_err(|error| AuxError::Session(error.to_string()))?;
        let slot = worker.side_model().await.map_err(AuxError::Session)?;
        generate_unbounded(&slot, messages).await
    };
    tokio::time::timeout(timeout, Box::pin(call))
        .await
        .unwrap_or(Err(AuxError::Timeout))
}

/// [`generate`] through an explicit model slot, for tests with a mock model.
#[cfg(test)]
pub async fn generate_with<M: aither_core::LanguageModel, T: AuxOutput>(
    slot: &crate::agent::ModelSlot<M>,
    messages: Vec<aither_core::llm::Message>,
    timeout: Duration,
) -> Result<T, AuxError> {
    tokio::time::timeout(timeout, Box::pin(generate_unbounded(slot, messages)))
        .await
        .unwrap_or(Err(AuxError::Timeout))
}

async fn generate_unbounded<M: aither_core::LanguageModel, T: AuxOutput>(
    slot: &crate::agent::ModelSlot<M>,
    messages: Vec<aither_core::llm::Message>,
) -> Result<T, AuxError> {
    let output = slot
        .generate::<T>(messages)
        .await
        .map_err(|error| match error {
            aither_core::llm::GenerateError::Provider(error) => AuxError::Model(error.to_string()),
            error @ aither_core::llm::GenerateError::Parse { .. } => {
                AuxError::Output(error.to_string())
            }
        })?;
    check(&output)?;
    Ok(output)
}

/// An aux output type: the JSON schema the model fills in, and the limits
/// of its text fields.
pub trait AuxOutput: JsonSchema + DeserializeOwned + 'static {
    /// Every text field with its limit in Unicode scalar values.
    fn texts(&self) -> Vec<(&str, usize)>;
}

/// Rejects an output with a text over its limit or holding a control
/// character other than newline and tab, or a line or paragraph separator.
/// Nothing is truncated: such an output is a failed generation.
fn check<T: AuxOutput>(output: &T) -> Result<(), AuxError> {
    for (text, limit) in output.texts() {
        if text.chars().count() > limit {
            return Err(AuxError::Output(format!(
                "a field exceeds {limit} characters"
            )));
        }
        if text.chars().any(|ch| {
            (ch.is_control() && !matches!(ch, '\n' | '\t')) || matches!(ch, '\u{2028}' | '\u{2029}')
        }) {
            return Err(AuxError::Output(
                "a field holds a control character".to_string(),
            ));
        }
    }
    Ok(())
}

/// A scripted model for tests: replies with its text as one event.
#[cfg(test)]
pub(super) mod test_support {
    use aither_core::LanguageModel;
    use aither_core::llm::{Event, LLMRequest, model::Profile as ModelProfile};
    use futures_lite::Stream;

    pub struct Replying(pub &'static str);

    #[derive(Debug)]
    pub struct Never;

    impl std::fmt::Display for Never {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("mock error")
        }
    }

    impl std::error::Error for Never {}

    impl aither_core::llm::Classify for Never {
        fn failure(&self) -> aither_core::llm::Failure {
            aither_core::llm::Failure::permanent()
        }
    }

    impl LanguageModel for Replying {
        type Error = Never;

        fn respond(
            &self,
            _request: LLMRequest,
        ) -> impl Stream<Item = Result<Event, Self::Error>> + Send {
            futures_lite::stream::iter([Ok(Event::Text(self.0.to_string()))])
        }

        fn profile(&self) -> impl std::future::Future<Output = Result<ModelProfile, Never>> + Send {
            std::future::ready(Ok(ModelProfile::new(
                "mock", "test", "mock", "mock", 100_000,
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
