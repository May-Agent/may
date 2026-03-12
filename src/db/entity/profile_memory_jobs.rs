//! Per-profile durable memory refresh jobs.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "profile_memory_jobs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub profile_id: String,
    #[sea_orm(default_value = "0")]
    pub dirty: bool,
    pub last_processed_at: Option<String>,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
