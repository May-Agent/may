use base64::Engine;

pub use aither_browser::{
    BrowserAccessibilityNode, BrowserAccessibilityProperty, BrowserAccessibilitySnapshot,
    BrowserArtifactScreenshot, BrowserCapturedScreenshot, BrowserInteractiveElement,
    BrowserPageCapture, BrowserPageSnapshot, BrowserPageSummary, BrowserReadDetail,
    BrowserReadFormat, BrowserReadOptions, BrowserScreenshotMode, BrowserServiceRequest,
    BrowserServiceResponse, BrowserServiceStatus,
};

use crate::session::SessionPaths;

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
