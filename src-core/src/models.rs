use std::{collections::BTreeSet, fmt, sync::Arc};

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

#[derive(Clone, PartialEq, Eq)]
pub struct IssuedApiKey {
    pub id: String,
    pub plaintext: String,
    pub prefix: String,
    pub user_id: String,
    pub scopes: BTreeSet<String>,
}

impl fmt::Debug for IssuedApiKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedApiKey")
            .field("id", &self.id)
            .field("prefix", &self.prefix)
            .field("user_id", &self.user_id)
            .field("scopes", &self.scopes)
            .finish()
    }
}
