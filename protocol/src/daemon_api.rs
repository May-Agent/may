//! Constants of the daemon's client-facing API.

/// The status of a question that is no longer open.
///
/// Withdrawn or already answered; clients tell it apart from a failure to
/// store the answer, a `5xx`, and from an answer that does not fit the
/// question, a `400`.
pub const ASK_USER_NOT_OPEN: skyzen::StatusCode = skyzen::StatusCode::NOT_FOUND;

/// TCP port the daemon serves the API and web UI on by default.
pub const DEFAULT_PORT: u16 = 8787;

/// Path of the session endpoint `tail` (such as `/stop`) under
/// `/api/sessions/{session_id}`, with `session_id` validated and URL-encoded
/// as a single path segment.
#[must_use]
pub fn session_path(session_id: &str, tail: &str) -> String {
    let encoded_id = encode_path_segment(session_id);
    let mut path = String::with_capacity(14 + encoded_id.len() + tail.len());
    path.push_str("/api/sessions/");
    path.push_str(&encoded_id);
    path.push_str(tail);
    path
}

/// Percent-encodes a single URL path segment so reserved characters (`/`, `?`,
/// `#`, `%`, spaces, control bytes, non-ASCII) and dot-traversal segments
/// (`.` / `..`) never alter route structure or fail URI parsing.
#[must_use]
pub fn encode_path_segment(segment: &str) -> String {
    use std::fmt::Write as _;

    if segment == "." {
        return "%2E".to_string();
    }
    if segment == ".." {
        return "%2E%2E".to_string();
    }
    let mut encoded = String::with_capacity(segment.len());
    for &byte in segment.as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}
