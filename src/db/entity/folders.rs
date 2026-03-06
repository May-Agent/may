//! Folder entity for grouping pinned sessions within a profile.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "folders")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Profile this folder belongs to.
    pub profile_id: String,
    pub name: String,
    /// Display order within the pinned section.
    #[sea_orm(default_value = "0")]
    pub sort_order: i32,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {
    #[sea_orm(
        belongs_to = "super::profiles::Entity",
        from = "Column::ProfileId",
        to = "super::profiles::Column::Id"
    )]
    Profile,
    #[sea_orm(has_many = "super::sessions::Entity")]
    Sessions,
}

impl Related<super::profiles::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Profile.def()
    }
}

impl Related<super::sessions::Entity> for Entity {
    fn to() -> RelationDef {
        Relation::Sessions.def()
    }
}

impl ActiveModelBehavior for ActiveModel {}
