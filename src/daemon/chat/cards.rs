//! The cards of terminal calls that ran one of May's commands: stored per
//! call, published to every client of the session, and joined with what
//! happened to them since (an `ask_user` answer) when history is served.

use std::sync::Arc;

use serde::Serialize;

use crate::agent::command_cards::{CardUpdate, CommandCards};
use crate::command_reading::Card;
use crate::daemon::state::DaemonState;

use super::SessionTurnFrame;
use super::hub::TurnHub;

/// The `tool_card` event of the web UI's turn stream.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCardEvent<'a> {
    turn_id: &'a str,
    id: &'a str,
    card: Option<&'a Card>,
}

/// Publishes the card of `call_id` to the session's clients: the web UI as a
/// turn event, the TUI as an agent frame.
pub(super) fn publish(
    hub: &TurnHub,
    session_id: &str,
    turn_id: &str,
    call_id: &str,
    card: Option<&Card>,
) {
    let turn = SessionTurnFrame {
        session_id,
        event: "tool_card",
        data: &ToolCardEvent {
            turn_id,
            id: call_id,
            card,
        },
    };
    match serde_json::to_string(&turn) {
        Ok(json) => hub.publish(session_id, "session_turn", json.into()),
        Err(error) => tracing::error!(%error, "failed to encode a tool card"),
    }
    let frame = crate::client::SessionAgentFrame {
        session_id: session_id.to_owned(),
        turn_id: turn_id.to_owned(),
        at: jiff::Timestamp::now(),
        body: crate::client::SessionAgentBody::ToolCard {
            call_id: call_id.to_owned(),
            card: card.cloned(),
        },
    };
    match serde_json::to_string(&frame) {
        Ok(json) => hub.publish(session_id, "session_agent", json.into()),
        Err(error) => tracing::error!(%error, "failed to encode a tool card frame"),
    }
}

/// Stores `card` as the card of `call_id`, or removes it.
pub(super) async fn store(
    db: &crate::db::ChatDb,
    session_id: &str,
    call_id: &str,
    card: Option<&Card>,
) {
    let stored = match card {
        Some(card) => db.set_tool_card(session_id, call_id, card).await,
        None => db.clear_tool_card(session_id, call_id).await,
    };
    if let Err(error) = stored {
        tracing::warn!(%error, call_id, "failed to store a tool card");
    }
}

/// Stores and publishes the updates the session's commands report, for as
/// long as the daemon runs. The first call for a session starts the reader.
pub(super) fn forward_updates(state: &Arc<DaemonState>, session_id: &str, cards: &CommandCards) {
    let Some(updates) = cards.claim_updates() else {
        return;
    };
    let state = state.clone();
    let session_id = session_id.to_owned();
    let tokio_handle = state.tokio_handle.clone();
    tokio_handle.spawn(async move {
        while let Ok(CardUpdate { call_id, card }) = updates.recv().await {
            store(&state.db(), &session_id, &call_id, card.as_ref()).await;
            publish(&state.turn_hub, &session_id, "", &call_id, card.as_ref());
        }
    });
}

/// The card with the answers committed for its question, when it asks one.
pub async fn with_answers(db: &crate::db::ChatDb, card: Card) -> Card {
    let Card::AskUser {
        request_id,
        answers: None,
        ..
    } = &card
    else {
        return card;
    };
    match db.committed_ask_user_answers(request_id).await {
        Ok(Some(committed)) => card.answered(&committed),
        Ok(None) => card,
        Err(error) => {
            tracing::warn!(%error, "failed to read the answers of an ask_user card");
            card
        }
    }
}

/// Tells the session's clients the question `request_id` was answered, so
/// its card shows the answer.
pub async fn announce_answer(state: &DaemonState, session_id: &str, request_id: &str) {
    let db = state.db();
    let found = match db.ask_user_card(session_id, request_id).await {
        Ok(found) => found,
        Err(error) => {
            tracing::warn!(%error, "failed to look up the card of an answered question");
            return;
        }
    };
    if let Some((call_id, card)) = found {
        let card = with_answers(&db, card).await;
        publish(&state.turn_hub, session_id, "", &call_id, Some(&card));
    }
}
