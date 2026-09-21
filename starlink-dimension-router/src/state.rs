use std::{collections::HashMap, fs, path::PathBuf, sync::{Arc, Mutex}};

use aiwork_core::CoreStore;
use serde::{Deserialize, Serialize};

use crate::{admin_session::{ensure_initial_admin_credential, AdminSessionStore, LoginThrottle, SESSION_TTL_MS}, assets::AssetLimiter, bridge_client::BridgeClient, config::RouterConfig};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserVideoJob {
    pub id: String,
    pub user_id: String,
    pub request_id: String,
    pub upstream_id: Option<String>,
    pub status: String,
    pub output_ref: Option<String>,
    pub error_code: Option<String>,
    pub reconcile_required: bool,
}

pub struct StarlinkRouterState {
    pub store: Arc<CoreStore>,
    pub bridge: Arc<Mutex<BridgeClient>>,
    pub config: RouterConfig,
    pub jobs: Arc<Mutex<HashMap<String, UserVideoJob>>>,
    pub admin_sessions: Arc<AdminSessionStore>,
    pub login_throttle: Arc<LoginThrottle>,
    pub asset_limiter: Arc<AssetLimiter>,
}

impl StarlinkRouterState {
    pub fn open(config: RouterConfig, bridge: BridgeClient) -> Result<Arc<Self>, String> {
        fs::create_dir_all(&config.data_dir).map_err(|e| format!("创建 Core 数据目录失败: {e}"))?;
        let store = Arc::new(CoreStore::open(&config.data_dir).map_err(|e| e.to_string())?);
        store.migrate().map_err(|e| e.to_string())?;
        let initial_password = std::env::var("STARLINK_ADMIN_INITIAL_PASSWORD").ok();
        ensure_initial_admin_credential(&store, initial_password.as_deref()).map_err(|e| e.to_string())?;
        let jobs = load_jobs(&config.data_dir);
        Ok(Arc::new(Self { store, bridge: Arc::new(Mutex::new(bridge)), config, jobs: Arc::new(Mutex::new(jobs)), admin_sessions: Arc::new(AdminSessionStore::new(SESSION_TTL_MS)), login_throttle: Arc::new(LoginThrottle::new()), asset_limiter: Arc::new(AssetLimiter::from_env()) }))
    }

    pub fn for_test(store: Arc<CoreStore>, bridge: BridgeClient, config: RouterConfig) -> Arc<Self> {
        Arc::new(Self { store, bridge: Arc::new(Mutex::new(bridge)), config, jobs: Arc::new(Mutex::new(HashMap::new())), admin_sessions: Arc::new(AdminSessionStore::new(SESSION_TTL_MS)), login_throttle: Arc::new(LoginThrottle::new()), asset_limiter: Arc::new(AssetLimiter::from_env()) })
    }

    pub fn replace_bridge(&self, bridge: BridgeClient) {
        *self.bridge.lock().unwrap() = bridge;
    }

    pub fn persist_jobs(&self) {
        let path = self.config.data_dir.join("video_jobs.json");
        let snapshot = self.jobs.lock().unwrap().clone();
        if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
            let _ = fs::write(path, bytes);
        }
    }
}

fn load_jobs(data_dir: &PathBuf) -> HashMap<String, UserVideoJob> {
    fs::read(data_dir.join("video_jobs.json"))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}
