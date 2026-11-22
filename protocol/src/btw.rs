//! The `/btw` mailbox and side-request responses.

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/context-dump`.
pub struct ContextDumpResponse {
    /// Where the dump was written.
    pub path: String,
    /// Messages in the dump.
    pub message_count: usize,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/compact`.
pub struct CompactResponse {
    /// Whether anything was compacted.
    pub compacted: bool,
    /// Messages folded into the summary.
    pub messages_compacted: usize,
    /// Messages kept verbatim.
    pub messages_remaining: usize,
    /// The summary.
    pub summary: Option<String>,
}

/// One message of a `/btw` request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BtwMessage {
    /// `system`, `user` or `assistant`.
    pub role: String,
    /// Message text.
    pub content: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Body of `POST /api/sessions/{id}/btw`.
pub struct BtwRequest {
    /// The side question with the context it is asked in, oldest first.
    pub messages: Vec<BtwMessage>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
/// Answer of `POST /api/sessions/{id}/btw`.
pub struct BtwResponse {
    /// The answer.
    pub answer: String,
}
