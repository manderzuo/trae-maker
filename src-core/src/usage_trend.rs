use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreStore, CreditAmount};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTrendPoint {
    pub bucket_start_ms: i64,
    pub credits: CreditAmount,
}

impl CoreStore {
    /// Groups only settled credits with a final, request-scoped receipt.
    /// Historical commits and holds without a verified settlement are excluded.
    pub fn usage_trend(
        &self,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
        api_key_id: Option<&str>,
    ) -> Result<Vec<UsageTrendPoint>, CoreError> {
        if start_ms < 0 || end_ms <= start_ms {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.range".into(),
                value: format!("start_ms={start_ms}, end_ms={end_ms}"),
            });
        }
        let span_ms = end_ms - start_ms;
        if bucket_ms <= 0 || bucket_ms > span_ms {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.bucket".into(),
                value: bucket_ms.to_string(),
            });
        }
        let bucket_count = span_ms / bucket_ms + i64::from(span_ms % bucket_ms != 0);
        if bucket_count > 1_000 {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.bucket_count".into(),
                value: bucket_count.to_string(),
            });
        }

        let mut points = (0..bucket_count)
            .map(|index| UsageTrendPoint {
                bucket_start_ms: start_ms + index * bucket_ms,
                credits: CreditAmount::default(),
            })
            .collect::<Vec<_>>();
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT s.settled_at_ms, s.actual_credits
             FROM billing_settlements s
             INNER JOIN billing_receipts r ON r.receipt_id = s.receipt_id
             INNER JOIN requests q ON q.id = s.request_id
             WHERE r.status IN ('final','failed_no_charge')
               AND s.settled_at_ms >= ?1 AND s.settled_at_ms < ?2
               AND (?3 IS NULL OR q.api_key_id = ?3)
             ORDER BY s.settled_at_ms ASC, s.request_id ASC",
        )?;
        let rows = statement.query_map(
            rusqlite::params![start_ms, end_ms, api_key_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;

        for row in rows {
            let (settled_at_ms, actual_credits) = row?;
            let index = ((settled_at_ms - start_ms) / bucket_ms) as usize;
            let Some(point) = points.get_mut(index) else { continue };
            let current = point.credits.as_microcredits();
            let total = current.checked_add(actual_credits).ok_or(CoreError::InvalidQuotaAmount)?;
            point.credits = CreditAmount::try_from_microcredits(total)
                .ok_or(CoreError::InvalidQuotaAmount)?;
        }
        Ok(points)
    }
}
