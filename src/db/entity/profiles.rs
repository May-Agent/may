//! Profile entity for the chat history database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profiles")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Well-known identifier: "daily", "work", "local", or None for custom profiles.
    pub slug: Option<String>,
    pub name: String,
    /// Emoji or icon identifier for the profile.
    pub icon: Option<String>,
    /// Environment type: "local" or "container".
    pub environment: String,
    /// Docker image tag for container environments.
    pub container_image: Option<String>,
    /// Docker container ID when created.
    pub container_id: Option<String>,
    /// Persona overlay text injected into the agent system prompt.
    pub persona: Option<String>,
    /// JSON blob of per-profile preferences.
    pub preferences_json: Option<String>,
    /// Whether this profile has location awareness enabled.
    #[sea_orm(default_value = "0")]
    pub location_aware: bool,
    /// Maximum number of pinned items in this profile.
    #[sea_orm(default_value = "10")]
    pub pinned_limit: i32,
    /// Display order among profiles.
    #[sea_orm(default_value = "0")]
    pub sort_order: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(has_many = "super::sessions::Entity")]
    Sessions,
    #[sea_orm(has_many = "super::folders::Entity")]
    Folders,
}

impl Related<super::sessions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sessions.def()
    }
}

impl Related<super::folders::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Folders.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
