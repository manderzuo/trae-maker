use rusqlite::params;
use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreStore};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTrendPoint {
    pub bucket_start_ms: i64,
    pub text_points: i64,
    pub video_points: i64,
    pub other_points: i64,
    pub total_points: i64,
}

impl CoreStore {
    /// Returns settled point consumption grouped into fixed time buckets.
    /// The query deliberately ignores reserves, releases, and grants so the
    /// chart represents actual settled usage rather than token estimates.
    pub fn usage_trend(
        &self,
        start_ms: i64,
        end_ms: i64,
        bucket_ms: i64,
    ) -> Result<Vec<UsageTrendPoint>, CoreError> {
        if start_ms < 0 || end_ms <= start_ms {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.range".into(),
                value: format!("start_ms={start_ms}, end_ms={end_ms}"),
            });
        }
        if bucket_ms <= 0 || bucket_ms > end_ms - start_ms {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.bucket".into(),
                value: bucket_ms.to_string(),
            });
        }

        let bucket_count = (end_ms - start_ms + bucket_ms - 1) / bucket_ms;
        if bucket_count > 1_000 {
            return Err(CoreError::InvalidConfiguration {
                key: "usage_trend.bucket_count".into(),
                value: bucket_count.to_string(),
            });
        }

        let mut points = (0..bucket_count)
            .map(|index| UsageTrendPoint {
                bucket_start_ms: start_ms + index * bucket_ms,
                text_points: 0,
                video_points: 0,
                other_points: 0,
                total_points: 0,
            })
            .collect::<Vec<_>>();

        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT created_at_ms, resource_kind, amount
             FROM quota_ledger
             WHERE event_kind = 'commit'
               AND created_at_ms >= ?1
               AND created_at_ms < ?2
             ORDER BY created_at_ms ASC, entry_id ASC",
        )?;
        let rows = statement.query_map(params![start_ms, end_ms], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;

        for row in rows {
            let (created_at_ms, resource_kind, amount) = row?;
            let index = ((created_at_ms - start_ms) / bucket_ms) as usize;
            let Some(point) = points.get_mut(index) else {
                continue;
            };
            point.total_points += amount;
            if is_video_resource(&resource_kind) {
                point.video_points += amount;
            } else if is_text_resource(&resource_kind) {
                point.text_points += amount;
            } else {
                point.other_points += amount;
            }
        }
        Ok(points)
    }
}

fn is_video_resource(resource_kind: &str) -> bool {
    matches!(resource_kind, "video" | "videos" | "video_job")
        || resource_kind.starts_with("video_")
}

fn is_text_resource(resource_kind: &str) -> bool {
    matches!(resource_kind, "chat" | "chat_completion" | "text" | "credits")
}
