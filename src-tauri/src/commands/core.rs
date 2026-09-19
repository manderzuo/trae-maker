use std::collections::BTreeSet;
use std::sync::Mutex;

use aiwork_core::{NewUser, QuotaGrant, UserRole};
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
    actor_user_id: String,
) -> Result<MigrationApplyResponse, String> {
    if mappings.is_empty() {
        let report = crate::core_migration::apply_legacy(
            &state.data_dir,
            &mappings,
            &actor_user_id,
        )
        .map_err(|error| error.to_string())?;
        return Ok(MigrationApplyResponse {
            report,
            issued_keys: Vec::new(),
        });
    }
    let store = core_store_for_admin(&state, &runtime)?;
    apply_legacy_with_store(&state.data_dir, store, &mappings, &actor_user_id)
        .map_err(|error| error.to_string())
}

#[tauri::command]
pub fn core_user_create(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    id: String,
    name: String,
    role: String,
    actor_user_id: String,
) -> Result<CoreUserResponse, String> {
    let role_value = parse_role(&role)?;
    let store = core_store_for_admin(&state, &runtime)?;
    let user = store
        .create_user(
            NewUser {
                id,
                name,
                role: role_value,
            },
            &actor_user_id,
        )
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
    actor_user_id: String,
) -> Result<IssuedApiKeyResponse, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    let key = store
        .issue_api_key(&user_id, &name, normalize_scopes(scopes), &actor_user_id)
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
    actor_user_id: String,
    reason: String,
) -> Result<CoreQuotaBalanceResponse, String> {
    if reason.trim().is_empty() {
        return Err("reason is required".into());
    }
    let store = core_store_for_admin(&state, &runtime)?;
    let balance = store
        .grant(QuotaGrant {
            user_id,
            resource_kind,
            amount,
            actor_user_id,
            reason,
        })
        .map_err(|error| error.to_string())?;
    Ok(CoreQuotaBalanceResponse {
        user_id: balance.user_id,
        resource_kind: balance.resource_kind,
        available: balance.available,
        held: balance.held,
    })
}
