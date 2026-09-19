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

#[derive(Debug, Clone, Serialize)]
pub struct CoreUserAdminResponse {
    pub id: String,
    pub name: String,
    pub role: String,
    pub status: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct CoreApiKeyAdminResponse {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub prefix: String,
    pub scopes: BTreeSet<String>,
    pub status: String,
    pub created_at_ms: i64,
    pub revoked_at_ms: Option<i64>,
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

fn user_admin_response(view: aiwork_core::CoreUserAdminView) -> CoreUserAdminResponse {
    CoreUserAdminResponse {
        id: view.id,
        name: view.name,
        role: view.role,
        status: view.status,
        created_at_ms: view.created_at_ms,
        updated_at_ms: view.updated_at_ms,
    }
}

fn api_key_admin_response(view: aiwork_core::CoreApiKeyAdminView) -> CoreApiKeyAdminResponse {
    CoreApiKeyAdminResponse {
        id: view.id,
        user_id: view.user_id,
        name: view.name,
        prefix: view.prefix,
        scopes: view.scopes,
        status: view.status,
        created_at_ms: view.created_at_ms,
        revoked_at_ms: view.revoked_at_ms,
    }
}

pub(crate) fn core_users_list_for_store(
    store: &CoreStore,
    admin_api_key: &str,
) -> Result<Vec<CoreUserAdminResponse>, String> {
    let principal = authenticate_admin(store, admin_api_key)?;
    store
        .list_users_as_admin(&principal)
        .map(|users| users.into_iter().map(user_admin_response).collect())
        .map_err(|error| error.to_string())
}

pub(crate) fn core_api_keys_list_for_store(
    store: &CoreStore,
    admin_api_key: &str,
    user_id: Option<String>,
) -> Result<Vec<CoreApiKeyAdminResponse>, String> {
    let principal = authenticate_admin(store, admin_api_key)?;
    store
        .list_api_keys_as_admin(&principal, user_id.as_deref())
        .map(|keys| keys.into_iter().map(api_key_admin_response).collect())
        .map_err(|error| error.to_string())
}

pub(crate) fn core_quota_balance_for_store(
    store: &CoreStore,
    admin_api_key: &str,
    user_id: &str,
    resource_kind: &str,
) -> Result<CoreQuotaBalanceResponse, String> {
    if user_id.trim().is_empty() {
        return Err("user_id is required".into());
    }
    if resource_kind.trim().is_empty() {
        return Err("resource_kind is required".into());
    }
    let principal = authenticate_admin(store, admin_api_key)?;
    let balance = store
        .quota_balance_as_admin(&principal, user_id, resource_kind)
        .map_err(|error| error.to_string())?;
    Ok(CoreQuotaBalanceResponse {
        user_id: balance.user_id,
        resource_kind: balance.resource_kind,
        available: balance.available,
        held: balance.held,
    })
}

pub(crate) fn core_user_set_status_for_store(
    store: &CoreStore,
    admin_api_key: &str,
    user_id: &str,
    active: bool,
) -> Result<CoreUserAdminResponse, String> {
    if user_id.trim().is_empty() {
        return Err("user_id is required".into());
    }
    let principal = authenticate_admin(store, admin_api_key)?;
    store
        .set_user_status_as_admin(&principal, user_id, active)
        .map(user_admin_response)
        .map_err(|error| error.to_string())
}

pub(crate) fn core_api_key_revoke_for_store(
    store: &CoreStore,
    admin_api_key: &str,
    key_id: &str,
) -> Result<(), String> {
    if key_id.trim().is_empty() {
        return Err("key_id is required".into());
    }
    let principal = authenticate_admin(store, admin_api_key)?;
    store
        .revoke_api_key_as_admin(&principal, key_id)
        .map_err(|error| error.to_string())
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

#[tauri::command]
pub fn core_users_list(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    admin_api_key: String,
) -> Result<Vec<CoreUserAdminResponse>, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    core_users_list_for_store(&store, &admin_api_key)
}

#[tauri::command]
pub fn core_api_keys_list(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    admin_api_key: String,
    user_id: Option<String>,
) -> Result<Vec<CoreApiKeyAdminResponse>, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    core_api_keys_list_for_store(&store, &admin_api_key, user_id)
}

#[tauri::command]
pub fn core_quota_balance(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    admin_api_key: String,
    user_id: String,
    resource_kind: String,
) -> Result<CoreQuotaBalanceResponse, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    core_quota_balance_for_store(&store, &admin_api_key, &user_id, &resource_kind)
}

#[tauri::command]
pub fn core_user_set_status(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    admin_api_key: String,
    user_id: String,
    active: bool,
) -> Result<CoreUserAdminResponse, String> {
    let store = core_store_for_admin(&state, &runtime)?;
    core_user_set_status_for_store(&store, &admin_api_key, &user_id, active)
}

#[tauri::command]
pub fn core_api_key_revoke(
    state: State<'_, AppState>,
    runtime: State<'_, Mutex<Option<ApiServerRuntime>>>,
    admin_api_key: String,
    key_id: String,
) -> Result<(), String> {
    let store = core_store_for_admin(&state, &runtime)?;
    core_api_key_revoke_for_store(&store, &admin_api_key, &key_id)
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs};

    use aiwork_core::{CoreStore, NewUser, UserRole};

    use super::{
        authenticate_admin, core_api_key_revoke_for_store, core_api_keys_list_for_store,
        core_quota_balance_for_store, core_user_set_status_for_store, core_users_list_for_store,
    };

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

    #[test]
    fn admin_management_helpers_require_real_admin_and_redact_key_material() {
        let dir = std::env::temp_dir().join(format!("aiwork-command-admin-{}", rand::random::<u64>()));
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
            .issue_api_key("user", "worker", BTreeSet::from(["videos:submit".into()]), "bootstrap")
            .unwrap();

        let users = core_users_list_for_store(&store, &admin_key.plaintext).unwrap();
        assert_eq!(users.len(), 2);
        let keys = core_api_keys_list_for_store(&store, &admin_key.plaintext, Some("user".into())).unwrap();
        let serialized = serde_json::to_string(&keys).unwrap();
        assert!(serialized.contains(&user_key.prefix));
        assert!(!serialized.contains(&user_key.plaintext));
        assert!(!serialized.contains("key_digest"));
        assert!(core_users_list_for_store(&store, &user_key.plaintext).is_err());
        assert!(core_users_list_for_store(&store, "admin").is_err());

        let balance = core_quota_balance_for_store(
            &store,
            &admin_key.plaintext,
            "user",
            "videos.submit",
        )
        .unwrap();
        assert_eq!(balance.available, 0);
        let disabled =
            core_user_set_status_for_store(&store, &admin_key.plaintext, "user", false).unwrap();
        assert_eq!(disabled.status, "disabled");
        core_api_key_revoke_for_store(&store, &admin_key.plaintext, &user_key.id).unwrap();
        core_api_key_revoke_for_store(&store, &admin_key.plaintext, &user_key.id).unwrap();
        let revoked = core_api_keys_list_for_store(&store, &admin_key.plaintext, Some("user".into()))
            .unwrap()
            .into_iter()
            .find(|key| key.id == user_key.id)
            .unwrap();
        assert_eq!(revoked.status, "revoked");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn main_registers_all_core_admin_commands() {
        let source = fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
        for name in [
            "commands::core::core_users_list",
            "commands::core::core_api_keys_list",
            "commands::core::core_quota_balance",
            "commands::core::core_user_set_status",
            "commands::core::core_api_key_revoke",
        ] {
            assert!(source.contains(name), "missing command registration: {name}");
        }
    }
}
