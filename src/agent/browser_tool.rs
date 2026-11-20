//! Managed browser command adapter.
//!
//! Wraps [`aither_browser`] so browser screenshots flow through the IPC
//! registry as structured [`RecordedCallImage`] metadata rather than prose
//! parsing.

use std::borrow::Cow;
use std::path::{Component, Path, PathBuf};

use aither_core::llm::{IntoToolResult, Tool, ToolContext, ToolResult};

#[allow(
    unused_imports,
    reason = "re-exported for callers of agent::browser_tool"
)]
pub use aither_browser::{
    BrowserAccessibilityNode, BrowserAccessibilityProperty, BrowserAccessibilitySnapshot,
    BrowserArgs, BrowserArtifactScreenshot, BrowserBroker, BrowserCapturedScreenshot,
    BrowserInteractiveElement, BrowserPageCapture, BrowserPageSnapshot, BrowserPageSummary,
    BrowserQueue, BrowserReadDetail, BrowserReadOptions, BrowserRequest, BrowserResult,
    BrowserScreenshotMode, BrowserServiceRequest, BrowserServiceResponse, BrowserServiceStatus,
    BrowserToolError, BrowserToolOutput, channel,
};

use crate::ipc::{IpcImageTool, RecordedCallImage};

/// Result of a [`BrowserTool`] invocation pairing the rendered output with
/// any structured screenshot artifacts produced by the browser service.
#[derive(Debug, Clone)]
pub struct BrowserCommandOutput {
    output: BrowserToolOutput,
    images: Vec<RecordedCallImage>,
}

impl IntoToolResult for BrowserCommandOutput {
    fn into_tool_result(self) -> aither_core::Result<ToolResult> {
        self.output.into_tool_result()
    }
}

/// May's `browser` IPC command: calls the host browser broker and reports
/// screenshot artifacts as structured [`RecordedCallImage`] entries.
#[derive(Debug, Clone)]
pub struct BrowserTool {
    session_id: String,
    broker: Option<BrowserBroker>,
    sandbox_dir: Option<PathBuf>,
}

impl BrowserTool {
    /// Creates a browser tool bound to a session and optional broker.
    #[must_use]
    pub const fn new(session_id: String, broker: Option<BrowserBroker>) -> Self {
        Self {
            session_id,
            broker,
            sandbox_dir: None,
        }
    }

    /// Resolves relative screenshot artifact paths against `sandbox_dir`.
    #[must_use]
    pub fn with_sandbox_dir(mut self, sandbox_dir: PathBuf) -> Self {
        self.sandbox_dir = Some(sandbox_dir);
        self
    }

    fn structured_screenshots(&self, result: &BrowserResult) -> Vec<RecordedCallImage> {
        let Some(screenshot) = result.page.as_ref().and_then(|page| page.screenshot.as_ref())
        else {
            return Vec::new();
        };
        if !screenshot.mime_type.starts_with("image/") {
            return Vec::new();
        }
        let artifact = Path::new(screenshot.artifact_path.trim());
        if artifact.as_os_str().is_empty()
            || artifact
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Vec::new();
        }
        let path = if artifact.is_absolute() {
            if self.sandbox_dir.is_some() {
                return Vec::new();
            }
            artifact.to_path_buf()
        } else if let Some(sandbox_dir) = &self.sandbox_dir {
            sandbox_dir.join(artifact)
        } else {
            artifact.to_path_buf()
        };
        vec![RecordedCallImage {
            path,
            media_type: screenshot.mime_type.clone(),
            source: "browser screenshot".to_owned(),
        }]
    }
}

impl Tool for BrowserTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("browser")
    }

    type Arguments = BrowserArgs;
    type Res = BrowserCommandOutput;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        let Some(broker) = self.broker.as_ref() else {
            return Ok(BrowserCommandOutput {
                output: BrowserToolOutput::from(Err(BrowserToolError::UnavailableInRuntime)),
                images: Vec::new(),
            });
        };
        let response = broker
            .request(BrowserRequest {
                session_id: self.session_id.clone(),
                args,
            })
            .await?;
        let images = response.as_ref().map_or_else(
            |_| Vec::new(),
            |result| self.structured_screenshots(result),
        );
        Ok(BrowserCommandOutput {
            output: BrowserToolOutput::from(response),
            images,
        })
    }
}

impl IpcImageTool for BrowserTool {
    fn images(result: &Self::Res) -> &[RecordedCallImage] {
        &result.images
    }
}
