//! Loop detection: an agent that repeats the same calls, or keeps failing
//! the same way, is told so once, and its turn ends if the pattern goes on.
//!
//! Two rules count over the consecutive tool calls of one run, and a call
//! that breaks a pattern resets it:
//!
//! - call cycle: the last calls are one group of 1 to [`LONGEST_GROUP`]
//!   calls, compared by tool name and arguments, repeated in a row;
//! - same failure: calls in a row failed with the same error text, whatever
//!   their arguments, which the call-cycle rule cannot see since each retry
//!   differs.
//!
//! [`LoopDetector`] is the rules alone, fed one call at a time.
//! [`LoopWatch`] is one agent's detector under the daemon's live
//! [`LoopLimits`]: the agent's hooks feed it every tool turn, [`LoopAlerts`]
//! hands an alert to the model with the next request, and the turn runner
//! takes the pattern that ended a turn to tell the user.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use aither_agent::{ContextBlock, ContextSource, Moment, SourceId, ToolOutcome};
use aither_core::llm::{CommandOutcome, CommandStream, ToolResult};
use askama::Template;
use serde_json::Value;

use crate::config::LoopDetectionSettings;

const TAG: &str = "loop_alert";

/// The longest group of calls the call-cycle rule recognises; the loop
/// detection design (May-Agent/may#1052) names groups of 1 to 5 calls.
const LONGEST_GROUP: usize = 5;

/// A loop the rules saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopPattern {
    /// The last calls are one group of calls repeated in a row.
    CallCycle {
        /// The calls in the group.
        calls: usize,
        /// How many times in a row the group ran.
        repetitions: usize,
    },
    /// The last calls failed with the same error.
    SameFailure {
        /// How many calls in a row failed with it.
        repetitions: usize,
    },
}

impl LoopPattern {
    /// Why the turn was ended, as the user reads it.
    #[must_use]
    pub fn stop_notice(&self) -> String {
        LoopStoppedText { pattern: self }
            .render()
            .expect("the loop notice template renders")
    }
}

#[derive(Template)]
#[template(path = "loop_alert.txt", escape = "none")]
struct LoopAlertText<'a> {
    patterns: &'a [LoopPattern],
}

#[derive(Template)]
#[template(path = "loop_stopped.txt", escape = "none")]
struct LoopStoppedText<'a> {
    pattern: &'a LoopPattern,
}

/// What one call told the rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// No pattern reached a threshold.
    Nothing,
    /// These patterns reached their alert threshold with this call.
    Alert(Vec<LoopPattern>),
    /// This pattern reached the threshold at which the turn ends.
    End(LoopPattern),
}

/// A call as the call-cycle rule compares it.
#[derive(Debug, Clone, PartialEq)]
struct CallKey {
    name: String,
    arguments: Value,
}

impl CallKey {
    fn is(&self, name: &str, arguments: &Value) -> bool {
        self.name == name && self.arguments == *arguments
    }
}

/// The calls in a row that failed with `error`.
#[derive(Debug)]
struct FailureRun {
    error: String,
    count: usize,
    /// The run was alerted; it is not alerted again while it goes on.
    alerted: bool,
}

impl FailureRun {
    const fn pattern(&self) -> LoopPattern {
        LoopPattern::SameFailure {
            repetitions: self.count,
        }
    }
}

/// The cycle the latest call continues.
#[derive(Debug, Clone, Copy)]
struct Cycle {
    calls: usize,
    repetitions: usize,
}

impl Cycle {
    const fn pattern(self) -> LoopPattern {
        LoopPattern::CallCycle {
            calls: self.calls,
            repetitions: self.repetitions,
        }
    }
}

/// The two loop rules over a sequence of calls, fed one call at a time.
#[derive(Debug, Default)]
pub struct LoopDetector {
    /// The last calls, newest last; as many as the longest group.
    recent: VecDeque<CallKey>,
    /// For a group of `n` calls (index `n - 1`), how many calls in a row
    /// were the same as the call `n` before them.
    matched: [usize; LONGEST_GROUP],
    /// The size of the group whose cycle was alerted, while it goes on.
    cycle_alerted: Option<usize>,
    failure: Option<FailureRun>,
}

impl LoopDetector {
    /// Takes the next call: tool `name` ran with `arguments`, and `failure`
    /// is the error text the model received when the call failed. The
    /// thresholds are the ones in force for this call.
    pub fn observe(
        &mut self,
        name: &str,
        arguments: &Value,
        failure: Option<&str>,
        thresholds: LoopDetectionSettings,
    ) -> Detection {
        let cycle = self.follow_cycle(name, arguments);
        self.follow_failure(failure);
        if cycle.repetitions >= thresholds.cycle_end {
            return Detection::End(cycle.pattern());
        }
        if let Some(run) = &self.failure
            && run.count >= thresholds.failure_end
        {
            return Detection::End(run.pattern());
        }
        let mut alerts = Vec::new();
        if self.cycle_alerted.is_none() && cycle.repetitions >= thresholds.cycle_alert {
            self.cycle_alerted = Some(cycle.calls);
            alerts.push(cycle.pattern());
        }
        if let Some(run) = &mut self.failure
            && !run.alerted
            && run.count >= thresholds.failure_alert
        {
            run.alerted = true;
            alerts.push(run.pattern());
        }
        if alerts.is_empty() {
            Detection::Nothing
        } else {
            Detection::Alert(alerts)
        }
    }

    /// Records the call for the call-cycle rule and returns the cycle it
    /// continues: the alerted one while it goes on, else the group repeated
    /// most often, the smaller on a tie.
    fn follow_cycle(&mut self, name: &str, arguments: &Value) -> Cycle {
        for (index, matched) in self.matched.iter_mut().enumerate() {
            let before = self
                .recent
                .len()
                .checked_sub(index + 1)
                .and_then(|at| self.recent.get(at));
            *matched = if before.is_some_and(|call| call.is(name, arguments)) {
                *matched + 1
            } else {
                0
            };
        }
        self.recent.push_back(CallKey {
            name: name.to_owned(),
            arguments: arguments.clone(),
        });
        if self.recent.len() > LONGEST_GROUP {
            self.recent.pop_front();
        }
        if self
            .cycle_alerted
            .is_some_and(|calls| self.matched[calls - 1] == 0)
        {
            self.cycle_alerted = None;
        }
        let calls = self.cycle_alerted.unwrap_or_else(|| {
            (1..=LONGEST_GROUP).fold(1, |best, calls| {
                if self.repetitions(calls) > self.repetitions(best) {
                    calls
                } else {
                    best
                }
            })
        });
        Cycle {
            calls,
            repetitions: self.repetitions(calls),
        }
    }

    /// How many times in a row the last group of `calls` calls ran,
    /// counting only whole groups.
    const fn repetitions(&self, calls: usize) -> usize {
        self.matched[calls - 1] / calls + 1
    }

    /// Records the call for the same-failure rule: a failure with the
    /// previous call's error extends the run, any other call ends it.
    fn follow_failure(&mut self, failure: Option<&str>) {
        let previous = self.failure.take();
        self.failure = failure.map(|error| match previous {
            Some(run) if run.error == error => FailureRun {
                count: run.count + 1,
                ..run
            },
            _ => FailureRun {
                error: error.to_owned(),
                count: 1,
                alerted: false,
            },
        });
    }
}

/// The error text the model received from a call that failed: a tool
/// error, or what a command printed before it exited with a non-zero code.
/// `None` when it did not fail, and for a command that failed without
/// printing anything: a search that found nothing and a test that said no
/// exit that way, and with no text there is no error to call the same.
#[must_use]
pub fn failure_text(result: &ToolResult) -> Option<String> {
    let failed = result.command().map_or_else(
        || result.is_error(),
        |command| {
            matches!(command.outcome, CommandOutcome::Exited { code } if code != 0)
                && !(command.stdout.is_empty()
                    && command.stderr.as_ref().is_none_or(CommandStream::is_empty))
        },
    );
    failed.then(|| result.render_for_model())
}

/// The error text the model received from the call `outcome`, if it failed.
fn outcome_failure(outcome: &ToolOutcome) -> Option<String> {
    match &outcome.result {
        Ok(result) => failure_text(result),
        Err(_) => Some(outcome.for_model().render_for_model()),
    }
}

/// The loop thresholds every agent of the daemon counts to.
///
/// A clone shares the values, so the daemon keeps one, hands it to every
/// agent it builds and [`set`](Self::set)s it from each config change.
#[derive(Debug, Clone, Default)]
pub struct LoopLimits(Arc<LimitCells>);

#[derive(Debug)]
struct LimitCells {
    cycle_alert: AtomicUsize,
    cycle_end: AtomicUsize,
    failure_alert: AtomicUsize,
    failure_end: AtomicUsize,
}

impl Default for LimitCells {
    fn default() -> Self {
        let settings = LoopDetectionSettings::default();
        Self {
            cycle_alert: AtomicUsize::new(settings.cycle_alert),
            cycle_end: AtomicUsize::new(settings.cycle_end),
            failure_alert: AtomicUsize::new(settings.failure_alert),
            failure_end: AtomicUsize::new(settings.failure_end),
        }
    }
}

impl LoopLimits {
    /// Moves every holder of these limits to `settings`.
    pub fn set(&self, settings: LoopDetectionSettings) {
        let cells = &self.0;
        cells
            .cycle_alert
            .store(settings.cycle_alert, Ordering::Relaxed);
        cells.cycle_end.store(settings.cycle_end, Ordering::Relaxed);
        cells
            .failure_alert
            .store(settings.failure_alert, Ordering::Relaxed);
        cells
            .failure_end
            .store(settings.failure_end, Ordering::Relaxed);
    }

    /// The thresholds in force now.
    #[must_use]
    pub fn get(&self) -> LoopDetectionSettings {
        let cells = &self.0;
        LoopDetectionSettings {
            cycle_alert: cells.cycle_alert.load(Ordering::Relaxed),
            cycle_end: cells.cycle_end.load(Ordering::Relaxed),
            failure_alert: cells.failure_alert.load(Ordering::Relaxed),
            failure_end: cells.failure_end.load(Ordering::Relaxed),
        }
    }
}

/// What one agent's watch holds between its calls.
#[derive(Debug, Default)]
struct WatchState {
    detector: LoopDetector,
    /// Alerts the model has yet to read.
    alerts: Vec<LoopPattern>,
    /// The pattern that ended the run, until the turn runner takes it.
    stop: Option<LoopPattern>,
}

/// One agent's loop detection under the daemon's live thresholds.
///
/// The agent's hooks, its [`LoopAlerts`] source and the agent itself each
/// hold a clone; aither owns the first two and calls them separately, so the
/// state they share sits behind a lock that no await is held across.
#[derive(Debug, Clone)]
pub struct LoopWatch {
    limits: LoopLimits,
    state: Arc<Mutex<WatchState>>,
}

impl LoopWatch {
    /// A watch counting to `limits`.
    #[must_use]
    pub fn new(limits: LoopLimits) -> Self {
        Self {
            limits,
            state: Arc::default(),
        }
    }

    fn state(&self) -> MutexGuard<'_, WatchState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A run starts: the counting starts afresh, and an alert or stop the
    /// last run left is dropped with it.
    pub fn begin_run(&self) {
        *self.state() = WatchState::default();
    }

    /// Takes the calls of a finished tool turn; whether the turn must end.
    ///
    /// A turn's calls run concurrently and arrive in completion order, which
    /// says nothing about the order the model made them in, so they are
    /// taken in an order of their own: a repeated turn reads as the same
    /// group every time.
    #[must_use]
    pub fn observe_turn(&self, outcomes: &[ToolOutcome]) -> bool {
        let thresholds = self.limits.get();
        let mut calls: Vec<(&ToolOutcome, String, Option<String>)> = outcomes
            .iter()
            .map(|outcome| {
                (
                    outcome,
                    outcome.arguments.to_string(),
                    outcome_failure(outcome),
                )
            })
            .collect();
        calls.sort_by(|(left, left_arguments, _), (right, right_arguments, _)| {
            left.name
                .cmp(&right.name)
                .then_with(|| left_arguments.cmp(right_arguments))
        });
        let mut state = self.state();
        for (outcome, _, failure) in &calls {
            match state.detector.observe(
                &outcome.name,
                &outcome.arguments,
                failure.as_deref(),
                thresholds,
            ) {
                Detection::Nothing => {}
                Detection::Alert(patterns) => state.alerts.extend(patterns),
                Detection::End(pattern) => {
                    state.stop = Some(pattern);
                    break;
                }
            }
        }
        state.stop.is_some()
    }

    /// The pattern that ended the last run, once.
    #[must_use]
    pub fn take_stop(&self) -> Option<LoopPattern> {
        self.state().stop.take()
    }

    /// The source that hands this watch's alerts to the model.
    #[must_use]
    pub fn alerts(&self) -> LoopAlerts {
        LoopAlerts(self.clone())
    }
}

/// Hands each loop alert to the model once, with the next request.
#[derive(Debug)]
pub struct LoopAlerts(LoopWatch);

fn alert_blocks(patterns: &[LoopPattern]) -> Vec<ContextBlock> {
    if patterns.is_empty() {
        return Vec::new();
    }
    vec![ContextBlock::stable(
        TAG,
        LoopAlertText { patterns }
            .render()
            .expect("the loop alert template renders"),
    )]
}

impl ContextSource for LoopAlerts {
    fn id(&self) -> SourceId {
        SourceId::new(TAG)
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn blocks(&mut self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => {
                let alerts = std::mem::take(&mut self.0.state().alerts);
                alert_blocks(&alerts)
            }
            _ => Vec::new(),
        }
    }

    #[allow(clippy::unused_async_trait_impl, reason = "the trait method is async")]
    async fn peek(&self, moment: Moment) -> Vec<ContextBlock> {
        match moment {
            Moment::Request { .. } => {
                let alerts = self.0.state().alerts.clone();
                alert_blocks(&alerts)
            }
            _ => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::CommandResult;

    use super::*;

    /// Thresholds low enough to reach in a few calls: alert at 3, end at 5.
    const LOW: LoopDetectionSettings = LoopDetectionSettings {
        cycle_alert: 3,
        cycle_end: 5,
        failure_alert: 3,
        failure_end: 5,
    };

    /// A call: the tool, its script, and the error it failed with.
    type Call<'a> = (&'a str, &'a str, Option<&'a str>);

    fn script(text: &str) -> Value {
        serde_json::json!({ "script": text, "description": "检查 検査 확인" })
    }

    /// Feeds `calls` and returns what each did.
    fn feed(
        detector: &mut LoopDetector,
        calls: &[Call<'_>],
        thresholds: LoopDetectionSettings,
    ) -> Vec<Detection> {
        calls
            .iter()
            .map(|(name, text, failure)| {
                detector.observe(name, &script(text), *failure, thresholds)
            })
            .collect()
    }

    fn alerts_and_ends(detections: &[Detection]) -> Vec<(usize, &Detection)> {
        detections
            .iter()
            .enumerate()
            .filter(|(_, detection)| **detection != Detection::Nothing)
            .collect()
    }

    /// Asserts that no call raised anything.
    fn assert_quiet(detections: &[Detection]) {
        assert_eq!(
            alerts_and_ends(detections),
            [] as [(usize, &Detection); 0],
            "{detections:?}"
        );
    }

    #[test]
    fn a_repeated_call_is_alerted_once_and_ended_at_the_second_threshold() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = vec![("terminal", "cargo test", None); 5];
        let seen = feed(&mut detector, &calls, LOW);
        assert_eq!(
            alerts_and_ends(&seen),
            [
                (
                    2,
                    &Detection::Alert(vec![LoopPattern::CallCycle {
                        calls: 1,
                        repetitions: 3
                    }])
                ),
                (
                    4,
                    &Detection::End(LoopPattern::CallCycle {
                        calls: 1,
                        repetitions: 5
                    })
                ),
            ]
        );
    }

    #[test]
    fn a_repeated_group_of_calls_is_a_cycle_of_that_group() {
        let mut detector = LoopDetector::default();
        let group: [Call; 3] = [
            ("terminal", "cargo build", None),
            ("terminal", "cat src/lib.rs", None),
            ("monitor", "tail -f build.log", None),
        ];
        let calls: Vec<Call> = group.iter().copied().cycle().take(15).collect();
        let seen = feed(&mut detector, &calls, LOW);
        assert_eq!(
            alerts_and_ends(&seen),
            [
                (
                    8,
                    &Detection::Alert(vec![LoopPattern::CallCycle {
                        calls: 3,
                        repetitions: 3
                    }])
                ),
                (
                    14,
                    &Detection::End(LoopPattern::CallCycle {
                        calls: 3,
                        repetitions: 5
                    })
                ),
            ]
        );
    }

    #[test]
    fn a_group_of_six_calls_is_longer_than_any_cycle() {
        let mut detector = LoopDetector::default();
        let group: [Call; 6] = ["a", "b", "c", "d", "e", "f"].map(|text| ("terminal", text, None));
        let calls: Vec<Call> = group.iter().copied().cycle().take(36).collect();
        assert_quiet(&feed(&mut detector, &calls, LOW));
    }

    #[test]
    fn the_same_tool_with_other_arguments_is_no_cycle() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = ["ls", "ls -a", "ls -l", "ls src", "ls tests", "ls docs"]
            .map(|text| ("terminal", text, None))
            .to_vec();
        assert_quiet(&feed(&mut detector, &calls, LOW));
    }

    #[test]
    fn a_broken_cycle_raises_nothing() {
        let mut detector = LoopDetector::default();
        let a: Call = ("terminal", "cargo check", None);
        let b: Call = ("terminal", "git status", None);
        let c: Call = ("terminal", "git log -1", None);
        // Two in a row, a break, two more, another break, two more: no
        // call and no group ever runs three times in a row.
        let calls = [a, a, b, a, a, c, a, a];
        assert_quiet(&feed(&mut detector, &calls, LOW));
    }

    #[test]
    fn a_cycle_that_breaks_after_its_alert_is_a_new_event_when_it_returns() {
        let mut detector = LoopDetector::default();
        let a: Call = ("terminal", "cargo check", None);
        let b: Call = ("terminal", "git diff", None);
        let calls = [a, a, a, a, b, a, a, a];
        let alert = Detection::Alert(vec![LoopPattern::CallCycle {
            calls: 1,
            repetitions: 3,
        }]);
        assert_eq!(
            alerts_and_ends(&feed(&mut detector, &calls, LOW)),
            [(2, &alert), (7, &alert)],
            "the call after the alert breaks the cycle; three in a row again is alerted again"
        );
    }

    const GONE: &str = "Error: the working directory /tmp/工作目录/作業ディレクトリ/작업 디렉터리 no longer exists";

    #[test]
    fn identical_failures_with_differing_calls_are_alerted_once_and_ended() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = ["ls", "pwd", "cd ..", "ls -a", "true"]
            .map(|text| ("terminal", text, Some(GONE)))
            .to_vec();
        let seen = feed(&mut detector, &calls, LOW);
        assert_eq!(
            alerts_and_ends(&seen),
            [
                (
                    2,
                    &Detection::Alert(vec![LoopPattern::SameFailure { repetitions: 3 }])
                ),
                (
                    4,
                    &Detection::End(LoopPattern::SameFailure { repetitions: 5 })
                ),
            ]
        );
    }

    #[test]
    fn a_failure_run_that_breaks_after_its_alert_is_a_new_event_when_it_returns() {
        let mut detector = LoopDetector::default();
        let failing = |text: &'static str| -> Call<'static> { ("terminal", text, Some(GONE)) };
        let calls = [
            failing("ls"),
            failing("pwd"),
            failing("ls -a"),
            ("terminal", "cd /tmp", None),
            failing("ls -l"),
            failing("pwd -P"),
            failing("ls -R"),
        ];
        let alert = Detection::Alert(vec![LoopPattern::SameFailure { repetitions: 3 }]);
        assert_eq!(
            alerts_and_ends(&feed(&mut detector, &calls, LOW)),
            [(2, &alert), (6, &alert)],
            "the success after the alert ends the run; three in a row again is alerted again"
        );
    }

    #[test]
    fn differing_errors_raise_nothing() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = ["a", "b", "c", "d", "e", "f"]
            .map(|text| ("terminal", text, Some(text)))
            .to_vec();
        assert_quiet(&feed(&mut detector, &calls, LOW));
    }

    #[test]
    fn interleaved_successes_raise_nothing() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = (0..12)
            .map(|step| {
                if step % 2 == 0 {
                    (
                        "terminal",
                        ["ls a", "ls b", "ls c", "ls d", "ls e", "ls f"][step / 2],
                        Some(GONE),
                    )
                } else {
                    (
                        "terminal",
                        ["echo 1", "echo 2", "echo 3", "echo 4", "echo 5", "echo 6"][step / 2],
                        None,
                    )
                }
            })
            .collect();
        assert_quiet(&feed(&mut detector, &calls, LOW));
    }

    #[test]
    fn an_identical_failing_call_raises_both_rules_in_one_alert() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = vec![("terminal", "cargo test", Some(GONE)); 3];
        let seen = feed(&mut detector, &calls, LOW);
        assert_eq!(
            seen.last(),
            Some(&Detection::Alert(vec![
                LoopPattern::CallCycle {
                    calls: 1,
                    repetitions: 3
                },
                LoopPattern::SameFailure { repetitions: 3 },
            ]))
        );
    }

    #[test]
    fn the_thresholds_of_each_call_are_the_ones_it_is_counted_to() {
        let mut detector = LoopDetector::default();
        let call: Call = ("terminal", "cargo test", None);
        // The defaults alert later than three in a row.
        assert_quiet(&feed(
            &mut detector,
            &[call, call, call],
            LoopDetectionSettings::default(),
        ));
        assert_eq!(
            feed(&mut detector, &[call], LOW),
            [Detection::Alert(vec![LoopPattern::CallCycle {
                calls: 1,
                repetitions: 4
            }])],
            "lowered thresholds apply to the next call"
        );
    }

    #[test]
    fn the_defaults_alert_at_five_and_end_at_ten() {
        let mut detector = LoopDetector::default();
        let calls: Vec<Call> = vec![("terminal", "cargo test", None); 10];
        let seen = feed(&mut detector, &calls, LoopDetectionSettings::default());
        let marks: Vec<usize> = alerts_and_ends(&seen)
            .into_iter()
            .map(|(index, _)| index + 1)
            .collect();
        assert_eq!(marks, [5, 10]);
    }

    fn command(code: i32, stderr: &str) -> ToolResult {
        ToolResult::Command(Box::new(CommandResult {
            stdout: CommandStream::Inline {
                text: String::new(),
            },
            stderr: Some(CommandStream::Inline {
                text: stderr.to_string(),
            }),
            outcome: CommandOutcome::Exited { code },
            interruptions: Vec::new(),
        }))
    }

    #[test]
    fn a_failure_is_a_tool_error_or_a_non_zero_exit_as_the_model_read_it() {
        assert_eq!(
            failure_text(&ToolResult::error(GONE)),
            Some(GONE.to_string())
        );
        let failed = command(2, "ls: 不存在 / 存在しません / 없음");
        assert_eq!(failure_text(&failed), Some(failed.render_for_model()));
        assert_eq!(failure_text(&command(0, "warning")), None);
        // `rg` finding nothing: a non-zero exit that says nothing.
        assert_eq!(failure_text(&command(1, "")), None);
        assert_eq!(failure_text(&ToolResult::text("ok")), None);
    }

    #[test]
    fn a_config_change_moves_the_limits_watches_were_built_with() {
        let limits = LoopLimits::default();
        let watch = LoopWatch::new(limits.clone());
        assert_eq!(watch.limits.get(), LoopDetectionSettings::default());
        limits.set(LOW);
        assert_eq!(watch.limits.get(), LOW);
    }

    #[tokio::test]
    async fn an_alert_reaches_the_next_request_once() {
        let watch = LoopWatch::new(LoopLimits::default());
        let mut source = watch.alerts();
        let request = Moment::Request { reassembled: false };
        assert_eq!(source.blocks(request).await, [] as [ContextBlock; 0]);
        watch
            .state()
            .alerts
            .push(LoopPattern::SameFailure { repetitions: 5 });

        let peeked = source.peek(request).await;
        assert_eq!(peeked.len(), 1, "a preview shows the alert");
        let blocks = source.blocks(request).await;
        let [block] = blocks.as_slice() else {
            panic!("one alert: {blocks:?}");
        };
        assert_eq!(block.block().tag(), TAG);
        assert!(
            block.block().content().starts_with(
                "Your last 5 calls failed with the same error. Retrying the same way cannot work"
            ),
            "{}",
            block.block().content()
        );
        assert_eq!(
            source.blocks(request).await,
            [] as [ContextBlock; 0],
            "once"
        );
    }

    #[test]
    fn a_new_run_drops_what_the_last_one_left() {
        let watch = LoopWatch::new(LoopLimits::default());
        {
            let mut state = watch.state();
            state
                .alerts
                .push(LoopPattern::SameFailure { repetitions: 5 });
            state.stop = Some(LoopPattern::SameFailure { repetitions: 10 });
        }
        watch.begin_run();
        assert_eq!(watch.take_stop(), None);
        assert_eq!(watch.state().alerts, [] as [LoopPattern; 0]);
    }

    #[test]
    fn the_notice_names_the_pattern() {
        assert_eq!(
            LoopPattern::CallCycle {
                calls: 1,
                repetitions: 10
            }
            .stop_notice(),
            "The turn was ended because the agent was looping: it made the same call 10 times in a row. Send a message to continue."
        );
        assert_eq!(
            LoopPattern::CallCycle {
                calls: 2,
                repetitions: 10
            }
            .stop_notice(),
            "The turn was ended because the agent was looping: it repeated the same group of 2 calls 10 times in a row. Send a message to continue."
        );
        assert_eq!(
            LoopPattern::SameFailure { repetitions: 10 }.stop_notice(),
            "The turn was ended because the agent was looping: its last 10 calls failed with the same error. Send a message to continue."
        );
    }
}
