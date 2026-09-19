use std::collections::BTreeSet;
use std::sync::Mutex;

use aiwork_core::{CoreStore, NewUser, Principal, QuotaGrant, UserRole};
use serde::Serialize;
use tauri::State;

use crate::api_server::core_bridge::CoreMode;
use crate::commands::api_server::{core_store_for_admin, ApiServerRuntime};
use crate::core_migration::{
    apply_legacy_with_store, IssuedApiKeyResponse, LegacyMigrationMapping, MigrationApplyResponse,
    MigrationReport,
};
use crate::state::AppState;

#[derive(Debug, Clone, Serialize)]
pub struct CoreStatus {
    pub schema_version: u32,
    pub database_path: String,
    pub foreign_keys_enabled: bool,
    pub core_mode: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreUserResponse {
    pub id: String,
    pub name: String,
    pub role: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreQuotaBalanceResponse {
    pub user_id: String,
    pub resource_kind: String,
    pub available: i64,
    pub held: i64,
}

fn parse_role(role: &str) -> Result<UserRole, String> {
    match role.trim().to_ascii_lowercase().as_str() {
        "admin" => Ok(UserRole::Admin),
        "operator" => Ok(UserRole::Operator),
        "user" => Ok(UserRole::User),
        _ => Err("role must be admin, operator, or user".into()),
    }
}

fn role_name(role: UserRole) -> &'static str {
    match role {
        UserRole::Admin => "admin",
        UserRole::Operator => "operator",
        UserRole::User => "user",
    }
}

fn normalize_scopes(values: Vec<String>) -> BTreeSet<String> {
    values
        .into_iter()
        .map(|scope| scope.trim().to_string())
        .filter(|scope| !scope.is_empty())
        .collect()
}

fn issued_response(key: aiwork_core::IssuedApiKey) -> IssuedApiKeyResponse {
    IssuedApiKeyResponse {
        id: key.id,
        plaintext: key.plaintext,
        prefix: key.prefix,
        user_id: key.user_id,
        scopes: key.scopes,
    }
}

fn authenticate_admin(store: &CoreStore, admin_api_key: &str) -> Result<Principal, String> {
    if admin_api_key.trim().is_empty() {
        return Err("admin_api_key is required".into());
    }
    let principal = store
        .authenticate_api_key(admin_api_key)
        .map_err(|_| "admin_api_key is invalid".to_string())?;
    store
        .authorize_admin_principal(&principal)
        .map_err(|_| "admin_api_key is not authorized".to_string())?;
    Ok(principal)
}

#[tauri::command]
pub fn core_status(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
) -> Result<CoreStatus, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    let settings = crate::api_server::gateway_settings::load(&state.data_dir);
    let core_mode = CoreMode::try_from(settings.core_mode.as_str())
        .map_err(|error| error.to_string())?;
    let running = runtime
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_some();
    Ok(CoreStatus {
        schema_version: store.schema_version().map_err(|error| error.to_string())?,
        database_path: state
            .data_dir
            .join("data")
            .join(aiwork_core::CORE_DB_FILE)
            .display()
            .to_string(),
        foreign_keys_enabled: store
            .foreign_keys_enabled()
            .map_err(|error| error.to_string())?,
        core_mode: match core_mode {
            CoreMode::Off => "off",
            CoreMode::Shadow => "shadow",
            CoreMode::Enforce => "enforce",
        }
        .into(),
        running,
    })
}

#[tauri::command]
pub fn core_migration_inspect(state: State<'_, AppState>) -> Result<MigrationReport, String> {
    crate::core_migration::inspect_legacy(&state.data_dir).map_err(|error| error.to_string())
}

#[tauri::command]
pub fn core_migration_apply(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    mappings: Vec<LegacyMigrationMapping>,
    admin_api_key: String,
) -> Result<MigrationApplyResponse, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    let principal = authenticate_admin(&store, &admin_api_key)?;
    if mappings.is_empty() {
        let report = crate::core_migration::apply_legacy(
            &state.data_dir,
            &mappings,
            &principal,
        )
        .map_err(|error| error.to_string())?;
        return Ok(MigrationApplyResponse {
            report,
            issued_keys: Vec::new(),
        });
    }
    apply_legacy_with_store(&state.data_dir, store, &mappings, &principal)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn core_user_create(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    id: String,
    name: String,
    role: String,
    admin_api_key: String,
) -> Result<CoreUserResponse, String> {
    let role_value = parse_role(&role)?;
    let store = core_store_for_admin(&state, &runtime)?;
    let principal = authenticate_admin(&store, &admin_api_key)?;
    let input = NewUser { id, name, role: role_value };
    let user = store
        .create_user_as_admin(input, &principal)
    .map_err(|error| error.to_string())?;
    Ok(CoreUserResponse {
        id: user.id,
        name: user.name,
        role: role_name(user.role).into(),
    })
}

#[tauri::command]
pub fn core_api_key_issue(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    user_id: String,
    name: String,
    scopes: Vec<String>,
    admin_api_key: String,
) -> Result<IssuedApiKeyResponse, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    let principal = authenticate_admin(&store, &admin_api_key)?;
    let key = store
        .issue_api_key_as_admin(&user_id, &name, normalize_scopes(scopes), &principal)
        .map_err(|error| error.to_string())?;
    Ok(issued_response(key))
}

#[tauri::command]
pub fn core_quota_grant(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    user_id: String,
    resource_kind: String,
    amount: i64,
    admin_api_key: String,
    reason: String,
) -> Result<CoreQuotaBalanceResponse, String> {
    if reason.trim().is_empty() {
        return Err("reason is required".into());
    }
    let store = core_store_for_admin(&state, &runtime)?;
    let principal = authenticate_admin(&store, &admin_api_key)?;
    let balance = store
        .grant_as_admin(QuotaGrant {
            user_id,
            resource_kind,
            amount,
            actor_user_id: principal.user_id.clone(),
            reason,
        }, &principal)
        .map_err(|error| error.to_string())?;
    Ok(CoreQuotaBalanceResponse {
        user_id: balance.user_id,
        resource_kind: balance.resource_kind,
        available: balance.available,
        held: balance.held,
    })
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs};

    use aiwork_core::{CoreStore, NewUser, UserRole};

    use super::authenticate_admin;

    #[test]
    fn command_auth_rejects_non_admin_and_does_not_accept_an_admin_id() {
        let dir = std::env::temp_dir().join(format!("aiwork-command-auth-{}", rand::random::<u64>()));
        let store = CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        store
            .create_user(
                NewUser { id: "admin".into(), name: "Admin".into(), role: UserRole::Admin },
                "bootstrap",
            )
            .unwrap();
        store
            .create_user(
                NewUser { id: "user".into(), name: "User".into(), role: UserRole::User },
                "admin",
            )
            .unwrap();
        let admin_key = store
            .issue_api_key("admin", "admin", BTreeSet::from(["admin:*".into()]), "bootstrap")
            .unwrap();
        let user_key = store
            .issue_api_key("user", "user", BTreeSet::new(), "bootstrap")
            .unwrap();

        assert!(authenticate_admin(&store, &user_key.plaintext).is_err());
        assert!(authenticate_admin(&store, "admin").is_err());
        assert_eq!(authenticate_admin(&store, &admin_key.plaintext).unwrap().user_id, "admin");
        let _ = fs::remove_dir_all(dir);
    }
}
