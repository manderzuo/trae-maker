use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreStore, CreditAmount};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoreAdminSummary {
    pub allocated_credits: CreditAmount,
    pub key_commitments_credits: CreditAmount,
    pub verified_spent_credits: CreditAmount,
    pub verified_spent_today_credits: CreditAmount,
    pub held_credits: CreditAmount,
    pub reconcile_required_count: u64,
    pub queued_jobs: u64,
    pub running_jobs: u64,
    pub updated_at_ms: i64,
}

impl CoreStore {
    /// Summarizes only Key-scoped credits and settlement records verified by a
    /// final receipt. Upstream balance and its freshness are added by the router.
    pub fn admin_summary(&self, now_ms: i64) -> Result<CoreAdminSummary, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let allocated = connection.query_row(
            "SELECT COALESCE(SUM(
                 COALESCE((SELECT SUM(delta) FROM quota_ledger WHERE budget_account_id = a.id), 0) +
                 COALESCE((SELECT SUM(amount) FROM quota_reservations
                           WHERE key_budget_account_id = a.id AND state IN ('held','unknown')), 0) +
                 COALESCE((SELECT SUM(amount) FROM quota_ledger
                           WHERE budget_account_id = a.id AND event_kind = 'commit'), 0)
             ), 0)
             FROM quota_budget_accounts a
             WHERE a.scope = 'key' AND a.resource_kind = 'credits'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let mut commitment_statement = connection.prepare(
            "SELECT
                 COALESCE((SELECT SUM(delta) FROM quota_ledger WHERE budget_account_id = a.id), 0),
                 COALESCE((SELECT SUM(amount) FROM quota_reservations
                           WHERE key_budget_account_id = a.id AND state IN ('held','unknown')), 0)
             FROM quota_budget_accounts a
             WHERE a.scope = 'key' AND a.resource_kind = 'credits'
             ORDER BY a.id",
        )?;
        let commitment_rows = commitment_statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut key_commitments = 0_i64;
        for row in commitment_rows {
            let (available, held) = row?;
            let per_key = available
                .checked_add(held)
                .ok_or(CoreError::InvalidQuotaAmount)?
                .max(0);
            key_commitments = key_commitments
                .checked_add(per_key)
                .ok_or(CoreError::InvalidQuotaAmount)?;
        }
        drop(commitment_statement);
        let held = connection.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM quota_reservations
             WHERE resource_kind = 'credits' AND state IN ('held','unknown')",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let verified_spent = connection.query_row(
            "SELECT COALESCE(SUM(s.actual_credits), 0)
             FROM billing_settlements s
             INNER JOIN billing_receipts r ON r.receipt_id = s.receipt_id
             WHERE r.status IN ('final','failed_no_charge')",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let day_start = now_ms - now_ms.rem_euclid(86_400_000);
        let verified_spent_today = connection.query_row(
            "SELECT COALESCE(SUM(s.actual_credits), 0)
             FROM billing_settlements s
             INNER JOIN billing_receipts r ON r.receipt_id = s.receipt_id
             WHERE r.status IN ('final','failed_no_charge')
               AND s.settled_at_ms >= ?1 AND s.settled_at_ms <= ?2",
            rusqlite::params![day_start, now_ms],
            |row| row.get::<_, i64>(0),
        )?;
        let reconcile_required_count = connection.query_row(
            "SELECT COUNT(*) FROM (
                 SELECT request_id FROM quota_reservations
                 WHERE resource_kind = 'credits' AND state = 'unknown'
                 UNION
                 SELECT request_id FROM billing_settlements WHERE reconcile_required = 1
                 UNION
                 SELECT id FROM jobs WHERE reconcile_required = 1 OR state = 'unknown'
             )",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let queued_jobs = connection.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'queued'",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;
        let running_jobs = connection.query_row(
            "SELECT COUNT(*) FROM jobs WHERE state = 'running'",
            [],
            |row| row.get::<_, i64>(0),
        )? as u64;

        let credits = |value| CreditAmount::try_from_microcredits(value).ok_or(CoreError::InvalidQuotaAmount);
        Ok(CoreAdminSummary {
            allocated_credits: credits(allocated)?,
            key_commitments_credits: credits(key_commitments)?,
            verified_spent_credits: credits(verified_spent)?,
            verified_spent_today_credits: credits(verified_spent_today)?,
            held_credits: credits(held)?,
            reconcile_required_count,
            queued_jobs,
            running_jobs,
            updated_at_ms: Utc::now().timestamp_millis(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::{Path, PathBuf}};

    use rusqlite::Connection;

    use crate::CoreStore;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "aiwork-core-{label}-{}",
                rand::random::<u64>()
            )))
        }

        fn path(&self) -> &Path { &self.0 }
    }

    impl Drop for TestDir {
        fn drop(&mut self) { let _ = fs::remove_dir_all(&self.0); }
    }

    fn seed_user_and_key(connection: &Connection, user: &str, key: &str, budget: &str) {
        connection.execute(
            "INSERT INTO users (id,name,role,status,created_at_ms,updated_at_ms)
             VALUES (?1,?1,'user','active',1,1)",
            [user],
        ).unwrap();
        connection.execute(
            "INSERT INTO api_keys (id,user_id,name,prefix,key_digest,scopes_json,status,created_at_ms)
             VALUES (?1,?2,?1,?1,?3,'[]','active',1)",
            rusqlite::params![key, user, key.as_bytes()],
        ).unwrap();
        connection.execute(
            "INSERT INTO quota_budget_accounts
             (id,scope,user_id,api_key_id,resource_kind,enabled,version,migration_state,created_at_ms,updated_at_ms)
             VALUES (?1,'key',?2,?3,'credits',1,1,'ready',1,1)",
            rusqlite::params![budget, user, key],
        ).unwrap();
    }

    fn seed_verified_settlement(
        connection: &Connection,
        request: &str,
        user: &str,
        key: &str,
        budget: &str,
        actual: i64,
        settled_at_ms: i64,
        reconcile_required: bool,
    ) {
        let quote_id = format!("quote-{request}");
        let receipt_id = format!("receipt-{request}");
        connection.execute(
            "INSERT INTO requests
             (id,user_id,api_key_id,protocol,endpoint,model,request_hash,state,created_at_ms,updated_at_ms)
             VALUES (?1,?2,?3,'openai','/v1/chat/completions','model',x'01','settled',1,?4)",
            rusqlite::params![request, user, key, settled_at_ms],
        ).unwrap();
        connection.execute(
            "INSERT INTO billing_quotes
             (quote_id,request_id,request_fingerprint,endpoint,model,max_credits,unit,source_ref,expires_at_ms,created_at_ms)
             VALUES (?1,?2,'fingerprint','/v1/chat/completions','model',5000000,'credits','quote-source',?3,1)",
            rusqlite::params![quote_id, request, settled_at_ms + 60_000],
        ).unwrap();
        connection.execute(
            "INSERT INTO billing_receipts
             (receipt_id,request_id,receipt_hash,status,actual_credits,unit,source_ref,observed_at_ms,received_at_ms)
             VALUES (?1,?2,x'02','final',?3,'credits','receipt-source',?4,?4)",
            rusqlite::params![receipt_id, request, actual, settled_at_ms - 3_600_000],
        ).unwrap();
        connection.execute(
            "INSERT INTO billing_settlements
             (request_id,quote_id,receipt_id,actual_credits,over_quote,reconcile_required,settled_at_ms)
             VALUES (?1,?2,?3,?4,0,?5,?6)",
            rusqlite::params![request, quote_id, receipt_id, actual, i64::from(reconcile_required), settled_at_ms],
        ).unwrap();
        connection.execute(
            "INSERT INTO quota_ledger
             (entry_id,user_id,resource_kind,event_kind,amount,delta,request_id,created_at_ms,budget_account_id,api_key_id)
             VALUES (?1,?2,'credits','commit',?3,-?3,?4,?5,?6,?7)",
            rusqlite::params![format!("ledger-{request}"), user, actual, request, settled_at_ms, budget, key],
        ).unwrap();
    }

    #[test]
    fn summary_uses_verified_credits_excludes_held_from_spend_and_has_no_active_account_metric() {
        let dir = TestDir::new("admin-summary");
        let store = CoreStore::open(dir.path()).unwrap();
        store.migrate().unwrap();
        {
            let connection = store.connection.lock().unwrap();
            seed_user_and_key(&connection, "u1", "k1", "budget-1");
            seed_user_and_key(&connection, "u2", "k2", "budget-2");
            connection.execute(
                "INSERT INTO quota_ledger
                 (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms,budget_account_id,api_key_id)
                 VALUES ('grant-1','u1','credits','grant',10000000,10000000,1,'budget-1','k1'),
                        ('grant-2','u2','credits','grant',3000000,3000000,1,'budget-2','k2'),
                        ('reserve-1','u1','credits','reserve',2000000,-2000000,2,'budget-1','k1')",
                [],
            ).unwrap();
            connection.execute(
                "INSERT INTO quota_reservations
                 (id,user_id,request_id,resource_kind,amount,state,expires_at_ms,created_at_ms,api_key_id,key_budget_account_id)
                 VALUES ('held-1','u1','held-request','credits',2000000,'held',9999999999999,2,'k1','budget-1')",
                [],
            ).unwrap();
            seed_verified_settlement(&connection, "verified-1", "u1", "k1", "budget-1", 1250000, 1_758_000_000_000, true);
            connection.execute(
                "INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,created_at_ms)
                 VALUES ('legacy-commit','u1','credits','commit',9000000,0,1758000000000)",
                [],
            ).unwrap();
        }

        let summary = store.admin_summary(1_758_000_000_000).unwrap();

        assert_eq!(summary.allocated_credits.to_string(), "13.000000");
        assert_eq!(summary.key_commitments_credits.to_string(), "11.750000");
        assert_eq!(summary.verified_spent_credits.to_string(), "1.250000");
        assert_eq!(summary.verified_spent_today_credits.to_string(), "1.250000");
        assert_eq!(summary.held_credits.to_string(), "2.000000");
        assert_eq!(summary.reconcile_required_count, 1);
        let json = serde_json::to_string(&summary).unwrap();
        assert!(!json.contains("active_api_keys"));
        assert!(!json.contains("active_accounts"));
    }

    #[test]
    fn usage_trend_uses_settled_time_and_filters_one_key_without_unverified_ledger_rows() {
        let dir = TestDir::new("usage-trend");
        let store = CoreStore::open(dir.path()).unwrap();
        store.migrate().unwrap();
        let start = 1_758_000_000_000_i64;
        {
            let connection = store.connection.lock().unwrap();
            seed_user_and_key(&connection, "u1", "k1", "budget-1");
            seed_user_and_key(&connection, "u2", "k2", "budget-2");
            seed_verified_settlement(&connection, "a1", "u1", "k1", "budget-1", 1250000, start + 15 * 60 * 1000, false);
            seed_verified_settlement(&connection, "b1", "u2", "k2", "budget-2", 3000000, start + 25 * 60 * 1000, false);
            seed_verified_settlement(&connection, "a2", "u1", "k1", "budget-1", 250000, start + 75 * 60 * 1000, false);
            connection.execute(
                "INSERT INTO quota_ledger (entry_id,user_id,resource_kind,event_kind,amount,delta,request_id,created_at_ms,api_key_id)
                 VALUES ('unverified-commit','u1','credits','commit',8000000,0,'not-settled',?1,'k1'),
                        ('reservation-only','u1','credits','reserve',9000000,-9000000,'held',?1,'k1')",
                [start + 10 * 60 * 1000],
            ).unwrap();
        }

        let all = store.usage_trend(start, start + 2 * 60 * 60 * 1000, 60 * 60 * 1000, None).unwrap();
        let only_k1 = store.usage_trend(start, start + 2 * 60 * 60 * 1000, 60 * 60 * 1000, Some("k1")).unwrap();

        assert_eq!(all.len(), 2);
        assert_eq!(all[0].credits.to_string(), "4.250000");
        assert_eq!(all[1].credits.to_string(), "0.250000");
        assert_eq!(only_k1[0].credits.to_string(), "1.250000");
        assert_eq!(only_k1[1].credits.to_string(), "0.250000");
        assert_eq!(only_k1[0].bucket_start_ms, start);
        assert!(only_k1[0].bucket_start_ms < only_k1[1].bucket_start_ms);
    }
}
