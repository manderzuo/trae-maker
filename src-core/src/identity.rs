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
    let video_read_is_implied_by_submit = scope == "videos:read" && principal.scopes.contains("videos:submit");
    if principal.scopes.contains(scope) || principal.scopes.contains("admin:*") || video_read_is_implied_by_submit {
        Ok(())
    } else {
        Err(AuthError::MissingScope {
            scope: scope.to_owned(),
        })
    }
}
