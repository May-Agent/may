//! Persisted network audit log entries.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "network_access_logs")]
pub struct Model {
    #[sea_orm(primary_key)]
    pub id: i32,
    pub session_id: Option<String>,
    pub profile_id: Option<String>,
    pub source: String,
    pub mode: String,
    pub domain: String,
    pub port: i32,
    pub allowed: bool,
    pub reason: Option<String>,
    pub timestamp: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
