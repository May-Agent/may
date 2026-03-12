//! Persisted notification mailbox entries.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "notification_mailbox")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub title: String,
    pub body: String,
    pub session_id: Option<String>,
    pub callback_context_xml: Option<String>,
    pub callback_message: Option<String>,
    pub status: String,
    #[sea_orm(default_value = "0")]
    pub sent_count: i32,
    pub last_error: Option<String>,
    pub delivered_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
