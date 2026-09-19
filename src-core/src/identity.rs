use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub key_id: String,
    pub scopes: BTreeSet<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("API key is invalid or revoked")]
    InvalidApiKey,
    #[error("missing required scope: {scope}")]
    MissingScope { scope: String },
    #[error("core storage error: {0}")]
    Storage(#[from] crate::CoreError),
}

pub fn require_scope(principal: &Principal, scope: &str) -> Result<(), AuthError> {
    if principal.scopes.contains(scope) {
        Ok(())
    } else {
        Err(AuthError::MissingScope {
            scope: scope.to_owned(),
        })
    }
}
