//! Per-session context-debug snapshot entity.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "context_debug_snapshots")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub session_id: String,
    pub timestamp: String,
    pub model: String,
    pub provider_id: String,
    pub prompt: String,
    pub prompt_attachments_json: String,
    pub assembled_messages_json: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
