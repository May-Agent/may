//! Shared error helpers for the UI server.

use serde::Serialize;
use skyzen::{Error as SkyError, StatusCode};

use super::types::{ApiErrorBody, ApiErrorCode};

pub(super) fn internal_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::INTERNAL_SERVER_ERROR)
}

pub(super) fn bad_request_error(err: impl std::fmt::Display) -> SkyError {
    SkyError::msg(err.to_string()).set_status(StatusCode::BAD_REQUEST)
}

/// Build a typed error response with a machine-readable error code.
///
/// Use this instead of [`SkyError`] when the frontend needs to distinguish error categories
/// programmatically (e.g. container preparing vs build failed) without string-matching.
pub(super) fn typed_error_response(
    status: StatusCode,
    code: ApiErrorCode,
    message: impl std::fmt::Display,
) -> skyzen::Response {
    let body = ApiErrorBody {
        code,
        error: message.to_string(),
    };
    let json = serde_json::to_vec(&body).unwrap_or_else(|_| {
        br#"{"code":"internal_error","error":"serialization failed"}"#.to_vec()
    });
    let mut response = skyzen::Response::new(skyzen::Body::from(json));
    *response.status_mut() = status;
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    response
}

/// Wrap a serializable value in a `200 OK` JSON response.
pub(super) fn json_ok_response<T: Serialize>(data: &T) -> Result<skyzen::Response, SkyError> {
    let json = serde_json::to_vec(data).map_err(internal_error)?;
    let mut response = skyzen::Response::new(skyzen::Body::from(json));
    response.headers_mut().insert(
        skyzen::header::CONTENT_TYPE,
        skyzen::header::HeaderValue::from_static("application/json"),
    );
    Ok(response)
}
