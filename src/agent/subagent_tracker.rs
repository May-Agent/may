//! Live tracking of `subagent` runs.
//!
//! The `subagent` command runs inside the session's IPC command registry.
//! aither's [`SubagentRuns`] lists and cancels the runs in flight; what it
//! does not know is how many tokens each has used, so the command credits the
//! usage of every request its child makes to the run's entry here. The jobs
//! panel and the status bar read the two together.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use aither_agent::SubagentRuns;
use dashmap::DashMap;

use crate::command_reading::{Card, SubagentState};

/// Point-in-time view of one tracked subagent run, for rendering.
#[derive(Debug, Clone)]
pub struct SubagentRunSnapshot {
    /// The run's id in the session's [`SubagentRuns`].
    pub run_id: u64,
    /// Subagent type name the call invoked.
    pub agent: String,
    /// Full prompt handed to the subagent.
    pub prompt: String,
    /// When the run started.
    pub started_at: Instant,
    /// Prompt tokens consumed so far.
    pub prompt_tokens: u64,
    /// Completion tokens produced so far.
    pub completion_tokens: u64,
    /// LLM responses the run has observed so far.
    pub requests: u64,
}

impl SubagentRunSnapshot {
    /// Seconds elapsed since the run started.
    #[must_use]
    pub fn elapsed_secs(&self) -> u64 {
        self.started_at.elapsed().as_secs()
    }

    /// Total tokens observed so far.
    #[must_use]
    pub const fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

/// The counters of one run.
#[derive(Debug, Default)]
struct Counters {
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
    requests: AtomicU64,
}

/// Where a run's token usage is credited.
#[derive(Debug, Clone, Default)]
pub struct RunUsage {
    counters: Arc<Counters>,
}

impl RunUsage {
    /// Credits one request's `usage` to the run.
    pub fn record(&self, usage: &aither_core::llm::Usage) {
        self.counters.requests.fetch_add(1, Ordering::Relaxed);
        if let Some(tokens) = usage.prompt_tokens {
            self.counters
                .prompt_tokens
                .fetch_add(u64::from(tokens), Ordering::Relaxed);
        }
        if let Some(tokens) = usage.completion_tokens {
            self.counters
                .completion_tokens
                .fetch_add(u64::from(tokens), Ordering::Relaxed);
        }
    }
}

/// Token usage of the session's runs in flight, and where the commands that
/// render as cards report them.
#[derive(Debug, Default, Clone)]
pub struct SubagentTracker {
    runs: SubagentRuns,
    usage: Arc<DashMap<u64, RunUsage>>,
    cards: super::command_cards::CommandCards,
}

impl SubagentTracker {
    /// The runs in flight, listed and cancelled by id.
    #[must_use]
    pub const fn runs(&self) -> &SubagentRuns {
        &self.runs
    }

    /// The session's command cards: `subagent` reports its runs there, and
    /// so do the other commands that render as cards.
    #[must_use]
    pub const fn cards(&self) -> &super::command_cards::CommandCards {
        &self.cards
    }

    /// Starts crediting run `id`; the entry goes when the guard drops.
    #[must_use]
    pub fn track(&self, id: u64) -> (RunUsage, TrackedRun) {
        let usage = RunUsage::default();
        self.usage.insert(id, usage.clone());
        (
            usage,
            TrackedRun {
                usage: Arc::clone(&self.usage),
                id,
            },
        )
    }

    /// Snapshots of the runs in flight, oldest first.
    #[must_use]
    pub fn running(&self) -> Vec<SubagentRunSnapshot> {
        self.runs
            .list()
            .into_iter()
            .map(|info| {
                let counters = self
                    .usage
                    .get(&info.id)
                    .map(|usage| Arc::clone(&usage.counters))
                    .unwrap_or_default();
                SubagentRunSnapshot {
                    run_id: info.id,
                    agent: info.name,
                    prompt: info.prompt,
                    started_at: info.started,
                    prompt_tokens: counters.prompt_tokens.load(Ordering::Relaxed),
                    completion_tokens: counters.completion_tokens.load(Ordering::Relaxed),
                    requests: counters.requests.load(Ordering::Relaxed),
                }
            })
            .collect()
    }
}

/// Removes its run's usage entry when dropped, including when the call's
/// future is dropped.
#[derive(Debug)]
pub struct TrackedRun {
    usage: Arc<DashMap<u64, RunUsage>>,
    id: u64,
}

impl Drop for TrackedRun {
    fn drop(&mut self) {
        self.usage.remove(&self.id);
    }
}

/// The card of one `subagent` call: running from its start, done with its
/// result, withdrawn when the call failed.
pub struct RunCard<'a> {
    tracker: &'a SubagentTracker,
    call_id: String,
    agent: String,
    prompt: String,
    started_at: jiff::Timestamp,
}

impl<'a> RunCard<'a> {
    /// Reports a running card for call `call_id`.
    pub fn start(
        tracker: &'a SubagentTracker,
        call_id: String,
        agent: String,
        prompt: String,
    ) -> Self {
        let card = Self {
            tracker,
            call_id,
            agent,
            prompt,
            started_at: jiff::Timestamp::now(),
        };
        card.report(Some(card.of(SubagentState::Running, None, None)));
        card
    }

    fn of(
        &self,
        state: SubagentState,
        finished_at: Option<jiff::Timestamp>,
        result: Option<String>,
    ) -> Card {
        Card::Subagent {
            agent: self.agent.clone(),
            title: Card::brief_title(&self.prompt),
            state,
            started_at: self.started_at,
            finished_at,
            result,
        }
    }

    fn report(&self, card: Option<Card>) {
        self.tracker.cards().report(&self.call_id, card);
    }

    /// Shows the run as done with `result`.
    pub fn done(self, result: String) {
        self.report(Some(self.of(
            SubagentState::Done,
            Some(jiff::Timestamp::now()),
            Some(result),
        )));
    }

    /// Withdraws the card: the ordinary row of a failed call shows why.
    pub fn withdraw(self) {
        self.report(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_reported_as_the_card_of_its_call_until_it_returns() {
        let tracker = SubagentTracker::default();
        let updates = tracker.cards().claim_updates().expect("reader");
        let card = RunCard::start(
            &tracker,
            "c1".into(),
            "explorer".into(),
            "\n## Map the parser\n\nbody".into(),
        );
        card.done("Found two parsers.".into());

        let running = updates.try_recv().expect("start");
        assert_eq!(running.call_id, "c1");
        let Some(Card::Subagent {
            agent,
            title,
            state,
            result,
            ..
        }) = running.card
        else {
            panic!("a subagent card");
        };
        assert_eq!(
            (agent.as_str(), title.as_str()),
            ("explorer", "Map the parser")
        );
        assert_eq!((state, result), (SubagentState::Running, None));

        let done = updates.try_recv().expect("completion");
        let Some(Card::Subagent {
            state,
            finished_at,
            result,
            ..
        }) = done.card
        else {
            panic!("a subagent card");
        };
        assert_eq!(state, SubagentState::Done);
        assert!(finished_at.is_some());
        assert_eq!(result.as_deref(), Some("Found two parsers."));
    }

    #[test]
    fn a_failed_run_withdraws_its_card() {
        let tracker = SubagentTracker::default();
        let updates = tracker.cards().claim_updates().expect("reader");
        RunCard::start(&tracker, "c1".into(), "explorer".into(), "brief".into()).withdraw();
        assert!(updates.try_recv().expect("start").card.is_some());
        assert_eq!(updates.try_recv().expect("withdrawn").card, None);
    }

    #[test]
    fn usage_is_credited_to_the_run_it_was_tracked_for() {
        let tracker = SubagentTracker::default();
        let runs = tracker.runs();
        let call = aither_core::llm::CallIdentity::new(
            aither_core::llm::ToolCallId::new("c1"),
            aither_core::llm::AgentIdentity::Main,
        );
        let run = runs.start(&call, "explorer", "brief");
        let (usage, tracked) = tracker.track(run.id());
        usage.record(&aither_core::llm::Usage::new(10, 4));
        let [snapshot] = tracker.running().try_into().expect("one run");
        assert_eq!(snapshot.total_tokens(), 14);
        assert_eq!(snapshot.requests, 1);
        drop(tracked);
        drop(run);
        assert!(tracker.running().is_empty());
    }
}
