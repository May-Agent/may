//! Persisted network audit log row model.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, skyzen::FromRow)]
pub struct Model {
    pub id: i32,
    pub session_id: Option<String>,
    pub profile_id: Option<String>,
    pub source: String,
    pub mode: String,
    pub domain: String,
    pub port: i32,
    pub allowed: bool,
    pub reason: Option<String>,
    pub timestamp: String,
}
