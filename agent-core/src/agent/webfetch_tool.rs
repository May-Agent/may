//! The `webfetch` command: aither's fetcher under May's description, which
//! says how to read only part of a page.

use std::borrow::Cow;

use aither_agent::webfetch::{WebFetchArgs, WebFetchTool};
use aither_core::llm::{Tool, ToolContext, ToolResult};

const WEBFETCH_DESCRIPTION: &str = include_str!("../texts/webfetch_description.txt");

/// [`WebFetchTool`] with May's description.
#[derive(Debug, Clone, Default)]
pub struct MayWebFetch(WebFetchTool);

impl MayWebFetch {
    /// A `web_fetch` tool over the provider web-fetch implementation.
    #[must_use]
    pub fn new() -> Self {
        Self(WebFetchTool::new())
    }
}

impl Tool for MayWebFetch {
    fn name(&self) -> Cow<'static, str> {
        self.0.name()
    }

    fn description(&self) -> Cow<'static, str> {
        WEBFETCH_DESCRIPTION.trim().into()
    }

    type Arguments = WebFetchArgs;
    type Res = ToolResult;
    type Error = <WebFetchTool as Tool>::Error;

    async fn call(&self, args: Self::Arguments, cx: ToolContext) -> Result<Self::Res, Self::Error> {
        self.0.call(args, cx).await
    }
}

#[cfg(test)]
mod tests {
    use aither_core::llm::{Tool, tool::ToolDefinition};

    use super::MayWebFetch;

    #[test]
    fn description_tells_the_model_to_pipe_a_page_into_ask() {
        let tool = MayWebFetch::new();
        assert_eq!(tool.name(), "webfetch");
        let definition = ToolDefinition::new(&tool);
        let description = definition.description();
        assert!(description.contains("pipe it into `ask"), "{description}");
        assert!(description.contains("`rg` or `head`"), "{description}");
        assert!(
            description.contains("Read the whole page only when the whole page is needed"),
            "{description}"
        );
    }
}
