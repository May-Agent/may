#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserServiceStatus {
    pub running: bool,
    pub cdp_endpoint: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPageSnapshot {
    pub url: String,
    pub title: String,
    pub main_heading: Option<String>,
    pub headings: Vec<String>,
    pub text_excerpt: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum BrowserServiceRequest {
    Status,
    EnsureStarted,
    Stop,
    Read { url: String },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserServiceResponse {
    Ok { payload: BrowserServiceStatus },
    ReadOk { payload: BrowserPageSnapshot },
    Stopped,
    Error { message: String },
}
