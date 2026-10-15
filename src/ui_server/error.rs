//! Shared error helpers for the UI server.

//!
//! A `5xx` endpoint error is logged once, by the skyzen runtime
//! (`skyzen_core::log_endpoint_error`, at `error` level with the request's
//! method, path and the error's whole `source()` chain), before its response
//! body is redacted. The helpers below keep that chain intact so the log
//! carries the underlying cause, not just the outermost message.

use std::fmt::{Debug, Display};

use serde::Serialize;
use skyzen::{Error as SkyError, StatusCode};

use super::types::{ApiErrorBody, ApiErrorCode};

/// A `500` carrying `err` and its `source()` chain.
pub(super) fn internal_error(err: impl std::error::Error + Send + Sync + 'static) -> SkyError {
    SkyError::new(err)
}

/// A `500` for a failure that has no underlying error value, only a message.
pub(super) fn internal_error_message(
    message: impl Display + Debug + Send + Sync + 'static,
) -> SkyError {
    SkyError::msg(message)
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

#[cfg(test)]
mod tests {
    use std::error::Error as StdError;
    use std::fmt;

    use skyzen::{ErrorChain, StatusCode};

    use super::internal_error;

    #[derive(Debug)]
    struct Outer(std::io::Error);

    impl fmt::Display for Outer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("failed to create session directory")
        }
    }

    impl StdError for Outer {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn internal_error_keeps_the_source_chain_for_the_log() {
        let error = internal_error(Outer(std::io::Error::other("disk full")));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            ErrorChain(&error).to_string(),
            "failed to create session directory: disk full"
        );
    }
}
