//! `/api/sessions/{id}/ids` and `/api/sessions/{id}/stored-ids`: a session's
//! pool leased by a process that is not the daemon (`may exec`).
//!
//! The daemon owns every session's pool. A lease taken here is held by the
//! connection that asked for it ([`ConnectionIds`]) and released when that
//! connection releases it or closes, so a client that exits or crashes
//! leaves no word leased. A lease the client keeps for a stored id moves to
//! the session's stored leases, as the daemon's own stored ids do.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aither_core::IdLease;
use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use super::error::{bad_request_error, internal_error};
use super::state::DaemonState;

/// The leases one connection holds, by session and word. Every request of
/// the connection carries it; the leases go when the connection closes.
#[derive(Debug, Clone, Default)]
pub(super) struct ConnectionIds(Arc<Mutex<HashMap<(String, String), IdLease>>>);

impl ConnectionIds {
    fn leases(&self) -> std::sync::MutexGuard<'_, HashMap<(String, String), IdLease>> {
        self.0.lock().expect("connection id leases lock poisoned")
    }

    fn take(&self, session_id: &str, word: &str) -> SkyResult<IdLease> {
        self.leases()
            .remove(&(session_id.to_string(), word.to_string()))
            .ok_or_else(|| {
                SkyError::msg(format!(
                    "this connection holds no lease of '{word}' in session {session_id}"
                ))
                .set_status(StatusCode::NOT_FOUND)
            })
    }
}

#[skyzen::error(status = StatusCode::INTERNAL_SERVER_ERROR)]
pub(super) enum ConnectionIdsError {
    #[error("the request arrived on no connection that holds id leases")]
    Missing,
}

impl skyzen::extract::Extractor for ConnectionIds {
    type Error = ConnectionIdsError;

    fn extract(
        request: &mut skyzen::Request,
    ) -> impl std::future::Future<Output = Result<Self, Self::Error>> + Send {
        std::future::ready(
            request
                .extensions()
                .get::<Self>()
                .cloned()
                .ok_or(ConnectionIdsError::Missing),
        )
    }
}

/// `POST /api/sessions/{id}/ids` request body.
#[derive(Debug, serde::Deserialize, skyzen::ToSchema)]
pub struct LeaseIdRequest {
    /// The word to claim; any free word when absent.
    word: Option<String>,
}

/// `POST /api/sessions/{id}/ids` response body.
#[derive(Debug, serde::Serialize, skyzen::ToSchema)]
pub struct LeasedId {
    /// The word the connection now holds.
    word: String,
}

fn conflict(message: impl std::fmt::Display) -> SkyError {
    SkyError::msg(message.to_string()).set_status(StatusCode::CONFLICT)
}

fn param(params: &Params, name: &str) -> SkyResult<String> {
    Ok(params.get(name).map_err(bad_request_error)?.to_string())
}

/// `POST /api/sessions/{id}/ids` — leases a word of the session's pool for
/// the connection: `word` itself, or any free word. `409` when it is leased
/// or every word is.
pub(super) async fn lease_session_id(
    params: Params,
    connection: ConnectionIds,
    Json(request): Json<LeaseIdRequest>,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<Json<LeasedId>> {
    let session_id = param(&params, "id")?;
    let db = state.db();
    if db
        .get_session(&session_id)
        .await
        .map_err(internal_error)?
        .is_none()
    {
        return Err(super::sessions::session_not_found_error(&session_id));
    }
    let ids = db.session_ids(&session_id).await.map_err(internal_error)?;
    let lease = match request.word {
        Some(word) => ids.allocator().claim(&word).map_err(conflict)?,
        None => ids.lease().map_err(conflict)?,
    };
    let word = lease.to_string();
    connection
        .leases()
        .insert((session_id, word.clone()), lease);
    Ok(Json(LeasedId { word }))
}

/// `DELETE /api/sessions/{id}/ids/{word}` — releases the connection's lease
/// of `word`.
pub(super) async fn release_session_id(
    params: Params,
    connection: ConnectionIds,
) -> SkyResult<StatusCode> {
    let lease = connection.take(&param(&params, "id")?, &param(&params, "word")?)?;
    drop(lease);
    Ok(StatusCode::NO_CONTENT)
}

/// `POST /api/sessions/{id}/ids/{word}/keep` — moves the connection's lease
/// of `word` to the session's stored leases: the database now keeps an id
/// named by it.
pub(super) async fn keep_session_id(
    params: Params,
    connection: ConnectionIds,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<StatusCode> {
    let session_id = param(&params, "id")?;
    let lease = connection.take(&session_id, &param(&params, "word")?)?;
    state
        .db()
        .session_ids(&session_id)
        .await
        .map_err(internal_error)?
        .keep(lease);
    Ok(StatusCode::NO_CONTENT)
}

/// `DELETE /api/sessions/{id}/stored-ids/{word}` — ends the stored lease of
/// `word`: the id it named left the database.
pub(super) async fn release_stored_session_id(
    params: Params,
    State(state): State<Arc<DaemonState>>,
) -> SkyResult<StatusCode> {
    let session_id = param(&params, "id")?;
    state
        .db()
        .session_ids(&session_id)
        .await
        .map_err(internal_error)?
        .release(&param(&params, "word")?);
    Ok(StatusCode::NO_CONTENT)
}
