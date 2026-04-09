use base64::Engine;
use schemars::JsonSchema;

use crate::session::SessionPaths;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserServiceStatus {
    pub running: bool,
    pub cdp_endpoint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserReadDetail {
    Brief,
    Standard,
    Full,
}

impl Default for BrowserReadDetail {
    fn default() -> Self {
        Self::Standard
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserScreenshotMode {
    Off,
    Viewport,
    FullPage,
}

impl Default for BrowserScreenshotMode {
    fn default() -> Self {
        Self::Viewport
    }
}

#[derive(
    Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize, JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "kebab-case")]
pub enum BrowserReadFormat {
    #[default]
    Json,
}

#[derive(
    Debug, Clone, Default, serde::Serialize, serde::Deserialize, JsonSchema, PartialEq, Eq,
)]
#[serde(rename_all = "camelCase")]
pub struct BrowserReadOptions {
    #[serde(default)]
    pub detail: BrowserReadDetail,
    #[serde(default)]
    pub screenshot: BrowserScreenshotMode,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserInteractiveElement {
    pub role: String,
    pub tag_name: String,
    pub text: Option<String>,
    pub accessible_name: Option<String>,
    pub href: Option<String>,
    pub input_type: Option<String>,
    pub disabled: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAccessibilityProperty {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAccessibilityNode {
    pub role: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub value: Option<String>,
    pub depth: usize,
    pub child_count: usize,
    pub ignored: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub properties: Vec<BrowserAccessibilityProperty>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserAccessibilitySnapshot {
    pub total_nodes: usize,
    pub interesting_nodes: usize,
    pub interactive_nodes: usize,
    pub heading_nodes: usize,
    pub landmark_nodes: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<BrowserAccessibilityNode>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserCapturedScreenshot {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub data_base64: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserArtifactScreenshot {
    pub mime_type: String,
    pub width: u32,
    pub height: u32,
    pub artifact_path: String,
    pub api_path: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPageSummary {
    pub url: String,
    pub title: String,
    pub main_heading: Option<String>,
    pub headings: Vec<String>,
    pub text_excerpt: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub interactive_elements: Vec<BrowserInteractiveElement>,
    pub accessibility: BrowserAccessibilitySnapshot,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPageCapture {
    pub page: BrowserPageSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<BrowserCapturedScreenshot>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserPageSnapshot {
    #[serde(flatten)]
    pub page: BrowserPageSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<BrowserArtifactScreenshot>,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserCapturePersistError {
    #[error("failed to prepare browser artifact directory: {source}")]
    CreateArtifactDirectory {
        #[source]
        source: std::io::Error,
    },
    #[error("browser screenshot used an unsupported MIME type: {mime_type}")]
    UnsupportedScreenshotMimeType { mime_type: String },
    #[error("failed to decode browser screenshot payload: {source}")]
    DecodeScreenshot {
        #[source]
        source: base64::DecodeError,
    },
    #[error("failed to write browser artifact: {source}")]
    WriteArtifact {
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "command", rename_all = "kebab-case")]
pub enum BrowserServiceRequest {
    Status,
    EnsureStarted,
    Stop,
    Read {
        url: String,
        #[serde(default)]
        options: BrowserReadOptions,
    },
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BrowserServiceResponse {
    Ok { payload: BrowserServiceStatus },
    ReadOk { payload: BrowserPageCapture },
    Stopped,
    Error { message: String },
}

pub async fn persist_browser_page_capture(
    session_id: &str,
    session_paths: &SessionPaths,
    capture: BrowserPageCapture,
) -> Result<BrowserPageSnapshot, BrowserCapturePersistError> {
    let screenshot = match capture.screenshot {
        Some(screenshot) => {
            Some(persist_browser_screenshot(session_id, session_paths, screenshot).await?)
        }
        None => None,
    };

    Ok(BrowserPageSnapshot {
        page: capture.page,
        screenshot,
    })
}

async fn persist_browser_screenshot(
    session_id: &str,
    session_paths: &SessionPaths,
    screenshot: BrowserCapturedScreenshot,
) -> Result<BrowserArtifactScreenshot, BrowserCapturePersistError> {
    let extension = screenshot_extension(screenshot.mime_type.as_str())?;
    let browser_artifacts_dir = session_paths.artifacts_dir().join("browser");
    tokio::fs::create_dir_all(&browser_artifacts_dir)
        .await
        .map_err(|source| BrowserCapturePersistError::CreateArtifactDirectory { source })?;

    let filename = format!("snapshot-{}.{}", uuid::Uuid::new_v4(), extension);
    let relative_artifact_path = format!("browser/{filename}");
    let absolute_artifact_path = browser_artifacts_dir.join(&filename);
    let screenshot_bytes = base64::engine::general_purpose::STANDARD
        .decode(screenshot.data_base64.as_bytes())
        .map_err(|source| BrowserCapturePersistError::DecodeScreenshot { source })?;
    tokio::fs::write(&absolute_artifact_path, screenshot_bytes)
        .await
        .map_err(|source| BrowserCapturePersistError::WriteArtifact { source })?;

    Ok(BrowserArtifactScreenshot {
        mime_type: screenshot.mime_type,
        width: screenshot.width,
        height: screenshot.height,
        artifact_path: format!("artifacts/{relative_artifact_path}"),
        api_path: format!(
            "/api/sessions/{session_id}/artifacts/preview?path={relative_artifact_path}"
        ),
    })
}

fn screenshot_extension(mime_type: &str) -> Result<&'static str, BrowserCapturePersistError> {
    match mime_type {
        "image/png" => Ok("png"),
        "image/jpeg" => Ok("jpg"),
        "image/webp" => Ok("webp"),
        _ => Err(BrowserCapturePersistError::UnsupportedScreenshotMimeType {
            mime_type: mime_type.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::{
        BrowserAccessibilitySnapshot, BrowserCapturedScreenshot, BrowserPageCapture,
        BrowserPageSummary, persist_browser_page_capture,
    };
    use crate::session::SessionPaths;

    const PNG_1X1_BASE64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO5q3XYAAAAASUVORK5CYII=";

    #[tokio::test]
    async fn persist_browser_page_capture_writes_session_artifact() {
        let root = tempdir().expect("tempdir must be created");
        let session_paths = SessionPaths::new(root.path().join("sessions").join("session-1"));
        tokio::fs::create_dir_all(session_paths.artifacts_dir())
            .await
            .expect("artifacts dir must be created");

        let capture = BrowserPageCapture {
            page: BrowserPageSummary {
                url: "https://example.com".to_string(),
                title: "Example".to_string(),
                main_heading: Some("Example Domain".to_string()),
                headings: vec!["Example Domain".to_string()],
                text_excerpt: "Example Domain".to_string(),
                interactive_elements: Vec::new(),
                accessibility: BrowserAccessibilitySnapshot {
                    total_nodes: 1,
                    interesting_nodes: 1,
                    interactive_nodes: 0,
                    heading_nodes: 1,
                    landmark_nodes: 0,
                    nodes: Vec::new(),
                },
            },
            screenshot: Some(BrowserCapturedScreenshot {
                mime_type: "image/png".to_string(),
                width: 1,
                height: 1,
                data_base64: PNG_1X1_BASE64.to_string(),
            }),
        };

        let snapshot = persist_browser_page_capture("session-1", &session_paths, capture)
            .await
            .expect("browser capture should persist");
        let screenshot = snapshot
            .screenshot
            .expect("screenshot artifact metadata should exist");

        assert!(
            screenshot
                .artifact_path
                .starts_with("artifacts/browser/snapshot-")
        );
        assert_eq!(
            screenshot.api_path,
            format!(
                "/api/sessions/session-1/artifacts/preview?path={}",
                screenshot
                    .artifact_path
                    .strip_prefix("artifacts/")
                    .expect("artifact path must be prefixed with artifacts/")
            )
        );
        let artifact_path = session_paths.root().join(&screenshot.artifact_path);
        assert!(
            tokio::fs::metadata(artifact_path).await.is_ok(),
            "artifact file must exist"
        );
    }
}
