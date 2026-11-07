//! Whether the user is asleep: one state for the whole daemon.
//!
//! `/sleep` sets it; `/sleep off`, or a user message sent [`MIN_SLEEP`] or
//! more after it began, ends it. Only a sleep that lasted [`MIN_SLEEP`] is
//! recorded, when it ends, at the time it began. Every client follows
//! it on the `sleep` event of the event stream, and every session's agent is
//! told once per change, at the start of its next turn
//! ([`SleepState::take_notice`]). The state survives a daemon restart.
//!
//! The health reminder is shown to the user by the harness and never reaches
//! an agent: [`SleepState::remind_if_due`] only bumps the counter clients
//! watch on the `sleep` event.

// The state lock is held across persisting the change on purpose: writes to
// the file land in the order the changes were made.
#![allow(clippy::significant_drop_tightening)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::db::ChatDb;
use crate::ui_event_bus::{UiEventBus, UiEventTopics};

/// File under the May directory holding the persisted state.
const STATE_FILE: &str = "sleep.json";

/// A user message ends the state only this long after it began.
pub const MIN_SLEEP: Duration = Duration::from_hours(1);
const MIN_SLEEP_MS: i64 = 60 * 60 * 1000;

/// How long after the median `/sleep` time the reminder may still fire.
const REMINDER_WINDOW_MINUTES: u32 = 30;

/// The reminder needs `/sleep` on at least this many distinct nights: a few
/// test runs never make a habit, a bedtime kept for about a week does.
const MIN_HABIT_NIGHTS: usize = 5;

/// The median absolute circular deviation of the recorded times of day from
/// their median must not exceed this. An hour tolerates an ordinary
/// bedtime drifting from night to night, while times spread across the day
/// (a deviation of several hours) are no habit.
const MAX_HABIT_DEVIATION_MINUTES: u32 = 60;

/// Shifts a sleep time so that its local date names the night it belongs to
/// (a night runs noon to noon, so 23:30 and the 00:30 after it are one night).
const NIGHT_OFFSET: jiff::SignedDuration = jiff::SignedDuration::from_hours(12);

/// Reminders are at least this far apart.
const REMINDER_SPACING_MS: i64 = 12 * 60 * 60 * 1000;

const MINUTES_PER_DAY: u32 = 24 * 60;

/// What clients see of the state.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SleepStatus {
    /// Whether the user is asleep.
    pub asleep: bool,
    /// The session `/sleep` was run in, while the user is asleep.
    #[serde(default)]
    pub origin: Option<String>,
    /// How many health reminders this daemon has raised; a client shows the
    /// reminder when the count grows.
    #[serde(default)]
    pub reminders: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Persisted {
    asleep: bool,
    origin: Option<String>,
    /// When the state began, Unix milliseconds.
    since_ms: Option<i64>,
    /// Sessions whose agent was told the user fell asleep and not yet that
    /// they woke.
    told: BTreeSet<String>,
    #[serde(skip)]
    reminders: u64,
    #[serde(skip)]
    last_reminded_ms: Option<i64>,
}

impl Persisted {
    fn status(&self) -> SleepStatus {
        SleepStatus {
            asleep: self.asleep,
            origin: self.origin.clone(),
            reminders: self.reminders,
        }
    }
}

/// What a session's agent is told on its next turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepNotice {
    /// The user fell asleep; `greet` is set in the session that ran `/sleep`.
    Asleep {
        /// Whether this session's agent wishes the user good night.
        greet: bool,
    },
    /// The user woke.
    Woke,
}

/// The daemon's sleep state.
#[derive(Debug)]
pub struct SleepState {
    path: PathBuf,
    bus: UiEventBus,
    db: ChatDb,
    /// Mirror of the persisted flag for synchronous readers.
    asleep: AtomicBool,
    inner: Mutex<Persisted>,
}

impl SleepState {
    /// Loads the state kept in `may_dir`; a missing file means awake.
    ///
    /// # Panics
    /// When the file exists and cannot be read or parsed.
    #[must_use]
    pub fn load(may_dir: &Path, bus: UiEventBus, db: ChatDb) -> Self {
        let path = may_dir.join(STATE_FILE);
        let persisted = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str::<Persisted>(&text)
                .unwrap_or_else(|error| panic!("{} is not valid: {error}", path.display())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
            Err(error) => panic!("cannot read {}: {error}", path.display()),
        };
        Self {
            path,
            bus,
            db,
            asleep: AtomicBool::new(persisted.asleep),
            inner: Mutex::new(persisted),
        }
    }

    /// Whether the user is asleep.
    pub fn is_asleep(&self) -> bool {
        self.asleep.load(Ordering::SeqCst)
    }

    /// The current state.
    pub async fn status(&self) -> SleepStatus {
        self.inner.lock().await.status()
    }

    /// Marks the user asleep from `now`, as `/sleep` run in `origin`.
    /// Returns whether the state changed.
    ///
    /// # Errors
    /// When the state cannot be persisted.
    pub async fn sleep(&self, origin: &str, now: Timestamp) -> std::io::Result<bool> {
        let mut inner = self.inner.lock().await;
        if inner.asleep {
            return Ok(false);
        }
        inner.asleep = true;
        inner.origin = Some(origin.to_owned());
        inner.since_ms = Some(now.as_millisecond());
        // The session that ran the command is told again even if it never
        // heard the previous wake.
        inner.told.remove(origin);
        self.commit(&inner).await?;
        Ok(true)
    }

    /// Ends the state at `now` (`/sleep off`). Returns whether it changed.
    /// A sleep shorter than [`MIN_SLEEP`] was no sleep and leaves no record.
    ///
    /// # Errors
    /// When the sleep cannot be recorded or the state persisted.
    pub async fn wake(&self, now: Timestamp) -> Result<bool, SleepError> {
        let mut inner = self.inner.lock().await;
        self.end(&mut inner, now).await
    }

    /// The user sent a message at `now`: ends the state when it began
    /// [`MIN_SLEEP`] or more ago, and leaves it otherwise. Returns whether
    /// the state ended.
    ///
    /// # Errors
    /// When the sleep cannot be recorded or the state persisted.
    pub async fn user_message(&self, now: Timestamp) -> Result<bool, SleepError> {
        let mut inner = self.inner.lock().await;
        if !inner.asleep || !slept_long_enough(&inner, now) {
            return Ok(false);
        }
        self.end(&mut inner, now).await
    }

    /// Ends the state, recording the sleep when it lasted [`MIN_SLEEP`].
    async fn end(&self, inner: &mut Persisted, now: Timestamp) -> Result<bool, SleepError> {
        if !inner.asleep {
            return Ok(false);
        }
        if let (true, Some(since)) = (slept_long_enough(inner, now), inner.since_ms) {
            let began = Timestamp::from_millisecond(since).expect("persisted start is in range");
            self.db.record_sleep(began).await?;
        }
        inner.asleep = false;
        inner.origin = None;
        inner.since_ms = None;
        self.commit(inner).await?;
        Ok(true)
    }

    /// What `session_id`'s agent has not been told yet, recorded as told.
    ///
    /// # Errors
    /// When the state cannot be persisted.
    pub async fn take_notice(&self, session_id: &str) -> std::io::Result<Option<SleepNotice>> {
        let mut inner = self.inner.lock().await;
        let told = inner.told.contains(session_id);
        let notice = match (inner.asleep, told) {
            (true, false) => {
                inner.told.insert(session_id.to_owned());
                SleepNotice::Asleep {
                    greet: inner.origin.as_deref() == Some(session_id),
                }
            }
            (false, true) => {
                inner.told.remove(session_id);
                SleepNotice::Woke
            }
            _ => return Ok(None),
        };
        self.persist(&inner).await?;
        Ok(Some(notice))
    }

    /// Raises the health reminder when `now` is within
    /// [`REMINDER_WINDOW_MINUTES`] after the median time of day of the
    /// recorded sleeps in `zone`, the user is awake, and none was raised in
    /// the last twelve hours. The history must be a habit: sleeps on at
    /// least [`MIN_HABIT_NIGHTS`] distinct nights, within
    /// [`MAX_HABIT_DEVIATION_MINUTES`] of their median. Returns whether one
    /// was raised.
    ///
    /// # Errors
    /// When the history cannot be read.
    pub async fn remind_if_due(&self, now: Timestamp, zone: &TimeZone) -> Result<bool, SleepError> {
        let times = self.db.sleep_times().await?;
        let nights: BTreeSet<_> = times
            .iter()
            .map(|at| (*at - NIGHT_OFFSET).to_zoned(zone.clone()).date())
            .collect();
        if nights.len() < MIN_HABIT_NIGHTS {
            return Ok(false);
        }
        let minutes: Vec<u32> = times
            .into_iter()
            .map(|at| minute_of_day(at, zone))
            .collect();
        let Some(median) = median_minute_of_day(&minutes) else {
            return Ok(false);
        };
        if median_circular_deviation(&minutes, median) > MAX_HABIT_DEVIATION_MINUTES {
            return Ok(false);
        }
        let since_median = (minute_of_day(now, zone) + MINUTES_PER_DAY - median) % MINUTES_PER_DAY;
        let mut inner = self.inner.lock().await;
        let spaced = inner
            .last_reminded_ms
            .is_none_or(|last| now.as_millisecond().saturating_sub(last) > REMINDER_SPACING_MS);
        if inner.asleep || since_median >= REMINDER_WINDOW_MINUTES || !spaced {
            return Ok(false);
        }
        inner.reminders += 1;
        inner.last_reminded_ms = Some(now.as_millisecond());
        drop(inner);
        self.bus.publish(UiEventTopics::SLEEP);
        Ok(true)
    }

    /// Persists the state and tells every client.
    async fn commit(&self, inner: &Persisted) -> std::io::Result<()> {
        self.asleep.store(inner.asleep, Ordering::SeqCst);
        self.persist(inner).await?;
        self.bus.publish(UiEventTopics::SLEEP);
        Ok(())
    }

    async fn persist(&self, inner: &Persisted) -> std::io::Result<()> {
        let text = serde_json::to_string(inner).map_err(std::io::Error::other)?;
        async_fs::write(&self.path, text).await
    }
}

/// Why a sleep operation failed.
#[derive(Debug, thiserror::Error)]
pub enum SleepError {
    /// The state file could not be written.
    #[error("sleep state: {0}")]
    Io(#[from] std::io::Error),
    /// The sleep log could not be read or written.
    #[error("sleep log: {0}")]
    Db(#[from] crate::db::ChatDbError),
}

fn slept_long_enough(inner: &Persisted, now: Timestamp) -> bool {
    inner
        .since_ms
        .is_some_and(|since| now.as_millisecond().saturating_sub(since) >= MIN_SLEEP_MS)
}

/// The median of the distances on the 24-hour circle from `minutes` to
/// `center`, all minutes since midnight.
fn median_circular_deviation(minutes: &[u32], center: u32) -> u32 {
    let mut distances: Vec<u32> = minutes
        .iter()
        .map(|minute| {
            let apart = minute.abs_diff(center) % MINUTES_PER_DAY;
            apart.min(MINUTES_PER_DAY - apart)
        })
        .collect();
    distances.sort_unstable();
    let middle = distances.len() / 2;
    if distances.len() % 2 == 1 {
        distances[middle]
    } else {
        u32::midpoint(distances[middle - 1], distances[middle])
    }
}

fn minute_of_day(at: Timestamp, zone: &TimeZone) -> u32 {
    let time = at.to_zoned(zone.clone()).time();
    u32::try_from(time.hour()).expect("hour is non-negative") * 60
        + u32::try_from(time.minute()).expect("minute is non-negative")
}

/// The median of times of day, given as minutes since midnight, on the
/// 24-hour circle; `None` without times.
///
/// The circle is cut at its largest gap and the times are ordered from
/// there, so 23:30 and 00:30 have the median midnight.
#[must_use]
pub fn median_minute_of_day(minutes: &[u32]) -> Option<u32> {
    let mut sorted: Vec<u32> = minutes.iter().map(|m| m % MINUTES_PER_DAY).collect();
    sorted.sort_unstable();
    let last = *sorted.last()?;
    // The gap before each time, the first one wrapping past midnight.
    let gap_before = |index: usize| {
        let previous = if index == 0 { last } else { sorted[index - 1] };
        (sorted[index] + MINUTES_PER_DAY - previous) % MINUTES_PER_DAY
    };
    let start = (0..sorted.len())
        .max_by_key(|&index| gap_before(index))
        .unwrap_or(0);
    sorted.rotate_left(start);
    let mut unrolled: Vec<u32> = Vec::with_capacity(sorted.len());
    let mut offset = 0;
    for (index, minute) in sorted.iter().enumerate() {
        if index > 0 && *minute < sorted[index - 1] {
            offset = MINUTES_PER_DAY;
        }
        unrolled.push(minute + offset);
    }
    let middle = unrolled.len() / 2;
    let median = if unrolled.len() % 2 == 1 {
        unrolled[middle]
    } else {
        u32::midpoint(unrolled[middle - 1], unrolled[middle])
    };
    Some(median % MINUTES_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn state(dir: &Path) -> (SleepState, UiEventBus) {
        let bus = UiEventBus::new();
        let db = ChatDb::open(&dir.join("history.db")).await.expect("db");
        (SleepState::load(dir, bus.clone(), db), bus)
    }

    fn at(rfc3339: &str) -> Timestamp {
        rfc3339.parse().expect("timestamp")
    }

    const T0: &str = "2026-03-01T23:00:00Z";

    #[tokio::test]
    async fn every_session_is_told_once_per_change() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, bus) = state(dir.path()).await;
        let mut events = bus.subscribe();

        assert_eq!(sleep.take_notice("a").await.unwrap(), None);
        assert!(sleep.sleep("a", at(T0)).await.unwrap());
        assert!(sleep.is_asleep());
        assert!(events.recv().await.unwrap().contains(UiEventTopics::SLEEP));
        assert!(!sleep.sleep("b", at(T0)).await.unwrap(), "already asleep");

        assert_eq!(
            sleep.take_notice("a").await.unwrap(),
            Some(SleepNotice::Asleep { greet: true })
        );
        assert_eq!(
            sleep.take_notice("b").await.unwrap(),
            Some(SleepNotice::Asleep { greet: false })
        );
        assert_eq!(
            sleep.take_notice("a").await.unwrap(),
            None,
            "never replayed"
        );

        assert!(sleep.wake(at(T0)).await.unwrap());
        assert!(!sleep.is_asleep());
        assert!(events.recv().await.unwrap().contains(UiEventTopics::SLEEP));
        assert_eq!(
            sleep.take_notice("b").await.unwrap(),
            Some(SleepNotice::Woke)
        );
        assert_eq!(sleep.take_notice("b").await.unwrap(), None);
        assert_eq!(
            sleep.take_notice("never-told").await.unwrap(),
            None,
            "a session that never heard of the sleep is not woken"
        );
    }

    #[tokio::test]
    async fn the_state_survives_a_restart() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        sleep.sleep("a", at(T0)).await.unwrap();
        sleep.take_notice("a").await.unwrap();

        let (restarted, _) = state(dir.path()).await;
        assert!(restarted.is_asleep());
        assert_eq!(restarted.status().await.origin.as_deref(), Some("a"));
        assert_eq!(
            restarted.take_notice("a").await.unwrap(),
            None,
            "told before"
        );
        assert_eq!(
            restarted.take_notice("b").await.unwrap(),
            Some(SleepNotice::Asleep { greet: false })
        );
    }

    #[tokio::test]
    async fn a_message_ends_the_state_only_from_one_hour_on() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        let began = at(T0);
        sleep.sleep("a", began).await.unwrap();

        let just_before = began + MIN_SLEEP.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(!sleep.user_message(just_before).await.unwrap());
        assert!(
            sleep.is_asleep(),
            "a message within the hour leaves sleep on"
        );
        assert!(sleep.user_message(began + MIN_SLEEP).await.unwrap());
        assert!(!sleep.is_asleep(), "exactly one hour ends it");
        assert!(!sleep.user_message(began + MIN_SLEEP).await.unwrap());
    }

    #[tokio::test]
    async fn sleep_off_ends_the_state_at_once() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        sleep.sleep("a", at(T0)).await.unwrap();
        assert!(sleep.wake(at(T0)).await.unwrap());
        assert!(!sleep.is_asleep());
        assert!(!sleep.wake(at(T0)).await.unwrap());
    }

    #[tokio::test]
    async fn an_early_sleep_off_leaves_no_history_and_no_reminder() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        let began = at(T0);
        sleep.sleep("a", began).await.unwrap();
        let early = began + MIN_SLEEP.checked_sub(Duration::from_millis(1)).unwrap();
        assert!(sleep.wake(early).await.unwrap());
        assert_eq!(
            sleep.db.sleep_times().await.unwrap(),
            Vec::<Timestamp>::new()
        );
        assert!(!sleep.remind_if_due(began, &TimeZone::UTC).await.unwrap());
    }

    #[tokio::test]
    async fn a_real_sleep_is_recorded_at_its_start_across_a_restart() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        let began = at(T0);
        sleep.sleep("a", began).await.unwrap();

        let (restarted, _) = state(dir.path()).await;
        assert!(
            restarted.db.sleep_times().await.unwrap().is_empty(),
            "nothing before it ends"
        );
        assert!(restarted.user_message(began + MIN_SLEEP).await.unwrap());
        assert_eq!(restarted.db.sleep_times().await.unwrap(), vec![began]);
    }

    #[tokio::test]
    async fn too_few_nights_or_scattered_times_never_remind() {
        let utc = TimeZone::UTC;
        for times in [
            // Two runs, even at the same time.
            &["2026-02-26T23:30:00Z", "2026-02-27T23:30:00Z"][..],
            // Many sleeps on one night.
            &[
                "2026-02-27T22:00:00Z",
                "2026-02-27T22:10:00Z",
                "2026-02-27T22:20:00Z",
                "2026-02-27T22:30:00Z",
                "2026-02-27T22:40:00Z",
            ][..],
            // Five nights spread over the day.
            &[
                "2026-02-24T01:00:00Z",
                "2026-02-25T06:00:00Z",
                "2026-02-26T12:00:00Z",
                "2026-02-27T17:00:00Z",
                "2026-02-28T22:00:00Z",
            ][..],
        ] {
            let dir = tempfile::tempdir().expect("dir");
            let (sleep, _) = state(dir.path()).await;
            for time in times {
                sleep.db.record_sleep(at(time)).await.unwrap();
            }
            for probe in [
                "2026-03-01T01:00:00Z",
                "2026-03-01T06:00:00Z",
                "2026-03-01T12:00:00Z",
                "2026-03-01T17:00:00Z",
                "2026-03-01T22:00:00Z",
                "2026-03-01T23:30:00Z",
            ] {
                assert!(
                    !sleep.remind_if_due(at(probe), &utc).await.unwrap(),
                    "{times:?} at {probe}"
                );
            }
        }
    }

    #[test]
    fn the_median_wraps_past_midnight() {
        assert_eq!(median_minute_of_day(&[]), None);
        assert_eq!(median_minute_of_day(&[23 * 60]), Some(23 * 60));
        assert_eq!(
            median_minute_of_day(&[23 * 60 + 30, 30]),
            Some(0),
            "23:30 and 00:30 meet at midnight, not noon"
        );
        assert_eq!(
            median_minute_of_day(&[23 * 60, 23 * 60 + 30, 60]),
            Some(23 * 60 + 30)
        );
        assert_eq!(
            median_minute_of_day(&[22 * 60, 23 * 60 + 30, 30, 60]),
            Some(0),
            "the middle pair 23:30 and 00:30 averages across midnight"
        );
        assert_eq!(
            median_minute_of_day(&[22 * 60, 22 * 60 + 20, 23 * 60]),
            Some(22 * 60 + 20)
        );
    }

    /// Five nights around midnight, in UTC; the median is 23:30.
    async fn slept_around_midnight(sleep: &SleepState) {
        for time in [
            "2026-02-24T00:30:00Z",
            "2026-02-25T23:30:00Z",
            "2026-02-26T23:30:00Z",
            "2026-02-27T23:30:00Z",
            "2026-02-28T23:30:00Z",
        ] {
            sleep.db.record_sleep(at(time)).await.unwrap();
        }
    }

    #[tokio::test]
    async fn the_reminder_fires_at_the_median_time_once() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, bus) = state(dir.path()).await;
        let utc = TimeZone::UTC;
        assert!(
            !sleep
                .remind_if_due(at("2026-03-01T23:30:00Z"), &utc)
                .await
                .unwrap(),
            "no history, no reminder"
        );

        slept_around_midnight(&sleep).await;
        let mut events = bus.subscribe();
        assert!(
            !sleep
                .remind_if_due(at("2026-03-01T12:00:00Z"), &utc)
                .await
                .unwrap(),
            "not the median time"
        );
        assert!(
            sleep
                .remind_if_due(at("2026-03-01T23:30:00Z"), &utc)
                .await
                .unwrap()
        );
        assert!(events.recv().await.unwrap().contains(UiEventTopics::SLEEP));
        assert_eq!(sleep.status().await.reminders, 1);
        assert!(
            !sleep
                .remind_if_due(at("2026-03-01T23:45:00Z"), &utc)
                .await
                .unwrap(),
            "once per night"
        );
        assert!(
            sleep
                .remind_if_due(at("2026-03-02T23:35:00Z"), &utc)
                .await
                .unwrap(),
            "again the next night"
        );
    }

    #[tokio::test]
    async fn no_reminder_while_asleep() {
        let dir = tempfile::tempdir().expect("dir");
        let (sleep, _) = state(dir.path()).await;
        slept_around_midnight(&sleep).await;
        sleep.sleep("a", at("2026-03-01T23:00:00Z")).await.unwrap();
        assert!(
            !sleep
                .remind_if_due(at("2026-03-01T23:30:00Z"), &TimeZone::UTC)
                .await
                .unwrap()
        );
    }
}
