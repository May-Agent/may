//! What May's agents do around the loop: watching the host, guarding what the
//! agent writes, and the tools that pause the turn for the user.
//!
//! aither carries none of it: the loop calls these through its hooks and
//! context sources, and each module words what it says to the model itself.

use askama::Template;

pub mod ask_user;
pub mod compact_tool;
pub mod disk;
/// Filesystem questions the harness asks.
pub mod fs_util;
pub mod host_load;
pub mod language;
pub mod pressure;
pub mod runaway;
pub mod secret_guard;
pub mod subagent_file;
pub mod tool_request;
pub mod working_document;
pub mod workspace_request;

/// A template rendered as the text the model reads.
///
/// # Panics
/// Panics when the template fails to render, which typed input never does.
pub(crate) fn rendered(template: &impl Template) -> String {
    template
        .render()
        .expect("a harness template renders typed input")
}
