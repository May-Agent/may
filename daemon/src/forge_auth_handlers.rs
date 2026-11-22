//! The forge-auth REST handlers: status, configure, logout and OAuth device
//! flow. The request/response wire types live in `may_client::daemon::forge_auth`.

pub use may_client::daemon::forge_auth::*;

use skyzen::Result as SkyResult;
use skyzen::utils::{Json, State};

use super::error::{bad_request_error, internal_error, json_ok_response};
use super::state::DaemonState;
use crate::forge::{ForgeKind, LiveForgeAuth};

