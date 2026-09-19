//! Lease-aware video dispatch boundary.
//!
//! Core owns identity, quota, account selection, job/attempt state and
//! settlement.  This module only receives an already-selected lease and
//! forwards a transient request to an explicitly bound provider adapter.  A
//! missing binding is an unavailable endpoint, never permission to use the
//! legacy in-memory/video_tasks.json path.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aiwork_core::{LeaseOutcome, UpstreamLease, UpstreamLeaseGrant};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct VideoExecutionRequest {
    pub job_id: String,
    pub request_id: String,
    pub model: String,
    /// Transient request data for the adapter. Core persists only its digest.
    pub body: Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoAdapterOutcome {
    /// The upstream accepted the asynchronous task; the lease remains held
    /// as active until a later status/settlement operation confirms its end.
    Accepted {
        upstream_request_ref: String,
    },
    Succeeded {
        actual_units: Option<i64>,
        upstream_request_ref: Option<String>,
        output_ref: Option<String>,
        artifact_ref: Option<String>,
    },
    Canceled {
        upstream_request_ref: Option<String>,
    },
    Rejected {
        status: u16,
        code: String,
        accepted: bool,
    },
    TransportUnknown {
        reason: String,
        upstream_request_ref: Option<String>,
    },
}

impl VideoAdapterOutcome {
    pub fn lease_outcome(&self, now_ms: i64) -> Option<LeaseOutcome> {
        match self {
            Self::Accepted { .. } => None,
            Self::Succeeded {
                actual_units,
                upstream_request_ref,
                ..
            } => Some(LeaseOutcome::Success {
                actual_units: *actual_units,
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            }),
            Self::Rejected {
                status,
                code,
                accepted,
            } => Some(LeaseOutcome::Rejected {
                status: i64::from(*status),
                code: Some(code.clone()),
                accepted: *accepted,
                now_ms,
            }),
            Self::Canceled {
                upstream_request_ref,
            } => Some(LeaseOutcome::Canceled {
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            }),
            Self::TransportUnknown {
                reason,
                upstream_request_ref,
            } => Some(LeaseOutcome::TransportUnknown {
                reason: reason.clone(),
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            }),
        }
    }

    pub fn output_refs(&self) -> (Option<&str>, Option<&str>) {
        match self {
            Self::Succeeded {
                output_ref,
                artifact_ref,
                ..
            } => (output_ref.as_deref(), artifact_ref.as_deref()),
            _ => (None, None),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoCancelOutcome {
    Confirmed {
        upstream_request_ref: Option<String>,
    },
    Unsupported,
    Unknown {
        reason: String,
        upstream_request_ref: Option<String>,
    },
}

pub trait LeaseVideoAdapter: Send + Sync {
    fn submit_video(
        &self,
        lease: &UpstreamLeaseGrant,
        request: VideoExecutionRequest,
    ) -> VideoAdapterOutcome;

    fn cancel_video(&self, _lease: &UpstreamLeaseGrant) -> VideoCancelOutcome {
        VideoCancelOutcome::Unsupported
    }
}

#[derive(Clone)]
struct AccountBinding {
    account_ref: String,
    provider: String,
    credentials_ref: String,
}

/// Provider/account-bound video adapter registry. It deliberately has no
/// constructor that discovers legacy pool accounts implicitly.
#[derive(Clone, Default)]
pub struct CoreVideoExecutor {
    adapters: BTreeMap<String, Arc<dyn LeaseVideoAdapter>>,
    account_bindings: BTreeMap<String, AccountBinding>,
}

impl CoreVideoExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_provider(
        mut self,
        provider: impl Into<String>,
        adapter: Arc<dyn LeaseVideoAdapter>,
    ) -> Self {
        self.adapters.insert(provider.into(), adapter);
        self
    }

    pub fn for_account(
        mut self,
        account_ref: impl Into<String>,
        provider: impl Into<String>,
        credentials_ref: impl Into<String>,
    ) -> Self {
        let account_ref = account_ref.into();
        self.account_bindings.insert(
            account_ref.clone(),
            AccountBinding {
                account_ref,
                provider: provider.into(),
                credentials_ref: credentials_ref.into(),
            },
        );
        self
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    pub fn can_dispatch(&self) -> bool {
        !self.account_bindings.is_empty()
            && self
                .account_bindings
                .values()
                .all(|binding| self.adapters.contains_key(&binding.provider))
    }

    pub fn bound_account_refs(&self) -> Vec<String> {
        self.account_bindings.keys().cloned().collect()
    }

    pub fn submit_video(
        &self,
        lease: &UpstreamLeaseGrant,
        request: VideoExecutionRequest,
    ) -> VideoAdapterOutcome {
        let Some(binding) = self.account_bindings.get(&lease.account_ref) else {
            return VideoAdapterOutcome::TransportUnknown {
                reason: "account_binding_missing".into(),
                upstream_request_ref: None,
            };
        };
        if binding.provider != lease.provider {
            return VideoAdapterOutcome::TransportUnknown {
                reason: "lease_provider_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        if binding.credentials_ref != lease.credentials_ref {
            return VideoAdapterOutcome::TransportUnknown {
                reason: "lease_credentials_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        let Some(adapter) = self.adapters.get(&binding.provider) else {
            return VideoAdapterOutcome::TransportUnknown {
                reason: "adapter_unavailable".into(),
                upstream_request_ref: None,
            };
        };
        adapter.submit_video(lease, request)
    }

    pub fn grant_for_lease(&self, lease: &UpstreamLease) -> Option<UpstreamLeaseGrant> {
        let binding = self.account_bindings.get(&lease.account_ref)?;
        let observation_id = lease.observation_id.clone()?;
        Some(UpstreamLeaseGrant {
            lease_id: lease.id.clone(),
            account_ref: binding.account_ref.clone(),
            provider: binding.provider.clone(),
            credentials_ref: binding.credentials_ref.clone(),
            observation_id,
            predicted_units: lease.predicted_units,
            lease_expires_at_ms: lease.lease_expires_at_ms,
        })
    }

    pub fn cancel_video(&self, lease: &UpstreamLeaseGrant) -> VideoCancelOutcome {
        let Some(binding) = self.account_bindings.get(&lease.account_ref) else {
            return VideoCancelOutcome::Unknown {
                reason: "account_binding_missing".into(),
                upstream_request_ref: None,
            };
        };
        if binding.provider != lease.provider {
            return VideoCancelOutcome::Unknown {
                reason: "lease_provider_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        if binding.credentials_ref != lease.credentials_ref {
            return VideoCancelOutcome::Unknown {
                reason: "lease_credentials_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        let Some(adapter) = self.adapters.get(&binding.provider) else {
            return VideoCancelOutcome::Unknown {
                reason: "adapter_unavailable".into(),
                upstream_request_ref: None,
            };
        };
        adapter.cancel_video(lease)
    }
}

/// Deterministic test-only adapter boundary. It never performs network I/O or
/// reads credentials; callers provide explicit outcomes for each submission.
pub struct MockVideoAdapter {
    submit_outcomes: Mutex<std::collections::VecDeque<VideoAdapterOutcome>>,
    cancel_outcomes: Mutex<std::collections::VecDeque<VideoCancelOutcome>>,
    calls: Mutex<Vec<String>>,
}

impl MockVideoAdapter {
    pub fn new(
        submit_outcomes: Vec<VideoAdapterOutcome>,
        cancel_outcomes: Vec<VideoCancelOutcome>,
    ) -> Self {
        Self {
            submit_outcomes: Mutex::new(submit_outcomes.into()),
            cancel_outcomes: Mutex::new(cancel_outcomes.into()),
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl LeaseVideoAdapter for MockVideoAdapter {
    fn submit_video(
        &self,
        _lease: &UpstreamLeaseGrant,
        request: VideoExecutionRequest,
    ) -> VideoAdapterOutcome {
        self.calls
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(request.job_id);
        self.submit_outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| VideoAdapterOutcome::Accepted {
                upstream_request_ref: "mock-upstream-video".into(),
            })
    }

    fn cancel_video(&self, _lease: &UpstreamLeaseGrant) -> VideoCancelOutcome {
        self.cancel_outcomes
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(VideoCancelOutcome::Unsupported)
    }
}
