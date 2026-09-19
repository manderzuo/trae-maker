use std::sync::Arc;

use crate::CoreStore;

pub type SharedCoreStore = Arc<CoreStore>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserRole {
    Admin,
    Operator,
    User,
}

impl UserRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Admin => "admin",
            Self::Operator => "operator",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewUser {
    pub id: String,
    pub name: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct User {
    pub id: String,
    pub name: String,
    pub role: UserRole,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedApiKey {
    pub id: String,
    pub plaintext: String,
    pub prefix: String,
    pub user_id: String,
    pub scopes: std::collections::BTreeSet<String>,
}
