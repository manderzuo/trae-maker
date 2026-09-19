use std::{path::Path, sync::Arc};

use aiwork_core::{
    canonical_json_hash, require_scope, BeginRequest, BeginRequestInput, ChatExecutionRequest,
    ChatExecutionResult, CoreError, CoreStore, CreateVideoJobInput, LeaseOutcome, LeaseSettlement,
    PreflightReserveInput, PreflightReserveResult, Principal, RequestResult, RequestState, Reservation,
    ScheduleError, SchedulerLeaseRequest, SchedulerLeaseResult, SelectionStrategy, Settlement,
    UpstreamError, UpstreamLease, UpstreamLeaseGrant, VideoJobEnqueueResult, VideoJobLeaseResult,
};
use serde_json::Value;

#[path = "core_executor.rs"]
pub mod core_executor;

#[allow(unused_imports)]
pub use core_executor::{
    CancelSupport, CoreUpstreamExecutor, LegacyChatTransport, LegacyPoolLeaseAdapter,
    LegacyPoolStreamAdapter, LegacyStreamTransport, LeaseStreamAdapter, LeaseUpstreamAdapter,
    StreamEvent, StreamSink, StreamTerminalOutcome, StreamUsage, UpstreamOutcome,
};
#[allow(unused_imports)]
pub use super::core_video::{
    CoreVideoExecutor, LeaseVideoAdapter, MockVideoAdapter, VideoAdapterOutcome,
    VideoCancelOutcome, VideoExecutionRequest,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreMode {
    Off,
    Shadow,
    Enforce,
}

const CHAT_ENDPOINT: &str = "chat";

impl TryFrom<&str> for CoreMode {
    type Error = CoreError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "shadow" => Ok(Self::Shadow),
            "enforce" => Ok(Self::Enforce),
            _ => Err(CoreError::InvalidConfiguration {
                key: "core_mode".into(),
                value: value.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightResult {
    pub request_id: String,
    pub state: RequestState,
    pub result: Option<RequestResult>,
    pub reservation: Option<Reservation>,
    pub execution: Option<ChatExecutionRequest>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatOutcome {
    Success(ChatExecutionResult),
    Failure(UpstreamError),
    Upstream(UpstreamError),
}

#[derive(Debug)]
pub enum CoreLeaseError {
    EndpointNotEnabled,
    Core(CoreError),
    Schedule(ScheduleError),
}

impl From<CoreError> for CoreLeaseError {
    fn from(error: CoreError) -> Self {
        Self::Core(error)
    }
}

impl From<ScheduleError> for CoreLeaseError {
    fn from(error: ScheduleError) -> Self {
        match error {
            ScheduleError::Core(error) => Self::Core(error),
            other => Self::Schedule(other),
        }
    }
}

impl std::fmt::Display for CoreLeaseError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EndpointNotEnabled => formatter.write_str("scheduler_endpoint_not_enabled"),
            Self::Core(error) => error.fmt(formatter),
            Self::Schedule(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CoreLeaseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasePreflightResult {
    pub request_id: String,
    pub state: RequestState,
    pub result: Option<RequestResult>,
    pub reservation: Option<Reservation>,
    /// Only `Acquired` carries this grant. Replay deliberately has no grant.
    pub lease: Option<UpstreamLeaseGrant>,
    pub replay_lease: Option<UpstreamLease>,
    pub execution: Option<ChatExecutionRequest>,
}

pub struct CoreBridge {
    pub store: Arc<CoreStore>,
    pub mode: CoreMode,
    scheduler: Option<Arc<super::scheduler::SchedulerRuntime>>,
    upstream_executor: Option<CoreUpstreamExecutor>,
    video_executor: Option<CoreVideoExecutor>,
}

impl CoreBridge {
    pub fn new(store: Arc<CoreStore>, mode: CoreMode) -> Self {
        Self {
            store,
            mode,
            scheduler: None,
            upstream_executor: None,
            video_executor: None,
        }
    }

    pub fn with_upstream_executor(mut self, executor: CoreUpstreamExecutor) -> Self {
        self.upstream_executor = Some(executor);
        self
    }

    pub fn with_video_executor(mut self, executor: CoreVideoExecutor) -> Self {
        self.video_executor = Some(executor);
        self
    }

    pub fn with_scheduler(mut self, scheduler: Arc<super::scheduler::SchedulerRuntime>) -> Result<Self, super::scheduler::SchedulerError> {
        use super::scheduler::{validate_modes, SchedulerError, SchedulerMode};
        let effective = validate_modes(self.mode, scheduler.mode)?;
        if effective == SchedulerMode::Off || effective != scheduler.mode || !Arc::ptr_eq(&self.store, &scheduler.store) {
            return Err(SchedulerError::NotReady);
        }
        self.scheduler = Some(scheduler);
        Ok(self)
    }

    /// A missing runtime is an error, never permission to use a legacy pool.
    pub fn scheduler(&self) -> Result<&super::scheduler::SchedulerRuntime, super::scheduler::SchedulerError> {
        self.scheduler.as_deref().ok_or(super::scheduler::SchedulerError::NotReady)
    }

    /// Task 5's route-level bridge. The old `SchedulerRuntime::require_endpoint`
    /// gate remains owned by the startup command and still reports
    /// `scheduler_endpoint_not_enabled`; this method only makes the narrow
    /// lease path available to direct route/runtime consumers. A missing
    /// runtime or adapter fails closed and never selects a legacy pool.
    pub fn upstream_executor(&self) -> Result<CoreUpstreamExecutor, CoreLeaseError> {
        self.upstream_executor_with_bindings(std::iter::empty::<(String, String, String)>())
    }

    /// Build the executor registry only when the caller supplies the explicit
    /// Core-account binding `(account_ref, provider, credentials_ref)`. Task 4
    /// does not expose that directory from `SchedulerRuntime`; Task 6 startup
    /// wiring must provide it. An empty binding set therefore remains closed.
    pub fn upstream_executor_with_bindings<I>(
        &self,
        account_bindings: I,
    ) -> Result<CoreUpstreamExecutor, CoreLeaseError>
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if let Some(executor) = &self.upstream_executor {
            if executor.is_empty() || !executor.can_dispatch_without_provider_binding() {
                return Err(CoreLeaseError::EndpointNotEnabled);
            }
            return Ok(executor.clone());
        }
        let scheduler = self.scheduler.as_ref().ok_or(CoreLeaseError::EndpointNotEnabled)?;
        let executor = CoreUpstreamExecutor::from_chat_executors_with_accounts(
            &scheduler.executors,
            account_bindings,
        );
        if executor.is_empty() || !executor.can_dispatch_without_provider_binding() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        Ok(executor)
    }

    pub fn preflight_chat_with_lease(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: Option<&str>,
        body: &Value,
    ) -> Result<LeasePreflightResult, CoreLeaseError> {
        self.preflight_chat_with_lease_for_accounts(
            principal,
            api_key_id,
            client_idempotency_key,
            body,
            &[],
        )
    }

    /// Check idempotency without touching scheduler capacity. Stream routes
    /// use this before adapter readiness so a completed request remains a
    /// stable replay even while its upstream adapter is temporarily missing.
    pub fn lookup_chat_replay(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: &str,
        body: &Value,
    ) -> Result<Option<LeasePreflightResult>, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        require_scope(principal, "chat:invoke").map_err(|_| {
            CoreLeaseError::Core(CoreError::MissingScope {
                scope: "chat:invoke".into(),
            })
        })?;
        if principal.key_id != api_key_id {
            return Err(CoreLeaseError::Core(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            }));
        }
        let body = super::payload::sanitize_scheduler_chat_body(body);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "chat.model".into(),
                value: "missing or non-string".into(),
            })?;
        let Some(request) = self.store.lookup_idempotent_request(
            &principal.user_id,
            api_key_id,
            CHAT_ENDPOINT,
            model,
            &body,
            client_idempotency_key,
        )? else {
            return Ok(None);
        };
        let request = match request {
            BeginRequest::Existing(request) => request,
            BeginRequest::Conflict => return Err(CoreLeaseError::Schedule(ScheduleError::IdempotencyConflict)),
            BeginRequest::Created(_) => {
                return Err(CoreLeaseError::Core(CoreError::InvalidConfiguration {
                    key: "core.idempotency.lookup".into(),
                    value: "read-only lookup returned a created request".into(),
                }))
            }
        };
        let reservation = self.store.reservation_for_request(&request.id)?;
        let replay_lease = self
            .store
            .upstream_lease_for_request(&request.id, "chat_request")?;
        Ok(Some(LeasePreflightResult {
            request_id: request.id,
            state: request.state,
            result: request.result,
            reservation,
            lease: None,
            replay_lease,
            execution: None,
        }))
    }

    /// Lease preflight constrained to accounts that have an explicit executor
    /// binding. An empty list is fail-closed, never a wildcard.
    pub fn preflight_chat_with_lease_for_accounts(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: Option<&str>,
        body: &Value,
        bound_account_refs: &[String],
    ) -> Result<LeasePreflightResult, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        if bound_account_refs.is_empty() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        require_scope(principal, "chat:invoke").map_err(|_| CoreLeaseError::Core(CoreError::MissingScope {
            scope: "chat:invoke".into(),
        }))?;

        let body = super::payload::sanitize_scheduler_chat_body(body);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "chat.model".into(),
                value: "missing or non-string".into(),
            })?;
        let estimate = self.store.estimate_cost(CHAT_ENDPOINT, model, &body)?;
        if principal.key_id != api_key_id {
            return Err(CoreLeaseError::Core(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            }));
        }
        let idempotency_key = client_idempotency_key
            .filter(|key| !key.trim().is_empty())
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "idempotency_key".into(),
                value: "required in Core enforce mode".into(),
            })?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let input = SchedulerLeaseRequest {
            preflight: PreflightReserveInput {
                request: BeginRequestInput {
                    user_id: principal.user_id.clone(),
                    api_key_id: api_key_id.into(),
                    protocol: "openai".into(),
                    endpoint: CHAT_ENDPOINT.into(),
                    model: model.into(),
                    idempotency_key: idempotency_key.into(),
                    body: body.clone(),
                },
                resource_kind: estimate.resource_kind,
                amount: estimate.reserve_amount,
                ttl_ms: 15 * 60 * 1000,
            },
            provider_hint: None,
            required_capabilities: vec!["chat".into()],
            region: None,
            predicted_units: estimate.reserve_amount,
            safety_margin_units: 0,
            observation_max_age_ms: 10 * 60 * 1000,
            allowed_accounts: Some(bound_account_refs.to_vec()),
            dedicated_account: None,
            selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
            now_ms,
            lease_ttl_ms: 15 * 60 * 1000,
            reconcile_ttl_ms: 10 * 60 * 1000,
        };
        let result = self
            .store
            .preflight_reserve_with_lease(principal, input.clone())?;
        match result {
            SchedulerLeaseResult::Replay { request, lease } => {
                let reservation = self.store.reservation_for_request(&request.id)?;
                Ok(LeasePreflightResult {
                    request_id: request.id,
                    state: request.state,
                    result: request.result,
                    reservation,
                    lease: None,
                    replay_lease: Some(lease),
                    execution: None,
                })
            }
            SchedulerLeaseResult::Acquired(lease) => {
                // The current Core grant deliberately contains only the
                // execution locator. Re-reading the just-created idempotency
                // row is a read-only way to recover request/reservation IDs
                // without adding a second selector or settlement path.
                let recovered = self.store.preflight_reserve(input.preflight.clone());
                let (request, reservation) = match recovered {
                    Ok(PreflightReserveResult::Existing {
                        request,
                        reservation: Some(reservation),
                    }) => (request, reservation),
                    Err(error) => {
                        self.settle_acquired_lease_unknown(
                            principal,
                            &lease,
                            "lease_context_recovery_failed",
                        );
                        return Err(CoreLeaseError::Core(error));
                    }
                    Ok(other) => {
                        self.settle_acquired_lease_unknown(
                            principal,
                            &lease,
                            "lease_context_recovery_failed",
                        );
                        return Err(CoreLeaseError::Core(CoreError::ReservationRequestConflict {
                            request_id: format!("scheduler lease {}: {other:?}", lease.lease_id),
                        }))
                    }
                };
                let execution = ChatExecutionRequest {
                    request_id: request.id.clone(),
                    endpoint: CHAT_ENDPOINT.into(),
                    model: request.model.clone(),
                    body,
                };
                Ok(LeasePreflightResult {
                    request_id: request.id,
                    state: request.state,
                    result: request.result,
                    reservation: Some(reservation),
                    lease: Some(lease),
                    replay_lease: None,
                    execution: Some(execution),
                })
            }
        }
    }

    fn settle_acquired_lease_unknown(
        &self,
        principal: &Principal,
        lease: &UpstreamLeaseGrant,
        reason: &str,
    ) {
        let _ = self.store.settle_upstream_lease(
            principal,
            &lease.lease_id,
            LeaseOutcome::TransportUnknown {
                reason: reason.into(),
                upstream_request_ref: None,
                now_ms: chrono::Utc::now().timestamp_millis(),
            },
        );
    }

    pub fn settle_chat_lease(
        &self,
        principal: &Principal,
        lease_id: &str,
        outcome: LeaseOutcome,
    ) -> Result<LeaseSettlement, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        self.store
            .settle_upstream_lease_with_status(principal, lease_id, outcome)
            .map_err(CoreLeaseError::from)
    }

    /// Video dispatch is enabled only when an explicit provider/account-bound
    /// adapter was injected. The legacy video pool is never an implicit
    /// fallback for this path.
    pub fn video_executor(&self) -> Result<CoreVideoExecutor, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        let executor = self
            .video_executor
            .as_ref()
            .ok_or(CoreLeaseError::EndpointNotEnabled)?;
        if executor.is_empty() || !executor.can_dispatch() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        Ok(executor.clone())
    }

    pub fn preflight_video_job(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: Option<&str>,
        body: &Value,
        job_id: String,
    ) -> Result<VideoJobLeaseResult, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        require_scope(principal, "videos:submit").map_err(|_| {
            CoreLeaseError::Core(CoreError::MissingScope {
                scope: "videos:submit".into(),
            })
        })?;
        if principal.key_id != api_key_id {
            return Err(CoreLeaseError::Core(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            }));
        }
        let idempotency_key = client_idempotency_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "idempotency_key".into(),
                value: "required in Core enforce mode".into(),
            }))?;
        let executor = self.video_executor()?;
        let body = super::payload::sanitize_scheduler_chat_body(body);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "videos.model".into(),
                value: "missing or non-string".into(),
            }))?;
        let estimate = self.store.estimate_cost("videos", model, &body)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let input = SchedulerLeaseRequest {
            preflight: PreflightReserveInput {
                request: BeginRequestInput {
                    user_id: principal.user_id.clone(),
                    api_key_id: api_key_id.into(),
                    protocol: "openai".into(),
                    endpoint: "videos".into(),
                    model: model.into(),
                    idempotency_key: idempotency_key.into(),
                    body: body.clone(),
                },
                resource_kind: "video_job".into(),
                amount: estimate.reserve_amount,
                ttl_ms: 15 * 60 * 1000,
            },
            provider_hint: None,
            required_capabilities: vec!["video".into()],
            region: None,
            predicted_units: estimate.reserve_amount,
            safety_margin_units: 0,
            observation_max_age_ms: 10 * 60 * 1000,
            allowed_accounts: Some(executor.bound_account_refs()),
            dedicated_account: None,
            selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
            now_ms,
            lease_ttl_ms: 15 * 60 * 1000,
            reconcile_ttl_ms: 10 * 60 * 1000,
        };
        self.store
            .preflight_video_job(
                principal,
                input,
                CreateVideoJobInput {
                    id: job_id,
                    input_hash: canonical_json_hash(&body).to_vec(),
                },
            )
            .map_err(CoreLeaseError::from)
    }

    pub fn enqueue_video_job(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: Option<&str>,
        body: &Value,
        job_id: String,
    ) -> Result<VideoJobEnqueueResult, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        require_scope(principal, "videos:submit").map_err(|_| {
            CoreLeaseError::Core(CoreError::MissingScope {
                scope: "videos:submit".into(),
            })
        })?;
        if principal.key_id != api_key_id {
            return Err(CoreLeaseError::Core(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            }));
        }
        let idempotency_key = client_idempotency_key
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "idempotency_key".into(),
                value: "required in Core enforce mode".into(),
            }))?;
        let executor = self.video_executor()?;
        let body = super::payload::sanitize_scheduler_chat_body(body);
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "videos.model".into(),
                value: "missing or non-string".into(),
            }))?;
        let estimate = self.store.estimate_cost("videos", model, &body)?;
        let now_ms = chrono::Utc::now().timestamp_millis();
        let input = SchedulerLeaseRequest {
            preflight: PreflightReserveInput {
                request: BeginRequestInput {
                    user_id: principal.user_id.clone(),
                    api_key_id: api_key_id.into(),
                    protocol: "openai".into(),
                    endpoint: "videos".into(),
                    model: model.into(),
                    idempotency_key: idempotency_key.into(),
                    body: body.clone(),
                },
                resource_kind: "video_job".into(),
                amount: estimate.reserve_amount,
                ttl_ms: 15 * 60 * 1000,
            },
            provider_hint: None,
            required_capabilities: vec!["video".into()],
            region: None,
            predicted_units: estimate.reserve_amount,
            safety_margin_units: 0,
            observation_max_age_ms: 10 * 60 * 1000,
            allowed_accounts: Some(executor.bound_account_refs()),
            dedicated_account: None,
            selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
            now_ms,
            lease_ttl_ms: 15 * 60 * 1000,
            reconcile_ttl_ms: 10 * 60 * 1000,
        };
        self.store
            .enqueue_video_job(
                principal,
                input,
                CreateVideoJobInput {
                    id: job_id,
                    input_hash: canonical_json_hash(&body).to_vec(),
                },
            )
            .map_err(CoreLeaseError::from)
    }

    pub fn claim_video_job_for_worker(
        &self,
        worker_id: &str,
        job_id: &str,
    ) -> Result<Option<(aiwork_core::CoreJob, UpstreamLeaseGrant)>, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        let executor = self.video_executor()?;
        let claim = self
            .store
            .claim_video_job_by_id(worker_id, job_id, chrono::Utc::now().timestamp_millis())
            .map_err(CoreLeaseError::from)?;
        let Some(claim) = claim else {
            return Ok(None);
        };
        let grant = executor
            .grant_for_lease(&claim.lease)
            .ok_or(CoreLeaseError::EndpointNotEnabled)?;
        Ok(Some((claim.job, grant)))
    }

    /// Claim the next durable video queue item.  Selection and lease creation
    /// remain inside Core's Immediate transaction; this bridge only converts
    /// the persisted lease to an explicitly bound adapter grant.
    pub fn claim_next_video_job_for_worker(
        &self,
        worker_id: &str,
    ) -> Result<Option<(aiwork_core::CoreJob, UpstreamLeaseGrant)>, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        let executor = self.video_executor()?;
        let claim = self
            .store
            .claim_next_video_job(worker_id, chrono::Utc::now().timestamp_millis())
            .map_err(CoreLeaseError::from)?;
        let Some(claim) = claim else {
            return Ok(None);
        };
        let grant = executor
            .grant_for_lease(&claim.lease)
            .ok_or(CoreLeaseError::EndpointNotEnabled)?;
        Ok(Some((claim.job, grant)))
    }

    pub fn heartbeat_video_job_for_worker(
        &self,
        worker_id: &str,
        job_id: &str,
    ) -> Result<aiwork_core::CoreJob, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        self.store
            .heartbeat_video_job(worker_id, job_id, chrono::Utc::now().timestamp_millis(), 15 * 60 * 1000)
            .map_err(CoreLeaseError::from)
    }

    pub fn mark_video_job_running(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<aiwork_core::CoreJob, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        self.store
            .mark_video_job_running(principal, job_id, chrono::Utc::now().timestamp_millis())
            .map_err(CoreLeaseError::Core)
    }

    pub fn record_video_job_acceptance(
        &self,
        principal: &Principal,
        job_id: &str,
        upstream_request_ref: &str,
    ) -> Result<aiwork_core::CoreJob, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        self.store
            .record_video_job_acceptance(
                principal,
                job_id,
                upstream_request_ref,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(CoreLeaseError::Core)
    }

    pub fn settle_video_job(
        &self,
        principal: &Principal,
        job_id: &str,
        lease_id: &str,
        outcome: VideoAdapterOutcome,
    ) -> Result<LeaseSettlement, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (output_ref, artifact_ref) = outcome.output_refs();
        for (field, value) in [
            ("jobs.output_ref", output_ref),
            ("jobs.artifact_ref", artifact_ref),
        ] {
            if let Some(value) = value {
                if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                    return Err(CoreLeaseError::Core(CoreError::Validation {
                        field: field.into(),
                        reason: "must be a bounded non-control reference".into(),
                    }));
                }
            }
        }
        let lease_outcome = outcome.lease_outcome(now_ms).ok_or_else(|| {
            CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "video.outcome".into(),
                value: "accepted outcome must be recorded before settlement".into(),
            })
        })?;
        let settlement = self
            .store
            .settle_upstream_lease_with_status(principal, lease_id, lease_outcome)
            .map_err(CoreLeaseError::from)?;
        if settlement.applied && (output_ref.is_some() || artifact_ref.is_some()) {
            self.store
                .set_video_job_result_refs(
                    principal,
                    job_id,
                    output_ref,
                    artifact_ref,
                    now_ms,
                )
                .map_err(CoreLeaseError::Core)?;
        }
        Ok(settlement)
    }

    pub fn reconcile_video_job(
        &self,
        principal: &Principal,
        job_id: &str,
        lease_id: &str,
        outcome: VideoAdapterOutcome,
    ) -> Result<LeaseSettlement, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (output_ref, artifact_ref) = outcome.output_refs();
        for (field, value) in [
            ("jobs.output_ref", output_ref),
            ("jobs.artifact_ref", artifact_ref),
        ] {
            if let Some(value) = value {
                if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                    return Err(CoreLeaseError::Core(CoreError::Validation {
                        field: field.into(),
                        reason: "must be a bounded non-control reference".into(),
                    }));
                }
            }
        }
        let lease_outcome = outcome.lease_outcome(now_ms).ok_or_else(|| {
            CoreLeaseError::Core(CoreError::InvalidConfiguration {
                key: "video.reconcile_outcome".into(),
                value: "reconciliation requires explicit terminal evidence".into(),
            })
        })?;
        let settlement = self
            .store
            .reconcile_unknown_upstream_lease(principal, lease_id, lease_outcome)
            .map_err(CoreLeaseError::from)?;
        if settlement.applied && (output_ref.is_some() || artifact_ref.is_some()) {
            self.store
                .set_video_job_result_refs(principal, job_id, output_ref, artifact_ref, now_ms)
                .map_err(CoreLeaseError::Core)?;
        }
        Ok(settlement)
    }

    pub fn request_video_cancel(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<aiwork_core::CoreJob, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        if self.scheduler.is_none() {
            return Err(CoreLeaseError::EndpointNotEnabled);
        }
        require_scope(principal, "videos:cancel").map_err(|_| {
            CoreLeaseError::Core(CoreError::MissingScope {
                scope: "videos:cancel".into(),
            })
        })?;
        self.store
            .request_video_cancel(principal, job_id, chrono::Utc::now().timestamp_millis())
            .map_err(CoreLeaseError::Core)
    }

    pub fn video_job_lease(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<Option<UpstreamLease>, CoreLeaseError> {
        self.require_enforce().map_err(CoreLeaseError::Core)?;
        let job = self.store.video_job_for_user(principal, job_id)?;
        let Some(job) = job else { return Ok(None) };
        self.store
            .upstream_lease_for_request(&job.request_id, "video_job")
            .map_err(CoreLeaseError::Core)
    }

    pub fn open_for_mode(mode: CoreMode, data_dir: &Path) -> Result<Option<Arc<Self>>, CoreError> {
        match mode {
            CoreMode::Off => Ok(None),
            CoreMode::Shadow | CoreMode::Enforce => {
                let store = Arc::new(CoreStore::open(data_dir)?);
                store.migrate()?;
                Ok(Some(Arc::new(Self::new(store, mode))))
            }
        }
    }

    pub fn preflight_chat(
        &self,
        principal: &Principal,
        api_key_id: &str,
        client_idempotency_key: Option<&str>,
        body: &Value,
    ) -> Result<PreflightResult, CoreError> {
        self.require_enforce()?;
        require_scope(principal, "chat:invoke").map_err(|_| CoreError::MissingScope {
            scope: "chat:invoke".into(),
        })?;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "chat.model".into(),
                value: "missing or non-string".into(),
            })?;

        // Cost policy is checked before the atomic preflight so a missing
        // policy cannot create a request record or consume a client key.
        let estimate = self.store.estimate_cost(CHAT_ENDPOINT, model, body)?;
        if principal.key_id != api_key_id {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            });
        }
        let idempotency_key = client_idempotency_key
            .filter(|key| !key.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| generated_idempotency_key(CHAT_ENDPOINT, model, body));
        let result = self.store.preflight_reserve(PreflightReserveInput {
            request: BeginRequestInput {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
                protocol: "openai".into(),
                endpoint: CHAT_ENDPOINT.into(),
                model: model.into(),
                idempotency_key,
                body: body.clone(),
            },
            resource_kind: estimate.resource_kind,
            amount: estimate.reserve_amount,
            ttl_ms: 15 * 60 * 1000,
        })?;
        let (request, reservation, execution) = match result {
            PreflightReserveResult::Created { request, reservation } => {
                let execution = ChatExecutionRequest {
                    request_id: request.id.clone(),
                    endpoint: CHAT_ENDPOINT.into(),
                    model: request.model.clone(),
                    body: body.clone(),
                };
                (request, Some(reservation), Some(execution))
            }
            PreflightReserveResult::Existing { request, reservation } => {
                (request, reservation, None)
            }
            PreflightReserveResult::Conflict => return Err(CoreError::IdempotencyConflict),
            PreflightReserveResult::Insufficient { available, required } => {
                return Err(CoreError::QuotaInsufficient { available, required });
            }
        };

        Ok(PreflightResult {
            request_id: request.id,
            state: request.state,
            result: request.result,
            reservation,
            execution,
        })
    }

    pub fn settle_chat(
        &self,
        principal: &Principal,
        reservation_id: &str,
        outcome: ChatOutcome,
    ) -> Result<(), CoreError> {
        self.require_enforce()?;
        let (settlement, final_state, result) = match outcome {
            ChatOutcome::Success(result) => (
                Settlement::Commit {
                    actual_amount: result.actual_amount,
                },
                RequestState::Succeeded,
                Some(RequestResult {
                    status: Some(result.status as i64),
                    error_code: None,
                }),
            ),
            ChatOutcome::Failure(error) | ChatOutcome::Upstream(error) => {
                if error.is_uncertain() {
                    (Settlement::Unknown, RequestState::Unknown, Some(RequestResult {
                        status: None,
                        error_code: Some("upstream_uncertain".into()),
                    }))
                } else {
                    (Settlement::Release, RequestState::Failed, Some(upstream_error_result(&error)))
                }
            }
        };
        self.store
            .settle_request(principal, reservation_id, settlement, final_state, result)
            .map(|_| ())
            .map_err(|source| match source {
                CoreError::ReservationOwnerMismatch { .. } => source,
                source => CoreError::ReservationContext {
                    reservation_id: reservation_id.into(),
                    source: Box::new(source),
                },
            })
    }

    pub fn balance(
        &self,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<aiwork_core::QuotaBalance, CoreError> {
        self.store.balance(user_id, resource_kind)
    }

    fn require_enforce(&self) -> Result<(), CoreError> {
        if self.mode != CoreMode::Enforce {
            return Err(CoreError::CoreModeNotEnforcing {
                mode: format!("{:?}", self.mode).to_ascii_lowercase(),
            });
        }
        Ok(())
    }

}

fn upstream_error_result(error: &UpstreamError) -> RequestResult {
    match error {
        UpstreamError::Rejected { status, code } => RequestResult {
            status: Some(*status as i64),
            error_code: code.clone().or_else(|| Some("upstream_rejected".into())),
        },
        UpstreamError::Failed { .. } => RequestResult {
            status: None,
            error_code: Some("upstream_failed".into()),
        },
        UpstreamError::Timeout | UpstreamError::Disconnected => RequestResult {
            status: None,
            error_code: Some("upstream_uncertain".into()),
        },
    }
}

fn generated_idempotency_key(endpoint: &str, model: &str, body: &Value) -> String {
    let hash = aiwork_core::canonical_json_hash(&serde_json::json!({
        "endpoint": endpoint,
        "model": model,
        "body": body,
    }));
    let hex = hash.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    format!("auto-{hex}")
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::{Path, PathBuf}, sync::Arc};

    use aiwork_core::{
        ChatExecutor, CoreError, CoreStore, CostPolicy, MockChatExecutor, NewUser, Principal,
        QuotaGrant, RequestState, UserRole, UpstreamError,
    };
    use serde_json::json;

    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(prefix: &str) -> Self {
            let root = PathBuf::from(r"D:\gpt");
            fs::create_dir_all(&root).unwrap();
            let dir = root.join(format!(
                "aiwork-tauri-core-bridge-{prefix}-{}",
                rand::random::<u64>()
            ));
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bridge_with_grant(grant: i64) -> (CoreBridge, Principal, TestDir) {
        bridge_with_grant_mode(grant, CoreMode::Enforce)
    }

    fn bridge_with_grant_mode(grant: i64, mode: CoreMode) -> (CoreBridge, Principal, TestDir) {
        let dir = TestDir::new("bridge");
        let store = Arc::new(CoreStore::open(dir.path()).unwrap());
        store.migrate().unwrap();
        store
            .create_user(
                NewUser {
                    id: "u1".into(),
                    name: "Test user".into(),
                    role: UserRole::User,
                },
                "bootstrap",
            )
            .unwrap();
        let key = store
            .issue_api_key(
                "u1",
                "test",
                BTreeSet::from(["chat:invoke".to_owned()]),
                "bootstrap",
            )
            .unwrap();
        store
            .upsert_cost_policy(CostPolicy {
                id: "chat-policy-v1".into(),
                endpoint: "chat".into(),
                model_pattern: "mock-*".into(),
                resource_kind: "chat_request".into(),
                reserve_amount: 1,
                max_actual_amount: Some(1),
                version: 1,
                enabled: true,
            })
            .unwrap();
        if grant > 0 {
            store
                .grant(QuotaGrant {
                    user_id: "u1".into(),
                    resource_kind: "chat_request".into(),
                    amount: grant,
                    actor_user_id: "u1".into(),
                    reason: "test grant".into(),
                })
                .unwrap();
        }
        let principal = aiwork_core::Principal {
            user_id: "u1".into(),
            key_id: key.id.clone(),
            scopes: key.scopes,
        };
        (CoreBridge::new(store, mode), principal, dir)
    }

    fn chat_body() -> serde_json::Value {
        json!({"model": "mock-1", "messages": []})
    }

    #[test]
    fn unknown_core_mode_is_a_configuration_error() {
        let error = CoreMode::try_from("typo").unwrap_err();
        assert!(matches!(error, CoreError::InvalidConfiguration { .. }));
    }

    #[test]
    fn preflight_checks_scope_before_identity_or_policy() {
        let (bridge, mut principal, _dir) = bridge_with_grant(1);
        principal.scopes.clear();
        principal.key_id = "missing-key".into();

        let error = bridge
            .preflight_chat(&principal, "missing-key", Some("idem-scope"), &chat_body())
            .unwrap_err();
        assert!(matches!(error, CoreError::MissingScope { scope } if scope == "chat:invoke"));
    }

    #[test]
    fn preflight_reports_missing_policy_before_identity_mismatch() {
        let (bridge, mut principal, _dir) = bridge_with_grant(1);
        principal.key_id = "different-key".into();
        let body = json!({"model": "not-priced", "messages": []});

        let error = bridge
            .preflight_chat(&principal, "missing-key", Some("idem-order"), &body)
            .unwrap_err();
        assert!(matches!(error, CoreError::BudgetPolicyMissing { .. }));
    }

    #[test]
    fn preflight_reserves_before_execution_and_success_commits() {
        let (bridge, principal, _dir) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-success"), &chat_body())
            .unwrap();
        let reservation = first.reservation.as_ref().unwrap();
        assert_eq!(reservation.amount, 1);
        assert_eq!(first.execution.as_ref().unwrap().model, "mock-1");

        bridge
            .settle_chat(
                &principal,
                &reservation.id,
                ChatOutcome::Success(aiwork_core::ChatExecutionResult::ok()),
            )
            .unwrap();
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().available, 0);
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 0);
        assert_eq!(bridge.store.request_state(&first.request_id).unwrap(), RequestState::Settled);
    }

    #[test]
    fn insufficient_budget_fails_preflight_without_reservation() {
        let (bridge, principal, _dir) = bridge_with_grant(0);
        let error = bridge
            .preflight_chat(
                &principal,
                &principal.key_id,
                Some("idem-insufficient"),
                &chat_body(),
            )
            .unwrap_err();
        assert!(matches!(error, CoreError::QuotaInsufficient { .. }));
        bridge
            .store
            .grant(QuotaGrant {
                user_id: "u1".into(),
                resource_kind: "chat_request".into(),
                amount: 1,
                actor_user_id: "u1".into(),
                reason: "retry grant".into(),
            })
            .unwrap();
        let replay = bridge
            .preflight_chat(
                &principal,
                &principal.key_id,
                Some("idem-insufficient"),
                &chat_body(),
            )
            .unwrap();
        assert!(replay.execution.is_some());
        assert_eq!(replay.state, RequestState::Reserved);
    }

    #[test]
    fn same_idempotency_key_reuses_one_reservation() {
        let (bridge, principal, _dir) = bridge_with_grant(2);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-repeat"), &chat_body())
            .unwrap();
        let second = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-repeat"), &chat_body())
            .unwrap();

        assert_eq!(first.reservation.as_ref().unwrap().id, second.reservation.as_ref().unwrap().id);
        assert!(second.execution.is_none());
        assert_eq!(second.state, RequestState::Reserved);
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
    }

    #[test]
    fn timeout_and_disconnect_settle_as_unknown() {
        let (bridge, principal, _dir) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-timeout"), &chat_body())
            .unwrap();
        bridge
            .settle_chat(
                &principal,
                &first.reservation.as_ref().unwrap().id,
                ChatOutcome::Upstream(UpstreamError::Timeout),
            )
            .unwrap();

        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
        assert_eq!(bridge.store.request_state(&first.request_id).unwrap(), RequestState::Settled);
    }

    #[test]
    fn off_and_shadow_bridges_reject_without_store_side_effects() {
        for mode in [CoreMode::Off, CoreMode::Shadow] {
            let (bridge, principal, _dir) = bridge_with_grant_mode(1, mode);
            let error = bridge
                .preflight_chat(&principal, &principal.key_id, Some("idem-disabled"), &chat_body())
                .unwrap_err();
            assert!(matches!(error, CoreError::CoreModeNotEnforcing { .. }));
            assert_eq!(bridge.balance("u1", "chat_request").unwrap().available, 1);

            let settle_error = bridge
                .settle_chat(&principal, "missing-reservation", ChatOutcome::Failure(UpstreamError::Timeout))
                .unwrap_err();
            assert!(matches!(settle_error, CoreError::CoreModeNotEnforcing { .. }));
        }
    }

    #[test]
    fn body_endpoint_cannot_select_a_different_cost_policy() {
        let (bridge, principal, _dir) = bridge_with_grant(1);
        let body = json!({"endpoint": "cheap", "model": "mock-1", "messages": []});
        let result = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-endpoint"), &body)
            .unwrap();
        assert_eq!(result.execution.as_ref().unwrap().endpoint, "chat");
    }

    #[test]
    fn explicit_upstream_failure_releases_quota_and_settles_request() {
        let (bridge, principal, _dir) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-failure"), &chat_body())
            .unwrap();
        bridge
            .settle_chat(
                &principal,
                &first.reservation.as_ref().unwrap().id,
                ChatOutcome::Failure(UpstreamError::Rejected {
                    status: 502,
                    code: Some("upstream_failed".into()),
                }),
            )
            .unwrap();

        assert_eq!(bridge.balance("u1", "chat_request").unwrap().available, 1);
        assert_eq!(bridge.store.request_state(&first.request_id).unwrap(), RequestState::Settled);
    }

    #[test]
    fn terminal_replay_does_not_execute_mock_again() {
        let (bridge, principal, _dir) = bridge_with_grant(2);
        let mut first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-terminal"), &chat_body())
            .unwrap();
        let executor = MockChatExecutor::ok();
        let result = executor.execute(first.execution.take().unwrap()).unwrap();
        bridge
            .settle_chat(
                &principal,
                &first.reservation.as_ref().unwrap().id,
                ChatOutcome::Success(result),
            )
            .unwrap();

        let replay = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-terminal"), &chat_body())
            .unwrap();
        assert_eq!(replay.state, RequestState::Settled);
        assert!(replay.execution.is_none());
        assert_eq!(executor.calls().len(), 1);
    }

    #[test]
    fn settlement_rejects_a_different_owner() {
        let (bridge, principal, _dir) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-owner"), &chat_body())
            .unwrap();
        let other = Principal {
            user_id: "u2".into(),
            key_id: "key-u2".into(),
            scopes: BTreeSet::from(["chat:invoke".to_owned()]),
        };
        let error = bridge
            .settle_chat(
                &other,
                &first.reservation.as_ref().unwrap().id,
                ChatOutcome::Success(aiwork_core::ChatExecutionResult::ok()),
            )
            .unwrap_err();
        assert!(matches!(error, CoreError::ReservationOwnerMismatch { .. }));
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
    }
}
