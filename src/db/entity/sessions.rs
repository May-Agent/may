//! Session entity for the chat history database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "sessions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    /// Last message preview (truncated).
    pub preview: Option<String>,
    /// Number of messages in this session.
    #[sea_orm(default_value = "0")]
    pub message_count: i32,
    /// Profile this session belongs to.
    pub profile_id: Option<String>,
    /// Whether this session is pinned in its profile.
    #[sea_orm(default_value = "0")]
    pub pinned: bool,
    /// Whether this session has been auto-archived.
    #[sea_orm(default_value = "0")]
    pub archived: bool,
    /// Folder this session belongs to (for pinned sessions).
    pub folder_id: Option<String>,
    /// Display order within the pinned section.
    pub pin_sort_order: Option<i32>,
    /// Persisted runtime state for UI/runtime recovery.
    #[sea_orm(default_value = "idle")]
    pub runtime_state: String,
    /// Pending immediate user message payload.
    pub pending_immediate_json: Option<String>,
    /// Queued user message payloads.
    pub queued_messages_json: Option<String>,
    /// Pending permission request id.
    pub waiting_permission_request_id: Option<String>,
    /// Pending permission request payload.
    pub waiting_permission_request_json: Option<String>,
    /// Pending ask-user request id.
    pub waiting_ask_user_request_id: Option<String>,
    /// Pending ask-user request payload.
    pub waiting_ask_user_request_json: Option<String>,
    /// Last runtime-state update timestamp.
    pub runtime_updated_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::messages::Entity")]
    Messages,
    #[sea_orm(
        belongs_to = "super::profiles::Entity",
        from = "Column::ProfileId",
        to = "super::profiles::Column::Id"
    )]
    Profile,
    #[sea_orm(
        belongs_to = "super::folders::Entity",
        from = "Column::FolderId",
        to = "super::folders::Column::Id"
    )]
    Folder,
}

impl Related<super::messages::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Messages.def()
    }
}

impl Related<super::profiles::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Profile.def()
    }
}

impl Related<super::folders::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Folder.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
