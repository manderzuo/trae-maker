use std::{sync::{Arc, Weak}, time::Duration};

use crate::state::StarlinkRouterState;

pub const VIDEO_RECONCILE_INTERVAL: Duration = Duration::from_secs(15);

pub fn spawn(state: &Arc<StarlinkRouterState>) {
    let weak_state: Weak<StarlinkRouterState> = Arc::downgrade(state);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(VIDEO_RECONCILE_INTERVAL).await;
            let Some(state) = weak_state.upgrade() else { break; };
            let task_ids = state
                .jobs
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .filter(|job| {
                    job.reservation_id.is_some()
                        && job.billing_state != "settled"
                        && job.billing_state != "released"
                })
                .map(|job| job.id.clone())
                .collect::<Vec<_>>();
            for task_id in task_ids {
                let state = state.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let _ = crate::user_routes::reconcile_video_job_once(&state, &task_id);
                })
                .await;
            }
        }
    });
}
