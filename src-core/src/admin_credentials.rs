#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminCredentialRecord {
    pub user_id: String,
    pub username: String,
    pub password_hash: String,
    pub salt: String,
    pub iterations: u32,
    pub must_change_password: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAdminCredential {
    pub user_id: String,
    pub username: String,
    pub password_hash: String,
    pub salt: String,
    pub iterations: u32,
    pub must_change_password: bool,
}
