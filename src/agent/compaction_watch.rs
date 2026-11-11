//! What the main agent's loop needs to know about its context window: the
//! `compact` tool's valve reads the tokens a request held, and the model is
//! warned once as the window nears its automatic compaction.
//!
//! aither's loop carries no product policy, so the window's measure is read
//! from the events of the run ([`MayRun`]) and from the agent between runs,
//! and handed to the valve and to the [`PreCompactionAlert`] source.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use aither_agent::{
    AgentError, AgentEvent, AgentPath, Checkpoints, CompactionOutcome, ContextBlock, ContextPolicy,
    ContextSource, Moment, Run, SourceId, TimedEvent,
};
use askama::Template;

use crate::harness::compact_tool::CompactSignal;

const TAG: &str = "precompaction_alert";

#[derive(Template)]
#[template(path = "precompaction_alert.txt", escape = "none")]
struct PreCompactionText {
    remaining_tokens: usize,
}

/// The tokens the context held at the last measure, against the policy's
/// warn point.
#[derive(Debug)]
pub struct WindowGauge {
    policy: ContextPolicy,
    /// The window's size in tokens; zero while unknown.
    window: AtomicU32,
    tokens: AtomicUsize,
    /// The warning went out and the window has not fallen below the warn
    /// point since.
    warned: AtomicBool,
}

impl WindowGauge {
    /// A gauge of a window of `window` tokens, if its size is known.
    #[must_use]
    pub const fn new(policy: ContextPolicy, window: Option<u32>) -> Self {
        Self {
            policy,
            window: AtomicU32::new(match window {
                Some(window) => window,
                None => 0,
            }),
            tokens: AtomicUsize::new(0),
            warned: AtomicBool::new(false),
        }
    }

    /// Records the window's size once the model's profile has told it.
    pub fn set_window(&self, window: u32) {
        self.window.store(window, Ordering::Release);
    }

    fn window(&self) -> Option<u32> {
        Some(self.window.load(Ordering::Acquire)).filter(|window| *window > 0)
    }

    /// Records that the context holds `tokens`. A window back under the warn
    /// point, as after a compaction, can warn again when it refills.
    pub fn record(&self, tokens: usize) {
        self.tokens.store(tokens, Ordering::Release);
        if !self.near_compaction() {
            self.warned.store(false, Ordering::Release);
        }
    }

    /// The tokens recorded last.
    #[must_use]
    pub fn tokens(&self) -> usize {
        self.tokens.load(Ordering::Acquire)
    }

    fn near_compaction(&self) -> bool {
        self.window()
            .is_some_and(|window| self.policy.should_warn(self.tokens(), window))
    }

    fn remaining(&self) -> usize {
        self.window().map_or(0, |window| {
            self.policy
                .pause_point(window)
                .saturating_sub(self.tokens())
        })
    }
}

/// Warns the model once that automatic compaction is near.
#[derive(Debug)]
pub struct PreCompactionAlert {
    gauge: Arc<WindowGauge>,
}

impl PreCompactionAlert {
    /// The source that reads `gauge`.
    #[must_use]
    pub const fn new(gauge: Arc<WindowGauge>) -> Self {
        Self { gauge }
    }

    fn block(&self) -> Vec<ContextBlock> {
        vec![ContextBlock::stable(
            TAG,
            PreCompactionText {
                remaining_tokens: self.gauge.remaining(),
            }
            .render()
            .expect("the pre-compaction alert template renders"),
        )]
    }
}

impl ContextSource for PreCompactionAlert {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. }
                if self.gauge.near_compaction()
                    && !self.gauge.warned.swap(true, Ordering::AcqRel) =>
            {
                self.block()
            }
            _ => Vec::new(),
        }
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. }
                if self.gauge.near_compaction() && !self.gauge.warned.load(Ordering::Acquire) =>
            {
                self.block()
            }
            _ => Vec::new(),
        }
    }
}

/// Feeds the window's measure to the valve and the gauge from a run's events.
#[derive(Debug, Clone)]
pub struct RunWatch {
    own: AgentPath,
    gauge: Arc<WindowGauge>,
    valve: Arc<CompactSignal>,
    floor: usize,
}

impl RunWatch {
    /// The watch of the agent `own`.
    #[must_use]
    pub const fn new(
        own: AgentPath,
        gauge: Arc<WindowGauge>,
        valve: Arc<CompactSignal>,
        floor: usize,
    ) -> Self {
        Self {
            own,
            gauge,
            valve,
            floor,
        }
    }

    /// Records that the context holds `tokens` and no compaction is in
    /// flight.
    pub fn measured(&self, tokens: usize) {
        self.gauge.record(tokens);
        self.valve.sync(false, tokens, self.floor);
    }

    /// Records an estimate of the context's tokens, for a window the
    /// provider has reported nothing on yet or one that grew since its last
    /// report. A report always replaces it. `window` is the model's context
    /// window as its profile gives it.
    pub fn estimated(&self, tokens: usize, window: u32) {
        self.gauge.set_window(window);
        if tokens > self.gauge.tokens() {
            self.measured(tokens);
        }
    }

    /// Records that a compaction ended. One that applied left the context as
    /// the handoff, and the valve refuses another until the agent acts.
    pub fn ended(&self, applied: bool) {
        if applied {
            self.measured(0);
            self.valve.landed();
        } else {
            self.measured(self.gauge.tokens());
        }
    }

    fn observe(&self, timed: &TimedEvent) {
        if timed.agent != self.own {
            return;
        }
        match &timed.event {
            AgentEvent::RequestEnded {
                usage: Some(usage), ..
            } => {
                let tokens = usage.prompt_tokens.unwrap_or(0);
                self.measured(usize::try_from(tokens).unwrap_or(usize::MAX));
            }
            AgentEvent::CompactionStarted { .. } => {
                self.valve.sync(true, self.gauge.tokens(), self.floor);
            }
            AgentEvent::CompactionEnded { outcome, .. } => {
                self.ended(matches!(outcome, CompactionOutcome::Applied(_)));
            }
            _ => {}
        }
    }
}

/// An agent's run whose events the agent's [`RunWatch`] reads before the
/// caller's handler does.
#[must_use = "a run does nothing until it is driven"]
pub struct MayRun<F> {
    run: Run<F>,
    watch: RunWatch,
}

impl<F> std::fmt::Debug for MayRun<F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MayRun")
            .field("watch", &self.watch)
            .finish_non_exhaustive()
    }
}

impl<F> MayRun<F>
where
    F: std::future::Future<Output = Result<(), AgentError>>,
{
    pub(crate) const fn new(run: Run<F>, watch: RunWatch) -> Self {
        Self { run, watch }
    }

    /// A handle to the run's checkpoints; see [`Run::checkpoints`].
    #[must_use]
    pub fn checkpoints(&self) -> Checkpoints {
        self.run.checkpoints()
    }

    /// Drives the run as [`Run::drive`] does.
    ///
    /// # Errors
    ///
    /// The run's error, after the handler has seen every event before it.
    pub async fn drive<S, B, H>(
        self,
        state: S,
        stop: impl std::future::Future<Output = B>,
        mut handler: impl FnMut(S, TimedEvent) -> H,
    ) -> Result<ControlFlow<B, S>, AgentError>
    where
        H: std::future::Future<Output = ControlFlow<B, S>>,
    {
        let Self { run, watch } = self;
        run.drive(state, stop, move |state, timed| {
            watch.observe(&timed);
            handler(state, timed)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> ContextPolicy {
        ContextPolicy {
            compact_ceiling: 40_000,
            start_distance: 2_000,
            warn_distance: 10_000,
            ..ContextPolicy::default()
        }
    }

    #[tokio::test]
    async fn the_warning_goes_out_once_until_the_window_empties() {
        let gauge = Arc::new(WindowGauge::new(policy(), Some(128_000)));
        let mut source = PreCompactionAlert::new(gauge.clone());
        let request = Moment::Request { reassembled: false };

        gauge.record(29_000);
        assert!(
            source.blocks(request).await.is_empty(),
            "below the warn point"
        );
        gauge.record(31_000);
        assert_eq!(source.peek(request).await.len(), 1, "a preview shows it");
        assert_eq!(source.blocks(request).await.len(), 1);
        gauge.record(35_000);
        assert!(source.blocks(request).await.is_empty(), "once");

        gauge.record(0);
        gauge.record(31_000);
        assert_eq!(
            source.blocks(request).await.len(),
            1,
            "a refilled window warns again"
        );
    }
}
