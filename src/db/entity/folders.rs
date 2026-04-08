//! Folder row model for grouping pinned sessions within a profile.

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Model {
    pub id: String,
    /// Profile this folder belongs to.
    pub profile_id: String,
    pub name: String,
    /// Display order within the pinned section.
    pub sort_order: i32,
    pub created_at: String,
}
