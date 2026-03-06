//! Per-profile memory entity for the chat history database.
//!
//! Stores preference and recency memory extracted from conversations
//! by a fast LLM, maintained asynchronously.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile_memories")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Profile this memory belongs to.
    pub profile_id: String,
    /// Memory type: "preference" or "recency".
    pub memory_type: String,
    /// Memory content (markdown).
    pub content: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
