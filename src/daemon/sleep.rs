//! `/api/sleep`: the user's global sleep state, and the health reminder.

use std::sync::Arc;
use std::time::Duration;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};
use tokio::time::MissedTickBehavior;

use super::chat::deliver_user_message;
use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;
use super::types::ChatRequest;
use crate::sleep::SleepStatus;

/// What the turn `/sleep` starts carries as the user's message.
const SLEEP_COMMAND: &str = "/sleep";

/// How often the reminder checks the clock.
const REMINDER_TICK: Duration = Duration::from_secs(60);

/// Title and body of the health reminder, shown to the user as it is.
const REMINDER_TITLE: &str = "Time to rest";
const REMINDER_BODY: &str =
    "It is about when you usually go to sleep. Run /sleep and get some rest.";

/// `POST /api/sleep` payload.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct SleepRequest {
    /// The session `/sleep` was run in.
    pub(crate) session_id: String,
}

/// `GET /api/sleep`: the current state.
pub(super) async fn get_sleep(State(state): State<Arc<DaemonState>>) -> Json<SleepStatus> {
    Json(state.sleep.status().await)
}

/// `POST /api/sleep`: records the run, marks the user asleep for every
/// session and delivers `/sleep` to the session the command ran in, whose
/// agent wishes the user good night: a busy session takes it as its running
/// turn can. Running it while asleep only records it.
pub(super) async fn start_sleep(
    Json(request): Json<SleepRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SleepStatus>> {
    state
        .db()
        .get_session(&request.session_id)
        .await
        .map_err(internal_error)?
        .ok_or_else(|| bad_request_error("session not found"))?;
    let changed = state
        .sleep
        .sleep(&request.session_id, jiff::Timestamp::now())
        .await
        .map_err(internal_error)?;
    if changed {
        deliver_user_message(
            &state,
            ChatRequest {
                session_id: request.session_id,
                content: SLEEP_COMMAND.to_owned(),
                attachments: Vec::new(),
                hidden_context_xml: None,
                delivery_mode: crate::session::PendingUserMessageMode::default(),
                pending_entry_id: None,
            },
        )
        .await?;
    }
    Ok(Json(state.sleep.status().await))
}

/// `DELETE /api/sleep` (`/sleep off`): ends the state for every session.
pub(super) async fn end_sleep(
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<SleepStatus>> {
    state
        .sleep
        .wake(jiff::Timestamp::now())
        .await
        .map_err(internal_error)?;
    Ok(Json(state.sleep.status().await))
}

/// Raises the health reminder for the daemon's lifetime: clients show it
/// from the `sleep` event, and subscribed browsers get a web push. It is
/// the harness talking to the user and never enters an agent's context.
/// `[sleep] reminder` is read on every tick, so switching it off applies
/// live.
pub(super) fn spawn_reminders(state: &Arc<DaemonState>) {
    let state = Arc::clone(state);
    let handle = state.tokio_handle.clone();
    handle.spawn(async move {
        let mut tick = tokio::time::interval(REMINDER_TICK);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            crate::panic_boundary::round("sleep reminder", remind_once(&state)).await;
        }
    });
}

/// One check of the reminder.
async fn remind_once(state: &DaemonState) {
    let config = state.config.load();
    if !config.sleep.reminder {
        return;
    }
    let zone = match jiff::tz::TimeZone::get(config.time.user_timezone().as_str()) {
        Ok(zone) => zone,
        Err(error) => {
            tracing::error!(%error, "sleep reminder: the configured timezone is unknown");
            return;
        }
    };
    match state
        .sleep
        .remind_if_due(jiff::Timestamp::now(), &zone)
        .await
    {
        Ok(true) => {
            if let Err(error) = state
                .notification_service
                .send(REMINDER_TITLE, REMINDER_BODY)
                .await
            {
                tracing::warn!(%error, "sleep reminder: the web push failed");
            }
        }
        Ok(false) => {}
        Err(error) => tracing::error!(%error, "sleep reminder failed"),
    }
}
