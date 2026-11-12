//! Background self-update of release builds.
//!
//! The daemon checks GitHub releases every `[update] check_interval_hours`
//! while `[update] auto` is on, both read on every round so a change applies
//! at once. A dev build never runs this. An installed update does not
//! restart the daemon, which would end its turns: the next client replaces
//! it because its build id differs.

use std::sync::Arc;
use std::time::Duration;

use crate::update::{Channel, Outcome, update};

use super::state::DaemonState;

const SECONDS_PER_HOUR: u64 = 3600;

/// Starts the checker on release builds.
pub(super) fn spawn(state: Arc<DaemonState>) {
    if Channel::current() != Channel::Release {
        return;
    }
    state.tokio_handle.clone().spawn(async move {
        loop {
            if state.config.load().update.auto {
                crate::panic_boundary::round("auto update", check()).await;
            }
            let hours = state.config.load().update.check_interval_hours;
            async_io::Timer::after(Duration::from_secs(hours * SECONDS_PER_HOUR)).await;
        }
    });
}

async fn check() {
    match update(Channel::Release, |step| {
        tracing::debug!(step, "auto update");
    })
    .await
    {
        Ok(Outcome::UpToDate) => tracing::debug!("this release is the latest one"),
        Ok(Outcome::Updated { executable, to }) => {
            tracing::info!(executable = %executable.display(), %to, "updated May; the next client restarts the daemon onto it");
        }
        Err(error) => tracing::warn!(%error, "automatic update failed"),
    }
}
