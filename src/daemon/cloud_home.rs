//! What a cloud home's daemon takes from the daemon fronting it.

use std::sync::Arc;

use skyzen::routing::Params;
use skyzen::utils::{Json, State};
use skyzen::{Error as SkyError, Result as SkyResult, StatusCode};

use super::auth::Fronted;
use super::error::bad_request_error;
use super::state::DaemonState;
use crate::cloud::access::{AccessCell, ProviderPush};

/// `PUT /api/cloud-home/providers/{id}`: the provider `id` as the fronting
/// daemon pushed it. Only the fronting daemon's token is admitted: a request
/// any other identity makes is refused. The configuration replaces the
/// provider's in memory; an access token replaces the one in the cell the
/// provider's models already read, so an idle agent keeps going on it.
pub(super) async fn push_provider(
    params: Params,
    fronted: Fronted,
    State(state): State<Arc<DaemonState>>,
    Json(push): Json<ProviderPush>,
) -> SkyResult<StatusCode> {
    if !fronted.0 {
        return Err(
            SkyError::msg("only the daemon fronting this cloud home pushes its providers")
                .set_status(StatusCode::FORBIDDEN),
        );
    }
    let id = params.get("id").map_err(bad_request_error)?.to_owned();
    let ProviderPush {
        mut instance,
        access,
    } = push;
    let provider_type = instance.resolve_type(&id);
    if let Some(token) = &access
        && token.provider_type() != provider_type
    {
        return Err(bad_request_error(format!(
            "provider '{id}' is {provider_type:?}, but the access token is for {:?}",
            token.provider_type()
        )));
    }
    state.update_config(|config| {
        let held = config
            .provider
            .get(&id)
            .and_then(|current| current.pushed_access.clone());
        instance.pushed_access = match (access, held) {
            (Some(token), Some(cell)) => {
                cell.replace(token);
                Some(cell)
            }
            (Some(token), None) => Some(AccessCell::holding(token)),
            (None, _) => None,
        };
        config.provider.insert(id, instance);
    });
    Ok(StatusCode::NO_CONTENT)
}
