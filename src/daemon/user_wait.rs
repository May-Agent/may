//! Turns each popup that waits on the user into a notification.

use std::sync::Arc;

use tokio::sync::broadcast::error::RecvError;

use super::state::DaemonState;

/// Notify the user of every popup the sessions announce, for as long as the
/// daemon runs.
pub(super) fn spawn_notifier(state: &Arc<DaemonState>) {
    let mut waits = state.runtime_service.subscribe_user_waits();
    let notifications = state.notification_service.clone();
    state.tokio_handle.spawn(async move {
        loop {
            match waits.recv().await {
                Ok(wait) => {
                    crate::panic_boundary::round(
                        "user wait notifier",
                        notifications.notify_user_wait(&wait),
                    )
                    .await;
                }
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(
                        skipped,
                        "user wait announcements lagged; some popups were not notified"
                    );
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}
