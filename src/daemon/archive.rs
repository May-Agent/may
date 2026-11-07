//! Archiving of idle sessions.
//!
//! The daemon owns the archive: a session that stayed idle for longer than
//! `[sessions] recent_hours` leaves the session lists, and every change is
//! pushed to the clients on the `profile_sessions` event.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

use super::state::DaemonState;
use crate::db::{ChatDb, ChatDbError};
use crate::ui_event_bus::UiEventTopics;

/// Interval between sweeps; a changed window applies at the next one.
const SWEEP_INTERVAL: Duration = Duration::from_secs(30);

/// Sessions with background work, remembered with the last time they had
/// some, so the window counts from when the work ended.
#[derive(Debug, Default)]
struct IdleClock {
    last_busy: HashMap<String, Timestamp>,
}

impl IdleClock {
    /// Records the sessions busy at `now` and returns the ones that were
    /// busy since `cutoff`: they are not idle long enough to archive.
    fn held(
        &mut self,
        busy: BTreeSet<String>,
        now: Timestamp,
        cutoff: Timestamp,
    ) -> BTreeSet<String> {
        for id in busy {
            self.last_busy.insert(id, now);
        }
        self.last_busy.retain(|_, at| *at >= cutoff);
        self.last_busy.keys().cloned().collect()
    }
}

/// The instant before which an idle session is archived, `recent_hours`
/// before `now`.
fn cutoff(now: Timestamp, recent_hours: u64) -> Timestamp {
    let window = SignedDuration::from_hours(i64::try_from(recent_hours).expect("hours fit in i64"));
    now.checked_sub(window)
        .expect("the window does not underflow the timestamp range")
}

/// Archives the sessions that have been idle since before `cutoff`, except
/// `held`. Returns the archived session ids.
///
/// # Errors
/// Returns [`ChatDbError`] when the candidates cannot be listed.
pub(super) async fn archive_idle(
    db: &ChatDb,
    cutoff: Timestamp,
    held: &BTreeSet<String>,
) -> Result<Vec<String>, ChatDbError> {
    let mut archived = Vec::new();
    for session in db.sessions_needing_archive(&cutoff.to_string()).await? {
        if held.contains(&session.id) {
            continue;
        }
        match db.archive_session(&session.id).await {
            Ok(()) => archived.push(session.id),
            Err(error) => {
                tracing::warn!(session_id = %session.id, %error, "failed to auto-archive session");
            }
        }
    }
    Ok(archived)
}

/// Sweeps for idle sessions for as long as the daemon runs.
pub(super) fn spawn(state: Arc<DaemonState>) {
    state.tokio_handle.clone().spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        let mut clock = IdleClock::default();
        loop {
            interval.tick().await;
            let now = Timestamp::now();
            let recent_hours = state.config.load().sessions.recent_hours;
            let before = cutoff(now, recent_hours);
            let busy = state.session_background.busy_sessions().await;
            let held = clock.held(busy, now, before);
            match archive_idle(&state.db(), before, &held).await {
                Ok(archived) if archived.is_empty() => {}
                Ok(archived) => {
                    tracing::info!(count = archived.len(), "archived idle sessions");
                    state.notify_ui_events(UiEventTopics::PROFILE_SESSIONS);
                }
                Err(error) => tracing::warn!(%error, "archive sweep failed"),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{CreateProfileParams, ProfileEnvironmentKind};
    use crate::session::{
        PersistedAskUserRequest, PersistedPermissionRequest, PersistedPermissionRequestPayload,
        SessionRuntimeState,
    };

    async fn db_with_profile() -> (tempfile::TempDir, ChatDb) {
        let dir = tempfile::tempdir().expect("temp dir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db opens");
        db.create_profile(CreateProfileParams {
            id: "p",
            name: "P",
            environment: ProfileEnvironmentKind::Local,
            slug: None,
            icon: None,
            persona: None,
            preferences: None,
            location_aware: false,
        })
        .await
        .expect("profile");
        (dir, db)
    }

    async fn add(db: &ChatDb, id: &str) {
        db.create_session_in_profile(id, Some(id), Some("m"), Some("p"))
            .await
            .expect("session");
    }

    /// A cutoff after every write of the test: all sessions count as idle
    /// for longer than the window.
    fn past_window() -> Timestamp {
        Timestamp::now()
            .checked_add(SignedDuration::from_hours(1))
            .expect("in range")
    }

    async fn is_archived(db: &ChatDb, id: &str) -> bool {
        db.get_session(id)
            .await
            .expect("lookup")
            .expect("exists")
            .archived
    }

    #[tokio::test]
    async fn an_idle_session_is_archived_and_the_work_in_flight_is_not() {
        let (_dir, db) = db_with_profile().await;
        for id in [
            "idle",
            "running",
            "background",
            "permission",
            "pinned",
            "asking",
        ] {
            add(&db, id).await;
        }
        db.set_session_runtime_state("running", SessionRuntimeState::Running)
            .await
            .expect("state");
        db.set_waiting_permission(
            "permission",
            Some(PersistedPermissionRequest {
                id: "r".into(),
                request: PersistedPermissionRequestPayload {
                    mode: crate::PermissionRequestMode::Unsafe,
                    script: "ls".into(),
                    session_id: Some("permission".into()),
                    agent_id: None,
                },
            }),
        )
        .await
        .expect("permission");
        db.set_waiting_ask_user(
            "asking",
            Some(PersistedAskUserRequest {
                id: "q".into(),
                questions: Vec::new(),
            }),
        )
        .await
        .expect("ask_user");
        db.pin_session("pinned").await.expect("pin");

        let held = BTreeSet::from(["background".to_string()]);
        let archived = archive_idle(&db, past_window(), &held)
            .await
            .expect("sweep");

        assert!(is_archived(&db, "idle").await);
        assert!(
            is_archived(&db, "asking").await,
            "an open question archives"
        );
        assert!(!is_archived(&db, "running").await);
        assert!(!is_archived(&db, "background").await);
        assert!(!is_archived(&db, "permission").await);
        assert!(!is_archived(&db, "pinned").await);
        assert_eq!(archived.len(), 2);
    }

    #[tokio::test]
    async fn a_session_idle_for_less_than_the_window_stays() {
        let (_dir, db) = db_with_profile().await;
        add(&db, "fresh").await;
        let cutoff = Timestamp::now()
            .checked_sub(SignedDuration::from_hours(48))
            .expect("in range");
        let archived = archive_idle(&db, cutoff, &BTreeSet::new())
            .await
            .expect("sweep");
        assert_eq!(archived, Vec::<String>::new());
        assert!(!is_archived(&db, "fresh").await);
    }

    #[tokio::test]
    async fn opening_an_archived_session_makes_it_recent_again() {
        let (_dir, db) = db_with_profile().await;
        add(&db, "s").await;
        archive_idle(&db, past_window(), &BTreeSet::new())
            .await
            .expect("sweep");
        assert!(is_archived(&db, "s").await);

        db.unarchive_session("s").await.expect("open");
        assert!(!is_archived(&db, "s").await);
        // Its window restarted: a cutoff of 48 hours ago leaves it listed.
        let cutoff = Timestamp::now()
            .checked_sub(SignedDuration::from_hours(48))
            .expect("in range");
        assert_eq!(
            archive_idle(&db, cutoff, &BTreeSet::new())
                .await
                .expect("sweep"),
            Vec::<String>::new()
        );
    }

    #[test]
    fn background_work_holds_a_session_until_the_window_passes_its_end() {
        let t0: Timestamp = "2026-01-01T00:00:00Z".parse().expect("timestamp");
        let hour = SignedDuration::from_hours(1);
        let mut clock = IdleClock::default();
        let busy = BTreeSet::from(["a".to_string()]);

        let held = clock.held(busy, t0, t0.checked_sub(hour).expect("in range"));
        assert!(held.contains("a"));

        // The work ended; one window later the session is released.
        let later = t0.checked_add(hour).expect("in range");
        let held = clock.held(
            BTreeSet::new(),
            later,
            t0.checked_sub(SignedDuration::from_secs(1))
                .expect("in range"),
        );
        assert!(
            held.contains("a"),
            "idle for exactly the window is not enough"
        );
        let held = clock.held(BTreeSet::new(), later, later);
        assert!(held.is_empty());
    }

    #[tokio::test]
    async fn a_changed_window_applies_to_the_next_sweep() {
        let (_dir, db) = db_with_profile().await;
        add(&db, "s").await;
        let two_hours_on = Timestamp::now()
            .checked_add(SignedDuration::from_hours(2))
            .expect("in range");

        let kept = archive_idle(&db, cutoff(two_hours_on, 48), &BTreeSet::new())
            .await
            .expect("sweep");
        assert_eq!(kept, Vec::<String>::new());
        let archived = archive_idle(&db, cutoff(two_hours_on, 1), &BTreeSet::new())
            .await
            .expect("sweep");
        assert_eq!(archived, ["s"]);
    }
}
