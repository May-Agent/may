//! The working document: a file in the agent's sandbox that records the
//! session's work, and the context source that keeps it in the conversation
//! and current.
//!
//! The format is the embedder's: it reads the file through its
//! [`DocumentSchema`], into rows, the items they name, what they wait on,
//! the roots and the format's problems ([`DocumentSpec`]). [`WorkEvents`]
//! carry the state changes the rows track; a [`WorkTracker`] says what state
//! the issues and pull requests they name are in and what hangs under each
//! root, and its [`BlockerResolver`] what a blocker still waits on.
//! [`WorkingDocument`] puts all of it into the conversation and alerts, once
//! per event, when the document falls behind; as the model ends its turn it
//! objects while the document holds open work.

mod blocker;
mod document;
mod events;
pub(crate) mod file;
mod review;
mod schema;
#[cfg(test)]
pub(crate) mod test_schema;
mod tracker;

pub use blocker::{AnyBlockerResolver, BlockerError, BlockerResolver};
pub use document::{DocumentCheckpoint, WorkingDocument};
pub use events::{WorkEvent, WorkEvents};
pub use may_tasks::{WorkKind, WorkRef, words};
pub use review::{DocumentAlertItem, DocumentReview};
pub use schema::{AnyDocumentSchema, DocumentRow, DocumentSchema, DocumentSpec, ParsedDocument};
pub use tracker::{
    AnyWorkTracker, RelatedWork, WorkComment, WorkItem, WorkState, WorkStatus, WorkTracker,
    WorkTrackerError,
};
