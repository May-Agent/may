//! Background tasks: auto-archive stale sessions, container auto-stop.

use std::time::Duration;

use crate::db::ChatDb;

/// Interval between auto-archive sweeps.
const AUTO_ARCHIVE_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Sessions older than this are candidates for auto-archiving.
const AUTO_ARCHIVE_CUTOFF: jiff::SignedDuration = jiff::SignedDuration::from_hours(48);

/// Spawn a background task that periodically archives stale sessions.
///
/// A session is considered stale if it is:
/// - Not pinned
/// - Not already archived
/// - `updated_at` is older than 48 hours
#[must_use] 
pub fn spawn_auto_archive(db: ChatDb) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(AUTO_ARCHIVE_INTERVAL);
        // First tick completes immediately; skip it to avoid archiving on startup.
        interval.tick().await;

        loop {
            interval.tick().await;

            let cutoff = jiff::Timestamp::now()
                .checked_sub(AUTO_ARCHIVE_CUTOFF)
                .expect("48h subtraction should not overflow")
                .to_string();

            match db.sessions_needing_archive(&cutoff).await {
                Ok(sessions) if sessions.is_empty() => {
                    tracing::debug!("Auto-archive sweep: no stale sessions");
                }
                Ok(sessions) => {
                    let count = sessions.len();
                    for session in &sessions {
                        if let Err(e) = db.archive_session(&session.id).await {
                            tracing::warn!(
                                session_id = %session.id,
                                error = %e,
                                "Failed to auto-archive session"
                            );
                        }
                    }
                    tracing::info!(count, "Auto-archived stale sessions");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Auto-archive sweep failed");
                }
            }
        }
    })
}
