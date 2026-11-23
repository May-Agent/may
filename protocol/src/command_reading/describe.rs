//! A step in plain words, rendered from `templates/command_step.txt`.

use askama::Template;

use super::{BuildTool, Intent, ListTool, ReadRange, SearchTool};

#[derive(Template)]
#[template(path = "command_step.txt", escape = "none")]
struct StepLine<'a> {
    intent: &'a Intent,
}

pub(super) fn describe(intent: &Intent) -> String {
    StepLine { intent }
        .render()
        .expect("the command step template renders for every intent")
}

impl SearchTool {
    const fn finds_files(self) -> bool {
        matches!(self, Self::Fd | Self::Find)
    }
}

impl ListTool {
    const fn is_tree(self) -> bool {
        matches!(self, Self::Tree)
    }
}

impl BuildTool {
    /// The program's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Cargo => "cargo",
            Self::Bun => "bun",
            Self::Npm => "npm",
            Self::Uv => "uv",
            Self::Make => "make",
        }
    }
}
