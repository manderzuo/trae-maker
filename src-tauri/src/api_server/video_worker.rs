//! Durable Core video queue worker boundary.
//!
//! The worker is deliberately adapter-agnostic.  It can execute only when an
//! explicit provider/account-bound CoreVideoExecutor is present.  Missing or
//! corrupt protected payloads become unknown and keep the user's hold; they
//! are never silently retried with another account.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::thread;
use std::time::Duration;

use aiwork_core::UpstreamLease;

use super::core_bridge::CoreLeaseError;
use super::core_video::{VideoAdapterOutcome, VideoExecutionRequest};
use super::{ApiSharedState, CoreBridge};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoWorkerStep {
    Idle,
    Accepted {
        job_id: String,
        upstream_request_ref: String,
    },
    Settled {
        job_id: String,
        state: &'static str,
    },
    Unknown {
        job_id: String,
        reason: String,
    },
}

#[derive(Clone)]
pub struct VideoQueueWorker {
    state: Arc<ApiSharedState>,
    worker_id: String,
}

impl VideoQueueWorker {
    pub fn new(state: Arc<ApiSharedState>, worker_id: impl Into<String>) -> Self {
        Self {
            state,
            worker_id: worker_id.into(),
        }
    }

    /// Mark expired held/active leases unknown before the first queue claim.
    pub fn recover_expired(&self) -> Result<Vec<UpstreamLease>, String> {
        let bridge = self.bridge()?;
        bridge
            .store
            .recover_expired_upstream_leases(chrono::Utc::now().timestamp_millis())
            .map_err(|error| error.to_string())
    }

    /// Execute at most one queue item.  The caller should run this on a
    /// dedicated blocking worker thread; provider adapters are synchronous.
    pub fn run_once(&self) -> Result<VideoWorkerStep, String> {
        let bridge = self.bridge()?;
        let executor = bridge.video_executor().map_err(|error| error.to_string())?;
        let Some((job, lease)) = bridge
            .claim_next_video_job_for_worker(&self.worker_id)
            .map_err(|error| error.to_string())?
        else {
            return Ok(VideoWorkerStep::Idle);
        };

        let principal = bridge
            .store
            .principal_for_video_job(&job.id)
            .map_err(|error| error.to_string())?
            .ok_or_else(|| "video worker could not resolve job owner".to_string())?;

        let body = match self
            .state
            .video_payloads
            .get(&job.id, &job.user_id, &job.input_hash)
        {
            Ok(Some(body)) => body,
            Ok(None) => {
                self.settle_unknown(
                    &bridge,
                    &principal,
                    &job.id,
                    &lease.lease_id,
                    "video_payload_missing",
                )?;
                return Ok(VideoWorkerStep::Unknown {
                    job_id: job.id,
                    reason: "video_payload_missing".into(),
                });
            }
            Err(_) => {
                self.settle_unknown(
                    &bridge,
                    &principal,
                    &job.id,
                    &lease.lease_id,
                    "video_payload_read_failed",
                )?;
                return Ok(VideoWorkerStep::Unknown {
                    job_id: job.id,
                    reason: "video_payload_read_failed".into(),
                });
            }
        };

        let heartbeat_failed = Arc::new(AtomicBool::new(false));
        let stop_heartbeat = Arc::new(AtomicBool::new(false));
        let heartbeat = spawn_heartbeat(
            bridge.clone(),
            self.worker_id.clone(),
            job.id.clone(),
            heartbeat_failed.clone(),
            stop_heartbeat.clone(),
        );
        let outcome = executor.submit_video(
            &lease,
            VideoExecutionRequest {
                job_id: job.id.clone(),
                request_id: job.request_id.clone(),
                model: job.model.clone(),
                body,
            },
        );
        stop_heartbeat.store(true, Ordering::Release);
        drop(heartbeat);

        let outcome = if heartbeat_failed.load(Ordering::Acquire) {
            VideoAdapterOutcome::TransportUnknown {
                reason: "video_heartbeat_failed".into(),
                upstream_request_ref: outcome_request_ref(&outcome),
            }
        } else {
            outcome
        };

        match &outcome {
            VideoAdapterOutcome::Accepted {
                upstream_request_ref,
            } => {
                bridge
                    .record_video_job_acceptance(&principal, &job.id, upstream_request_ref)
                    .map_err(|error| error.to_string())?;
                Ok(VideoWorkerStep::Accepted {
                    job_id: job.id,
                    upstream_request_ref: upstream_request_ref.clone(),
                })
            }
            _ => {
                let state = terminal_state(&outcome);
                bridge
                    .settle_video_job(&principal, &job.id, &lease.lease_id, outcome.clone())
                    .map_err(|error| error.to_string())?;
                if releases_payload(&outcome) {
                    let _ = self.state.video_payloads.remove(&job.id);
                }
                if let VideoAdapterOutcome::TransportUnknown { reason, .. } = outcome {
                    Ok(VideoWorkerStep::Unknown {
                        job_id: job.id,
                        reason,
                    })
                } else {
                    Ok(VideoWorkerStep::Settled {
                        job_id: job.id,
                        state,
                    })
                }
            }
        }
    }

    fn bridge(&self) -> Result<Arc<CoreBridge>, String> {
        self.state
            .core
            .as_ref()
            .cloned()
            .ok_or_else(|| CoreLeaseError::EndpointNotEnabled.to_string())
    }

    fn settle_unknown(
        &self,
        bridge: &CoreBridge,
        principal: &aiwork_core::Principal,
        job_id: &str,
        lease_id: &str,
        reason: &str,
    ) -> Result<(), String> {
        bridge
            .settle_video_job(
                principal,
                job_id,
                lease_id,
                VideoAdapterOutcome::TransportUnknown {
                    reason: reason.into(),
                    upstream_request_ref: None,
                },
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

fn spawn_heartbeat(
    bridge: Arc<CoreBridge>,
    worker_id: String,
    job_id: String,
    failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        while !stop.load(Ordering::Acquire) {
            thread::sleep(HEARTBEAT_INTERVAL);
            if stop.load(Ordering::Acquire) {
                break;
            }
            if bridge
                .heartbeat_video_job_for_worker(&worker_id, &job_id)
                .is_err()
            {
                failed.store(true, Ordering::Release);
                break;
            }
        }
    })
}

fn outcome_request_ref(outcome: &VideoAdapterOutcome) -> Option<String> {
    match outcome {
        VideoAdapterOutcome::Accepted {
            upstream_request_ref,
        } => Some(upstream_request_ref.clone()),
        VideoAdapterOutcome::Succeeded {
            upstream_request_ref,
            ..
        }
        | VideoAdapterOutcome::Canceled {
            upstream_request_ref,
        }
        | VideoAdapterOutcome::TransportUnknown {
            upstream_request_ref,
            ..
        } => upstream_request_ref.clone(),
        VideoAdapterOutcome::Rejected { .. } => None,
    }
}

fn releases_payload(outcome: &VideoAdapterOutcome) -> bool {
    matches!(
        outcome,
        VideoAdapterOutcome::Succeeded { .. }
            | VideoAdapterOutcome::Canceled { .. }
            | VideoAdapterOutcome::Rejected {
                accepted: false,
                ..
            }
    )
}

fn terminal_state(outcome: &VideoAdapterOutcome) -> &'static str {
    match outcome {
        VideoAdapterOutcome::Succeeded { .. } => "succeeded",
        VideoAdapterOutcome::Canceled { .. } => "canceled",
        VideoAdapterOutcome::Rejected {
            accepted: false, ..
        } => "failed",
        VideoAdapterOutcome::Rejected { accepted: true, .. }
        | VideoAdapterOutcome::TransportUnknown { .. }
        | VideoAdapterOutcome::Accepted { .. } => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_confirmed_terminal_outcomes_release_protected_payload() {
        assert!(releases_payload(&VideoAdapterOutcome::Succeeded {
            actual_units: Some(1),
            upstream_request_ref: Some("upstream".into()),
            output_ref: Some("jobs/output".into()),
            artifact_ref: None,
        }));
        assert!(releases_payload(&VideoAdapterOutcome::Canceled {
            upstream_request_ref: Some("upstream".into()),
        }));
        assert!(releases_payload(&VideoAdapterOutcome::Rejected {
            status: 400,
            code: "invalid".into(),
            accepted: false,
        }));
        assert!(!releases_payload(&VideoAdapterOutcome::Accepted {
            upstream_request_ref: "upstream".into(),
        }));
        assert!(!releases_payload(&VideoAdapterOutcome::TransportUnknown {
            reason: "timeout".into(),
            upstream_request_ref: None,
        }));
    }
}
