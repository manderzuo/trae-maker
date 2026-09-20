use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CoreAdminSummary {
    pub active_api_keys: u64,
    pub total_points: i64,
    pub consumed_today_points: i64,
    pub queued_jobs: u64,
    pub running_jobs: u64,
    pub reconciliation_jobs: u64,
    pub updated_at: DateTime<Utc>,
}

impl Default for CoreAdminSummary {
    fn default() -> Self {
        Self {
            active_api_keys: 0,
            total_points: 0,
            consumed_today_points: 0,
            queued_jobs: 0,
            running_jobs: 0,
            reconciliation_jobs: 0,
            updated_at: Utc::now(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BridgeStatusSnapshot {
    pub connected: bool,
    pub base_url: String,
    pub default_model: String,
    pub model_count: usize,
    pub last_checked_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BridgeStatusSnapshot {
    pub fn connected(base_url: impl Into<String>, default_model: impl Into<String>, model_count: usize) -> Self {
        Self {
            connected: true,
            base_url: base_url.into(),
            default_model: default_model.into(),
            model_count,
            last_checked_at: Utc::now(),
            error: None,
        }
    }

    pub fn failed(base_url: impl Into<String>, error: impl Into<String>) -> Self {
        Self {
            connected: false,
            base_url: base_url.into(),
            default_model: String::new(),
            model_count: 0,
            last_checked_at: Utc::now(),
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BridgeStatusSnapshot;

    #[test]
    fn bridge_secret_is_never_serialized_in_public_status() {
        let status = BridgeStatusSnapshot::connected("https://127.0.0.1:7864", "seedance", 3);
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("Authorization"));
        assert!(!json.contains("api_key"));
    }
}
