//! The `/import-claude` / `/import-codex` request and response.

use serde::{Deserialize, Serialize};

/// `POST /api/rules/import` body.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RulesImportRequest {
    /// The agent to import from: `claude` or `codex`.
    pub source: String,
    /// Profile whose skills receive the copies and whose session imports the
    /// rules; the default profile when absent.
    #[serde(default)]
    pub profile_id: Option<String>,
}

/// `POST /api/rules/import` answer.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RulesImportResponse {
    /// The session importing the rules; `None` when the agent has no global
    /// rules file, so only skills were copied.
    pub session_id: Option<String>,
    /// The agent's global rules file.
    pub rules_file: String,
    /// Skills copied into May.
    pub imported_skills: Vec<String>,
    /// Skills left out because May already has one by that name.
    pub skipped_skills: Vec<String>,
}
