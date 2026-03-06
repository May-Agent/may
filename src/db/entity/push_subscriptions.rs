//! Web Push subscription entity for the chat history database.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "push_subscriptions")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    /// Push endpoint URL from the browser.
    pub endpoint: String,
    /// P-256 DH public key (base64url).
    pub p256dh: String,
    /// Auth secret (base64url).
    pub auth: String,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
