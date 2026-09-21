use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreStore};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreAdminSummary {
    pub active_api_keys: u64,
    pub core_total: i64,
    pub core_available: i64,
    pub core_held: i64,
    pub core_settled: i64,
    pub core_settled_today: i64,
    pub upstream_credits: Option<i64>,
    pub queued_jobs: u64,
    pub running_jobs: u64,
    pub reconciliation_jobs: u64,
    pub updated_at_ms: i64,
}

impl CoreStore {
    /// 只返回 Core 用户/Key/额度的聚合指标；不读取或返回上游账号行。
    pub fn admin_summary(&self, now_ms: i64) -> Result<CoreAdminSummary, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let active_api_keys = connection.query_row(
            "SELECT COUNT(*)
             FROM api_keys
             INNER JOIN users ON users.id = api_keys.user_id
             WHERE api_keys.status = 'active' AND users.role <> 'admin'",
            [], |row| row.get::<_, i64>(0),
        )? as u64;
        let core_total = connection.query_row(
            "SELECT COALESCE(SUM(delta), 0) FROM quota_ledger",
            [], |row| row.get::<_, i64>(0),
        )?;
        let core_held = connection.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM quota_reservations WHERE state IN ('held','unknown')",
            [], |row| row.get::<_, i64>(0),
        )?;
        let core_settled = connection.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM quota_ledger WHERE event_kind = 'commit'",
            [], |row| row.get::<_, i64>(0),
        )?;
        let day_start = now_ms - now_ms.rem_euclid(86_400_000);
        let core_settled_today = connection.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM quota_ledger WHERE event_kind = 'commit' AND created_at_ms >= ?1",
            [day_start], |row| row.get::<_, i64>(0),
        )?;
        let queued_jobs = connection.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'queued'",
            [], |row| row.get::<_, i64>(0),
        )? as u64;
        let running_jobs = connection.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'running'",
            [], |row| row.get::<_, i64>(0),
        )? as u64;
        let reconciliation_jobs = connection.query_row(
            "SELECT COUNT(*) FROM jobs WHERE reconcile_required = 1 OR state = 'unknown'",
            [], |row| row.get::<_, i64>(0),
        )? as u64;
        Ok(CoreAdminSummary {
            active_api_keys,
            core_total,
            // `quota_ledger.delta` already includes reserve/release/commit deltas;
            // subtracting `core_held` here would double-count held requests.
            core_available: core_total,
            core_held,
            core_settled,
            core_settled_today,
            upstream_credits: None,
            queued_jobs,
            running_jobs,
            reconciliation_jobs,
            updated_at_ms: Utc::now().timestamp_millis(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::CoreStore;

    #[test]
    fn admin_summary_separates_core_ledger_from_upstream_snapshot() {
        let dir = std::env::temp_dir().join(format!("core-summary-{}", rand::random::<u64>()));
        let store = CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        {
            let connection = store.connection.lock().unwrap();
            connection.execute("INSERT INTO users (id,name,role,status,created_at_ms,updated_at_ms) VALUES ('u','User','user','active',1,1)", []).unwrap();
            connection.execute("INSERT INTO users (id,name,role,status,created_at_ms,updated_at_ms) VALUES ('admin','Admin','admin','active',1,1)", []).unwrap();
            connection.execute("INSERT INTO api_keys (id,user_id,name,prefix,key_digest,scopes_json,status,created_at_ms) VALUES ('k','u','Key','ck_test',x'01','[]','active',1)", []).unwrap();
            connection.execute("INSERT INTO api_keys (id,user_id,name,prefix,key_digest,scopes_json,status,created_at_ms) VALUES ('admin-k','admin','Admin Key','ck_admin',x'02','[]','active',1)", []).unwrap();
            connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms) VALUES ('q','u','credits','commit',7,0,1758000000000)", []).unwrap();
        }
        let summary = store.admin_summary(1_758_000_000_000).unwrap();
        assert_eq!(summary.active_api_keys, 1);
        assert_eq!(summary.core_settled_today, 7);
        assert_eq!(summary.upstream_credits, None);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn usage_trend_groups_settled_points_by_time_and_resource() {
        let dir = std::path::PathBuf::from(r"D:\gpt\aiwork-core-trend-test")
            .join(format!("{}", rand::random::<u64>()));
        let store = CoreStore::open(&dir).unwrap();
        store.migrate().unwrap();
        let start = 1_758_000_000_000_i64;
        {
            let connection = store.connection.lock().unwrap();
            connection.execute("INSERT INTO users (id,name,role,status,created_at_ms,updated_at_ms) VALUES ('u','User','user','active',1,1)", []).unwrap();
            connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms) VALUES ('chat-1','u','chat','commit',3,0,?1)", [start + 10 * 60 * 1_000]).unwrap();
            connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms) VALUES ('video-1','u','video_job','commit',5,0,?1)", [start + 20 * 60 * 1_000]).unwrap();
            connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms) VALUES ('video-2','u','video_job','commit',2,0,?1)", [start + 60 * 60 * 1_000 + 5 * 60 * 1_000]).unwrap();
            connection.execute("INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms) VALUES ('reserve-1','u','video_job','reserve',99,0,?1)", [start + 10 * 60 * 1_000]).unwrap();
        }

        let trend = store.usage_trend(start, start + 2 * 60 * 60 * 1_000, 60 * 60 * 1_000).unwrap();

        assert_eq!(trend.len(), 2);
        assert_eq!(trend[0].text_points, 3);
        assert_eq!(trend[0].video_points, 5);
        assert_eq!(trend[0].total_points, 8);
        assert_eq!(trend[1].text_points, 0);
        assert_eq!(trend[1].video_points, 2);
        assert_eq!(trend[1].total_points, 2);
        assert!(trend[0].bucket_start_ms < trend[1].bucket_start_ms);
        let _ = std::fs::remove_dir_all(dir);
    }
}
