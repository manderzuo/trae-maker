use std::collections::BTreeSet;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    CostError, CostEstimate, CostPolicy, BeginRequest, BeginRequestInput, CoreError, CoreStore,
    LeaseOutcome, LeaseState, ObservationStatus, PreflightReserveInput, PreflightReserveResult,
    CreateVideoJobInput, JobAttemptState, JobState, QuotaReserve, RecoverableBillingRequest,
    RequestHandle, RequestResult, RequestState, ScheduleError, Settlement, SchedulerLeaseRequest, SchedulerLeaseResult,
    SelectionStrategy, UpstreamLease, UpstreamLeaseGrant, VideoJobEnqueueResult, VideoJobLeaseResult,
    upstream::LeaseSettlement,
    upstream::{
        account_matches_constraints, sanitize_error_category, sanitize_upstream_request_ref,
        selection_reason, validate_scheduler_request, CandidateAccount,
    },
};

pub fn canonical_json_hash(value: &Value) -> [u8; 32] {
    let mut canonical = Vec::new();
    write_canonical_json(value, &mut canonical);
    Sha256::digest(canonical).into()
}

impl CoreStore {
    /// Read-only idempotency lookup used by stream routes before checking
    /// upstream adapter readiness. It must never create a request or reserve
    /// quota when the adapter is unavailable.
    pub fn lookup_idempotent_request(
        &self,
        user_id: &str,
        api_key_id: &str,
        endpoint: &str,
        model: &str,
        body: &Value,
        idempotency_key: &str,
    ) -> Result<Option<BeginRequest>, CoreError> {
        let request_hash = request_hash(endpoint, model, body);
        let scope = format!("{user_id}:{api_key_id}:{endpoint}");
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let active_key = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
                params![api_key_id, user_id],
                |row| row.get::<_, bool>(0),
            )?;
        if !active_key {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: user_id.into(),
                api_key_id: api_key_id.into(),
            });
        }
        let Some((stored_hash, request_id)) = connection
            .query_row(
                "SELECT request_hash, request_id FROM idempotency_keys WHERE scope = ?1 AND client_key = ?2",
                params![scope, idempotency_key],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        else {
            return Ok(None);
        };
        if stored_hash != request_hash {
            return Ok(Some(BeginRequest::Conflict));
        }
        Ok(Some(BeginRequest::Existing(Self::request_handle_in_connection(
            &connection,
            &request_id,
        )?)))
    }

    pub fn preflight_reserve_with_lease(
        &self,
        principal: &crate::Principal,
        input: SchedulerLeaseRequest,
    ) -> Result<SchedulerLeaseResult, ScheduleError> {
        self.preflight_reserve_with_lease_internal(principal, input, None)
    }

    pub fn preflight_video_job(
        &self,
        principal: &crate::Principal,
        input: SchedulerLeaseRequest,
        job: CreateVideoJobInput,
    ) -> Result<VideoJobLeaseResult, ScheduleError> {
        if input.preflight.request.endpoint != "videos"
            || input.preflight.resource_kind != "video_job"
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "video_job.endpoint_or_resource".into(),
                value: format!(
                    "endpoint={}, resource_kind={}",
                    input.preflight.request.endpoint, input.preflight.resource_kind
                ),
            }));
        }
        crate::jobs::validate_video_job_input(&job)?;
        let result = self.preflight_reserve_with_lease_internal(principal, input, Some(&job))?;
        match result {
            SchedulerLeaseResult::Acquired(lease) => {
                let (job, attempt) = self
                    .video_job_bundle_for_user(principal, &job.id)?
                    .ok_or_else(|| CoreError::InvalidConfiguration {
                        key: "jobs.create".into(),
                        value: "video job missing after lease preflight".into(),
                    })?;
                Ok(VideoJobLeaseResult::Acquired { job, attempt, lease })
            }
            SchedulerLeaseResult::Replay { request, lease } => {
                let (job, attempt) = self
                    .video_job_bundle_for_request(principal, &request.id)?
                    .ok_or_else(|| CoreError::InvalidConfiguration {
                        key: "jobs.replay".into(),
                        value: "idempotent video request has no job".into(),
                    })?;
                Ok(VideoJobLeaseResult::Replay { job, attempt, lease })
            }
        }
    }

    pub fn enqueue_video_job(
        &self,
        principal: &crate::Principal,
        input: SchedulerLeaseRequest,
        job_input: CreateVideoJobInput,
    ) -> Result<VideoJobEnqueueResult, ScheduleError> {
        if input.preflight.request.endpoint != "videos"
            || input.preflight.resource_kind != "video_job"
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "video_job.endpoint_or_resource".into(),
                value: format!(
                    "endpoint={}, resource_kind={}",
                    input.preflight.request.endpoint, input.preflight.resource_kind
                ),
            }));
        }
        crate::jobs::validate_video_job_input(&job_input)?;
        validate_scheduler_request(&input)?;
        if principal.user_id != input.preflight.request.user_id
            || principal.key_id != input.preflight.request.api_key_id
        {
            return Err(ScheduleError::InvalidRequestIdentity);
        }
        if !principal.scopes.contains("videos:submit") {
            return Err(ScheduleError::MissingScope("videos:submit".into()));
        }
        let request_hash = request_hash(
            &input.preflight.request.endpoint,
            &input.preflight.request.model,
            &input.preflight.request.body,
        );
        let scope = format!(
            "{}:{}:{}",
            input.preflight.request.user_id,
            input.preflight.request.api_key_id,
            input.preflight.request.endpoint
        );
        let required_capabilities_json = serde_json::to_string(&input.required_capabilities).map_err(|_| {
            ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue.required_capabilities".into(),
                value: "cannot serialize queue capabilities".into(),
            })
        })?;
        let allowed_accounts_json = input
            .allowed_accounts
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| {
                ScheduleError::Core(CoreError::InvalidConfiguration {
                    key: "jobs.queue.allowed_accounts".into(),
                    value: "cannot serialize queue account filter".into(),
                })
            })?;
        let selection_strategy = match input.selection_strategy {
            SelectionStrategy::HighestNormalizedAvailable => "highest_normalized_available",
            SelectionStrategy::LeastActiveSlots => "least_active_slots",
        };
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active_key = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
            params![&principal.key_id, &principal.user_id],
            |row| row.get::<_, bool>(0),
        )?;
        if !active_key {
            return Err(ScheduleError::InvalidRequestIdentity);
        }
        let estimate = Self::estimate_in_connection(
            &transaction,
            &input.preflight.request.endpoint,
            &input.preflight.request.model,
            &input.preflight.request.body,
        )?;
        if estimate.resource_kind != input.preflight.resource_kind {
            return Err(ScheduleError::Core(CoreError::BudgetPolicyMissing {
                endpoint: input.preflight.request.endpoint.clone(),
                model: input.preflight.request.model.clone(),
            }));
        }
        if let Some((stored_hash, request_id)) = transaction
            .query_row(
                "SELECT request_hash, request_id FROM idempotency_keys WHERE scope = ?1 AND client_key = ?2",
                params![&scope, &input.preflight.request.idempotency_key],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            if stored_hash != request_hash {
                return Err(ScheduleError::IdempotencyConflict);
            }
            let request = Self::request_handle_in_transaction(&transaction, &request_id)?;
            let reservation = Self::reservation_by_request(&transaction, &request_id)?;
            let job = crate::jobs::job_for_request_in_transaction(&transaction, &request_id)?
                .ok_or_else(|| ScheduleError::Core(CoreError::ReservationRequestConflict {
                    request_id: request_id.clone(),
                }))?;
            transaction.commit()?;
            return Ok(VideoJobEnqueueResult::Replay {
                request,
                reservation,
                job,
            });
        }
        let now = input.now_ms;
        let reservation_expires = now
            .checked_add(input.preflight.ttl_ms)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let request_id = Self::new_id("request");
        transaction.execute(
            "INSERT INTO requests (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', ?8, ?8)",
            params![
                &request_id,
                &input.preflight.request.user_id,
                &input.preflight.request.api_key_id,
                &input.preflight.request.protocol,
                &input.preflight.request.endpoint,
                &input.preflight.request.model,
                request_hash.to_vec(),
                now
            ],
        )?;
        transaction.execute(
            "INSERT INTO idempotency_keys (scope, client_key, request_hash, request_id, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &scope,
                &input.preflight.request.idempotency_key,
                request_hash.to_vec(),
                &request_id,
                now
            ],
        )?;
        Self::transition_request_on_connection(
            &transaction,
            &request_id,
            RequestState::Received,
            RequestState::Validating,
            None,
            now,
        )?;
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &QuotaReserve {
                user_id: input.preflight.request.user_id.clone(),
                request_id: request_id.clone(),
                resource_kind: input.preflight.resource_kind.clone(),
                amount: input.preflight.amount,
                ttl_ms: input.preflight.ttl_ms,
            },
            &input.preflight.request.api_key_id,
            now,
            reservation_expires,
        )? {
            crate::ReserveResult::Created(reservation) => reservation,
            crate::ReserveResult::Insufficient { available } => {
                return Err(ScheduleError::Core(CoreError::QuotaInsufficient {
                    available,
                    required: input.preflight.amount,
                }))
            }
            crate::ReserveResult::Existing(_) => {
                return Err(ScheduleError::Core(CoreError::ReservationRequestConflict {
                    request_id,
                }))
            }
        };
        Self::transition_request_on_connection(
            &transaction,
            &request_id,
            RequestState::Validating,
            RequestState::Reserved,
            None,
            now,
        )?;
        crate::jobs::insert_video_job_without_attempt(
            &transaction,
            principal,
            &request_id,
            &input.preflight.request.model,
            &job_input,
            now,
        )?;
        transaction.execute(
            "UPDATE jobs SET
                queue_provider_hint = ?1,
                queue_required_capabilities_json = ?2,
                queue_region = ?3,
                queue_predicted_units = ?4,
                queue_safety_margin_units = ?5,
                queue_observation_max_age_ms = ?6,
                queue_allowed_accounts_json = ?7,
                queue_dedicated_account = ?8,
                queue_selection_strategy = ?9,
                queue_lease_ttl_ms = ?10,
                queue_reconcile_ttl_ms = ?11
             WHERE id = ?12",
            params![
                &input.provider_hint,
                &required_capabilities_json,
                &input.region,
                input.predicted_units,
                input.safety_margin_units,
                input.observation_max_age_ms,
                &allowed_accounts_json,
                &input.dedicated_account,
                selection_strategy,
                input.lease_ttl_ms,
                input.reconcile_ttl_ms,
                &job_input.id,
            ],
        )?;
        let request = Self::request_handle_in_transaction(&transaction, &request_id)?;
        let job = crate::jobs::job_for_request_in_transaction(&transaction, &request_id)?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.clone(),
            })?;
        transaction.commit()?;
        Ok(VideoJobEnqueueResult::Created {
            request,
            reservation,
            job,
        })
    }

    fn preflight_reserve_with_lease_internal(
        &self,
        principal: &crate::Principal,
        input: SchedulerLeaseRequest,
        video_job: Option<&CreateVideoJobInput>,
    ) -> Result<SchedulerLeaseResult, ScheduleError> {
        validate_scheduler_request(&input)?;
        if principal.user_id != input.preflight.request.user_id || principal.key_id != input.preflight.request.api_key_id {
            return Err(ScheduleError::InvalidRequestIdentity);
        }
        let required_scope = if input.preflight.request.endpoint == "videos" {
            "videos:submit".to_owned()
        } else {
            format!("{}:invoke", input.preflight.request.endpoint)
        };
        if !principal.scopes.contains(&required_scope) {
            return Err(ScheduleError::MissingScope(required_scope));
        }
        let now = input.now_ms;
        let request_hash = request_hash(
            &input.preflight.request.endpoint,
            &input.preflight.request.model,
            &input.preflight.request.body,
        );
        let scope = format!(
            "{}:{}:{}",
            input.preflight.request.user_id,
            input.preflight.request.api_key_id,
            input.preflight.request.endpoint
        );
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let active_key = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active')",
            params![&principal.key_id, &principal.user_id], |row| row.get::<_, bool>(0),
        )?;
        if !active_key {
            return Err(ScheduleError::InvalidRequestIdentity);
        }

        let estimate = Self::estimate_in_connection(
            &transaction, &input.preflight.request.endpoint, &input.preflight.request.model,
            &input.preflight.request.body,
        )?;
        if estimate.resource_kind != input.preflight.resource_kind {
            return Err(ScheduleError::Core(CoreError::BudgetPolicyMissing {
                endpoint: input.preflight.request.endpoint.clone(), model: input.preflight.request.model.clone(),
            }));
        }

        if let Some((stored_hash, request_id)) = transaction.query_row(
            "SELECT request_hash, request_id FROM idempotency_keys WHERE scope = ?1 AND client_key = ?2",
            params![&scope, &input.preflight.request.idempotency_key],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        ).optional()? {
            if stored_hash != request_hash {
                return Err(ScheduleError::IdempotencyConflict);
            }
            let request = Self::request_handle_in_transaction(&transaction, &request_id)?;
            let lease = Self::upstream_lease_for_request_in_transaction(&transaction, &request_id, &input.preflight.resource_kind)?
                .ok_or_else(|| ScheduleError::Core(CoreError::ReservationRequestConflict { request_id }))?;
            transaction.commit()?;
            return Ok(SchedulerLeaseResult::Replay { request, lease });
        }

        let candidate = Self::select_upstream_candidate(&transaction, &input)?;
        let reservation_expires = now.checked_add(input.preflight.ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let lease_expires = now.checked_add(input.lease_ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let reconcile_until = now.checked_add(input.reconcile_ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let request_id = Self::new_id("request");
        transaction.execute(
            "INSERT INTO requests (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', ?8, ?8)",
            params![&request_id, &input.preflight.request.user_id, &input.preflight.request.api_key_id,
                &input.preflight.request.protocol, &input.preflight.request.endpoint, &input.preflight.request.model,
                request_hash.to_vec(), now],
        )?;
        transaction.execute(
            "INSERT INTO idempotency_keys (scope, client_key, request_hash, request_id, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![&scope, &input.preflight.request.idempotency_key, request_hash.to_vec(), &request_id, now],
        )?;
        Self::transition_request_on_connection(&transaction, &request_id, RequestState::Received, RequestState::Validating, None, now)?;
        let reservation = match Self::reserve_dual_in_transaction(&transaction, &QuotaReserve {
            user_id: input.preflight.request.user_id.clone(), request_id: request_id.clone(),
            resource_kind: input.preflight.resource_kind.clone(), amount: input.preflight.amount,
            ttl_ms: input.preflight.ttl_ms,
        }, &input.preflight.request.api_key_id, now, reservation_expires)? {
            crate::ReserveResult::Created(reservation) => reservation,
            crate::ReserveResult::Insufficient { available } => return Err(ScheduleError::Core(CoreError::QuotaInsufficient {
                available, required: input.preflight.amount,
            })),
            crate::ReserveResult::Existing(_) => return Err(ScheduleError::Core(CoreError::ReservationRequestConflict { request_id })),
        };
        Self::transition_request_on_connection(&transaction, &request_id, RequestState::Validating, RequestState::Reserved, None, now)?;
        let lease = UpstreamLease {
            id: Self::new_id("lease"), request_id: request_id.clone(), account_ref: candidate.id.clone(),
            resource_kind: input.preflight.resource_kind.clone(), predicted_units: input.predicted_units,
            observation_id: Some(candidate.observation_id.clone()), state: LeaseState::Held,
            lease_expires_at_ms: lease_expires, reconcile_until_ms: Some(reconcile_until), upstream_request_ref: None,
            error_kind: None, created_at_ms: now, updated_at_ms: now, settled_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO upstream_leases (id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10, ?10, NULL)",
            params![&lease.id, &lease.request_id, &lease.account_ref, &lease.resource_kind, lease.predicted_units,
                &lease.observation_id, lease.state.as_str(), lease.lease_expires_at_ms, lease.reconcile_until_ms, now],
        )?;
        if let Some(video_job) = video_job {
            crate::jobs::insert_video_job_and_attempt(
                &transaction,
                principal,
                &request_id,
                &input.preflight.request.model,
                &lease,
                video_job,
                now,
            )?;
        }
        Self::insert_audit_event(&transaction, &principal.user_id, "upstream.lease_acquire", "upstream_lease", &lease.id,
            serde_json::json!({"request_id": request_id, "lease_id": lease.id, "account_ref": lease.account_ref,
                "provider": candidate.provider, "resource_kind": lease.resource_kind, "observation_id": candidate.observation_id,
                "selection_reason": selection_reason(&input), "reservation_id": reservation.id}), now)?;
        let grant = UpstreamLeaseGrant {
            lease_id: lease.id, account_ref: candidate.id, provider: candidate.provider,
            credentials_ref: candidate.credentials_ref,
            observation_id: candidate.observation_id, predicted_units: lease.predicted_units,
            lease_expires_at_ms: lease.lease_expires_at_ms,
        };
        transaction.commit()?;
        Ok(SchedulerLeaseResult::Acquired(grant))
    }

    pub fn heartbeat_upstream_lease(
        &self, principal: &crate::Principal, lease_id: &str, now_ms: i64, lease_ttl_ms: i64,
    ) -> Result<UpstreamLease, ScheduleError> {
        if lease_ttl_ms <= 0 { return Err(ScheduleError::Core(CoreError::InvalidQuotaAmount)); }
        let expires_at_ms = now_ms.checked_add(lease_ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease = Self::upstream_lease_by_id(&transaction, lease_id)?.ok_or_else(|| ScheduleError::LeaseNotFound(lease_id.into()))?;
        Self::validate_lease_owner(&transaction, &lease, principal)?;
        if matches!(lease.state, LeaseState::Held | LeaseState::Active) {
            transaction.execute(
                "UPDATE upstream_leases SET state = 'active', lease_expires_at_ms = ?1, updated_at_ms = ?2 WHERE id = ?3 AND state IN ('held', 'active')",
                params![expires_at_ms, now_ms, lease_id],
            )?;
        }
        let lease = Self::upstream_lease_by_id(&transaction, lease_id)?.expect("lease exists inside transaction");
        transaction.commit()?;
        Ok(lease)
    }

    pub fn request_upstream_cancel(
        &self,
        principal: &crate::Principal,
        lease_id: &str,
        now_ms: i64,
    ) -> Result<UpstreamLease, ScheduleError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease = Self::upstream_lease_by_id(&transaction, lease_id)?
            .ok_or_else(|| ScheduleError::LeaseNotFound(lease_id.into()))?;
        Self::validate_lease_owner(&transaction, &lease, principal)?;

        if !matches!(lease.state, LeaseState::Held | LeaseState::Active) {
            transaction.commit()?;
            return Ok(lease);
        }

        let current_request_state = Self::request_state_in_transaction(&transaction, &lease.request_id)?;
        if current_request_state != RequestState::CancelRequested {
            Self::transition_request_on_connection(
                &transaction,
                &lease.request_id,
                current_request_state,
                RequestState::CancelRequested,
                None,
                now_ms,
            )?;
            Self::insert_audit_event(
                &transaction,
                &principal.user_id,
                "upstream.lease_cancel_requested",
                "upstream_lease",
                lease_id,
                serde_json::json!({
                    "request_id": lease.request_id,
                    "lease_id": lease_id,
                    "reason": "client_cancel",
                }),
                now_ms,
            )?;
        }
        let lease = Self::upstream_lease_by_id(&transaction, lease_id)?.expect("lease exists inside transaction");
        transaction.commit()?;
        Ok(lease)
    }

    pub fn settle_upstream_lease(
        &self, principal: &crate::Principal, lease_id: &str, outcome: LeaseOutcome,
    ) -> Result<UpstreamLease, ScheduleError> {
        self.settle_upstream_lease_with_status(principal, lease_id, outcome)
            .map(|settlement| settlement.lease)
    }

    pub fn settle_upstream_lease_with_status(
        &self, principal: &crate::Principal, lease_id: &str, outcome: LeaseOutcome,
    ) -> Result<LeaseSettlement, ScheduleError> {
        self.settle_upstream_lease_with_status_inner(principal, lease_id, outcome, false)
    }

    pub fn reconcile_unknown_upstream_lease(
        &self,
        principal: &crate::Principal,
        lease_id: &str,
        outcome: LeaseOutcome,
    ) -> Result<LeaseSettlement, ScheduleError> {
        match &outcome {
            LeaseOutcome::Success {
                upstream_request_ref: Some(reference),
                ..
            }
            | LeaseOutcome::Canceled {
                upstream_request_ref: Some(reference),
                ..
            } if !reference.trim().is_empty() => {}
            LeaseOutcome::Rejected { accepted: false, .. } => {}
            _ => {
                return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                    key: "reconcile.evidence".into(),
                    value: "unknown lease reconciliation requires explicit terminal evidence".into(),
                }))
            }
        }
        self.settle_upstream_lease_with_status_inner(principal, lease_id, outcome, true)
    }

    fn settle_upstream_lease_with_status_inner(
        &self,
        principal: &crate::Principal,
        lease_id: &str,
        outcome: LeaseOutcome,
        allow_unknown: bool,
    ) -> Result<LeaseSettlement, ScheduleError> {
        let now = match &outcome {
            LeaseOutcome::Success { now_ms, .. }
            | LeaseOutcome::Rejected { now_ms, .. }
            | LeaseOutcome::TransportUnknown { now_ms, .. }
            | LeaseOutcome::Canceled { now_ms, .. } => *now_ms,
        };
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let lease = Self::upstream_lease_by_id(&transaction, lease_id)?.ok_or_else(|| ScheduleError::LeaseNotFound(lease_id.into()))?;
        Self::validate_lease_owner(&transaction, &lease, principal)?;
        if allow_unknown && matches!(lease.state, LeaseState::Held | LeaseState::Active) {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "reconcile.lease".into(),
                value: "explicit reconciliation requires an unknown lease".into(),
            }));
        }
        let is_unknown_reconciliation = allow_unknown && lease.state == LeaseState::Unknown;
        if !matches!(lease.state, LeaseState::Held | LeaseState::Active) && !is_unknown_reconciliation {
            transaction.commit()?;
            return Ok(LeaseSettlement { lease, applied: false });
        }
        if is_unknown_reconciliation
            && lease.reconcile_until_ms.map_or(true, |deadline| now > deadline)
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "reconcile.window".into(),
                value: "unknown lease reconciliation window has expired".into(),
            }));
        }
        let reservation = Self::reservation_by_request(&transaction, &lease.request_id)?
            .ok_or_else(|| ScheduleError::Core(CoreError::ReservationNotFound { reservation_id: lease.request_id.clone() }))?;
        Self::validate_reservation_owner(&transaction, &reservation, principal)?;
        let current_request_state = Self::request_state_in_transaction(&transaction, &lease.request_id)?;
        let canceled_outcome = matches!(&outcome, LeaseOutcome::Canceled { .. });
        let (lease_state, settlement, request_state, result, error_kind, upstream_request_ref, reconcile_until_ms) = match outcome {
            LeaseOutcome::Success { actual_units, upstream_request_ref, .. } => (
                LeaseState::Succeeded, crate::Settlement::Commit { actual_amount: actual_units }, RequestState::Succeeded,
                None, None::<String>, sanitize_upstream_request_ref(upstream_request_ref), None,
            ),
            LeaseOutcome::Rejected { status, code, accepted: false, .. } => (
                LeaseState::Failed, crate::Settlement::Release, RequestState::Failed,
                Some(RequestResult { status: Some(status), error_code: Some(sanitize_error_category(code.as_deref().unwrap_or_default(), false).into()) }),
                Some(sanitize_error_category(code.as_deref().unwrap_or_default(), false).into()), None, None,
            ),
            LeaseOutcome::Rejected { code, accepted: true, .. } => (
                LeaseState::Unknown, crate::Settlement::Unknown, RequestState::Unknown,
                Some(RequestResult { status: None, error_code: Some(sanitize_error_category(code.as_deref().unwrap_or_default(), false).into()) }),
                Some(sanitize_error_category(code.as_deref().unwrap_or_default(), false).into()), None,
                lease.reconcile_until_ms.or(Some(now.checked_add(crate::upstream::DEFAULT_RECONCILE_TTL_MS).ok_or(CoreError::InvalidQuotaAmount)?)),
            ),
            LeaseOutcome::TransportUnknown { reason, upstream_request_ref, .. } => (
                LeaseState::Unknown, crate::Settlement::Unknown, RequestState::Unknown,
                Some(RequestResult { status: None, error_code: Some(sanitize_error_category(&reason, true).into()) }),
                Some(sanitize_error_category(&reason, true).into()), sanitize_upstream_request_ref(upstream_request_ref),
                lease.reconcile_until_ms.or(Some(now.checked_add(crate::upstream::DEFAULT_RECONCILE_TTL_MS).ok_or(CoreError::InvalidQuotaAmount)?)),
            ),
            LeaseOutcome::Canceled { upstream_request_ref, .. } => (
                LeaseState::Failed, crate::Settlement::Release, RequestState::Canceled,
                Some(RequestResult { status: Some(499), error_code: Some("canceled".into()) }),
                Some("canceled".into()), sanitize_upstream_request_ref(upstream_request_ref), None,
            ),
        };
        if request_state == RequestState::Unknown {
            if current_request_state != RequestState::Unknown {
                Self::transition_request_on_connection(
                    &transaction, &lease.request_id, current_request_state, RequestState::Unknown, result, now,
                )?;
            }
        } else {
            Self::settle_request_state(&transaction, &lease.request_id, current_request_state, request_state, result, now)?;
        }
        Self::apply_settlement_for_reservation(&transaction, &reservation, settlement, now)?;
        let updated = if is_unknown_reconciliation {
            transaction.execute(
                "UPDATE upstream_leases SET state = ?1, reconcile_until_ms = ?2, upstream_request_ref = ?3, error_kind = ?4, updated_at_ms = ?5, settled_at_ms = ?6 WHERE id = ?7 AND state = 'unknown'",
                params![lease_state.as_str(), reconcile_until_ms, upstream_request_ref, error_kind, now,
                    if lease_state == LeaseState::Unknown { None } else { Some(now) }, lease_id],
            )?
        } else {
            transaction.execute(
                "UPDATE upstream_leases SET state = ?1, reconcile_until_ms = ?2, upstream_request_ref = ?3, error_kind = ?4, updated_at_ms = ?5, settled_at_ms = ?6 WHERE id = ?7 AND state IN ('held', 'active')",
                params![lease_state.as_str(), reconcile_until_ms, upstream_request_ref, error_kind, now,
                    if lease_state == LeaseState::Unknown { None } else { Some(now) }, lease_id],
            )?
        };
        if updated != 1 {
            return Ok(LeaseSettlement { lease, applied: false });
        }
        if lease.resource_kind == "video_job" {
            let (job_state, attempt_state, reconcile_required) = if canceled_outcome {
                (JobState::Canceled, JobAttemptState::Canceled, false)
            } else {
                match lease_state {
                    LeaseState::Succeeded => (JobState::Succeeded, JobAttemptState::Succeeded, false),
                    LeaseState::Failed => (JobState::Failed, JobAttemptState::Failed, false),
                    LeaseState::Unknown => (JobState::Unknown, JobAttemptState::Unknown, true),
                    _ => (JobState::Unknown, JobAttemptState::Unknown, true),
                }
            };
            Self::sync_video_job_settlement(
                &transaction,
                &lease,
                job_state,
                attempt_state,
                error_kind.as_deref(),
                upstream_request_ref.as_deref(),
                reconcile_required,
                now,
            )?;
        }
        Self::insert_audit_event(&transaction, &principal.user_id, "upstream.lease_settle", "upstream_lease", lease_id,
            serde_json::json!({"request_id": lease.request_id, "lease_id": lease_id, "outcome": lease_state.as_str(), "error_kind": error_kind}), now)?;
        let settled = Self::upstream_lease_by_id(&transaction, lease_id)?.expect("lease exists inside transaction");
        transaction.commit()?;
        Ok(LeaseSettlement { lease: settled, applied: true })
    }

    pub fn recover_expired_upstream_leases(&self, now_ms: i64) -> Result<Vec<UpstreamLease>, ScheduleError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms FROM upstream_leases WHERE state IN ('held', 'active') AND lease_expires_at_ms <= ?1 ORDER BY created_at_ms, id",
        )?;
        let expired = statement.query_map([now_ms], Self::upstream_lease_from_row)?.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        for lease in &expired {
            let reservation = Self::reservation_by_request(&transaction, &lease.request_id)?
                .ok_or_else(|| CoreError::ReservationNotFound {
                    reservation_id: lease.request_id.clone(),
                })?;
            // Recovery preserves the hold: an expired upstream call is not known
            // to have succeeded or failed. Mark the user reservation unknown so
            // it remains non-spendable without emitting a release ledger entry.
            Self::apply_settlement_for_reservation(&transaction, &reservation, Settlement::Unknown, now_ms)?;
            transaction.execute(
                "UPDATE upstream_leases SET state = 'unknown', reconcile_until_ms = ?1, error_kind = 'lease_expired', updated_at_ms = ?2 WHERE id = ?3 AND state IN ('held', 'active')",
                params![now_ms.checked_add(crate::upstream::DEFAULT_RECONCILE_TTL_MS).ok_or(CoreError::InvalidQuotaAmount)?, now_ms, &lease.id],
            )?;
            if lease.resource_kind == "video_job" {
                Self::sync_video_job_settlement(
                    &transaction,
                    lease,
                    JobState::Unknown,
                    JobAttemptState::Unknown,
                    Some("lease_expired"),
                    lease.upstream_request_ref.as_deref(),
                    true,
                    now_ms,
                )?;
            }
            let current_request_state = Self::request_state_in_transaction(&transaction, &lease.request_id)?;
            if current_request_state != RequestState::Unknown {
                Self::transition_request_on_connection(&transaction, &lease.request_id, current_request_state, RequestState::Unknown,
                    Some(RequestResult { status: None, error_code: Some("lease_expired".into()) }), now_ms)?;
            }
            Self::insert_audit_event(&transaction, "system", "upstream.lease_recovered", "upstream_lease", &lease.id,
                serde_json::json!({"request_id": lease.request_id, "lease_id": lease.id, "reason": "lease_expired"}), now_ms)?;
        }
        let mut statement = transaction.prepare(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms FROM upstream_leases WHERE state IN ('held', 'active', 'unknown') ORDER BY created_at_ms, id",
        )?;
        let recoverable = statement.query_map([], Self::upstream_lease_from_row)?.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        transaction.commit()?;
        Ok(recoverable)
    }

    pub fn request_state(&self, request_id: &str) -> Result<RequestState, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let value = connection
            .query_row(
                "SELECT state FROM requests WHERE id = ?1",
                [request_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.to_owned(),
            })?;
        RequestState::from_db(&value).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(),
            value,
        })
    }

    /// Lists a bounded batch of quoted requests whose quota is still held and
    /// whose final receipt has not been recorded. This is for restart recovery:
    /// callers may query the billing receipt, but must not repeat upstream work.
    pub fn recoverable_billing_requests(
        &self,
        limit: usize,
    ) -> Result<Vec<RecoverableBillingRequest>, CoreError> {
        let limit = limit.clamp(1, 500) as i64;
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let mut statement = connection.prepare(
            "SELECT r.id, r.endpoint
             FROM requests r
             JOIN billing_quotes q ON q.request_id = r.id
             JOIN quota_reservations reservation ON reservation.request_id = r.id
             LEFT JOIN billing_settlements settlement ON settlement.request_id = r.id
             WHERE settlement.request_id IS NULL
               AND reservation.resource_kind = 'credits'
               AND reservation.state IN ('held', 'unknown')
               AND r.state IN ('reserved', 'queued', 'dispatched', 'completing', 'unknown')
             ORDER BY r.created_at_ms, r.id
             LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(RecoverableBillingRequest {
                request_id: row.get(0)?,
                endpoint: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>().map_err(CoreError::from)
    }

    pub fn estimate_cost(
        &self,
        endpoint: &str,
        model: &str,
        body: &Value,
    ) -> Result<CostEstimate, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::estimate_in_connection(&connection, endpoint, model, body)
    }

    pub fn upsert_cost_policy(&self, policy: CostPolicy) -> Result<(), CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection.execute(
            "INSERT INTO cost_policies \
             (id, endpoint, model_pattern, resource_kind, reserve_amount, max_actual_amount, version, enabled) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) \
             ON CONFLICT(id) DO UPDATE SET endpoint = excluded.endpoint, \
               model_pattern = excluded.model_pattern, resource_kind = excluded.resource_kind, \
               reserve_amount = excluded.reserve_amount, max_actual_amount = excluded.max_actual_amount, \
               version = excluded.version, enabled = excluded.enabled",
            params![
                policy.id,
                policy.endpoint,
                policy.model_pattern,
                policy.resource_kind,
                policy.reserve_amount,
                policy.max_actual_amount,
                policy.version,
                i64::from(policy.enabled),
            ],
        )?;
        Ok(())
    }

    pub fn begin_request(&self, input: BeginRequestInput) -> Result<BeginRequest, CoreError> {
        self.begin_request_internal(input, true)
    }

    /// Creates an authenticated, Core-numbered request before an upstream
    /// quote is available. Callers must be behind the normal API-key auth
    /// boundary; paid dispatch still requires `reserve_credit_quote`.
    pub fn begin_billed_request(&self, input: BeginRequestInput) -> Result<BeginRequest, CoreError> {
        self.begin_request_internal(input, false)
    }

    fn begin_request_internal(
        &self,
        input: BeginRequestInput,
        require_local_cost_policy: bool,
    ) -> Result<BeginRequest, CoreError> {
        let request_hash = request_hash(&input.endpoint, &input.model, &input.body);
        let scope = format!("{}:{}:{}", input.user_id, input.api_key_id, input.endpoint);
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let active_key = transaction
            .query_row(
                "SELECT 1 FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
                params![input.api_key_id, input.user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if !active_key {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: input.user_id,
                api_key_id: input.api_key_id,
            });
        }

        if let Some((stored_hash, request_id)) = transaction
            .query_row(
                "SELECT request_hash, request_id FROM idempotency_keys WHERE scope = ?1 AND client_key = ?2",
                params![scope, input.idempotency_key],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let handle = Self::request_handle_in_transaction(&transaction, &request_id)?;
            return if stored_hash == request_hash {
                Ok(BeginRequest::Existing(handle))
            } else {
                Ok(BeginRequest::Conflict)
            };
        }

        if require_local_cost_policy {
            Self::estimate_in_transaction(&transaction, &input.endpoint, &input.model, &input.body)?;
        }
        let request_id = Self::new_id("request");
        transaction.execute(
            "INSERT INTO requests \
             (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', ?8, ?8)",
            params![
                request_id,
                input.user_id,
                input.api_key_id,
                input.protocol,
                input.endpoint,
                input.model,
                request_hash.to_vec(),
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO idempotency_keys (scope, client_key, request_hash, request_id, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![scope, input.idempotency_key, request_hash.to_vec(), request_id, now],
        )?;
        let handle = Self::request_handle_in_transaction(&transaction, &request_id)?;
        transaction.commit()?;
        Ok(BeginRequest::Created(handle))
    }

    /// Returns a stable, URL-safe digest of the stored request body and model.
    /// The digest is safe to pass to the authenticated billing bridge and lets
    /// Core reject a quote prepared for different request content.
    pub fn request_fingerprint_for_billing(&self, request_id: &str) -> Result<String, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        let fingerprint = connection
            .query_row(
                "SELECT request_hash FROM requests WHERE id = ?1",
                [request_id],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: request_id.to_owned(),
            })?;
        if fingerprint.len() != 32 {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: request_id.to_owned(),
            });
        }
        Ok(URL_SAFE_NO_PAD.encode(fingerprint))
    }

    pub fn upstream_lease_for_request(
        &self,
        request_id: &str,
        resource_kind: &str,
    ) -> Result<Option<UpstreamLease>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection
            .query_row(
                "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms \
                 FROM upstream_leases WHERE request_id = ?1 AND resource_kind = ?2",
                params![request_id, resource_kind],
                Self::upstream_lease_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub fn preflight_reserve(
        &self,
        input: PreflightReserveInput,
    ) -> Result<PreflightReserveResult, CoreError> {
        if input.amount <= 0 || input.ttl_ms < 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = now
            .checked_add(input.ttl_ms)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let request_hash = request_hash(
            &input.request.endpoint,
            &input.request.model,
            &input.request.body,
        );
        let scope = format!(
            "{}:{}:{}",
            input.request.user_id, input.request.api_key_id, input.request.endpoint
        );
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let max_concurrency = transaction
            .query_row(
                "SELECT max_concurrency FROM api_keys WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
                params![&input.request.api_key_id, &input.request.user_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::InvalidRequestIdentity {
                user_id: input.request.user_id.clone(),
                api_key_id: input.request.api_key_id.clone(),
            })?;

        if let Some((stored_hash, request_id)) = transaction
            .query_row(
                "SELECT request_hash, request_id FROM idempotency_keys WHERE scope = ?1 AND client_key = ?2",
                params![&scope, &input.request.idempotency_key],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
        {
            let request = Self::request_handle_in_transaction(&transaction, &request_id)?;
            return if stored_hash == request_hash {
                Ok(PreflightReserveResult::Existing {
                    request,
                    reservation: Self::reservation_by_request(&transaction, &request_id)?,
                })
            } else {
                Ok(PreflightReserveResult::Conflict)
            };
        }

        let active_concurrency: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM requests
             WHERE api_key_id = ?1
               AND state IN ('received','validating','reserved','queued','dispatched','completing','unknown')",
            [&input.request.api_key_id],
            |row| row.get(0),
        )?;
        if active_concurrency >= max_concurrency {
            return Err(CoreError::KeyConcurrencyExceeded {
                api_key_id: input.request.api_key_id.clone(),
                active_concurrency,
                max_concurrency,
            });
        }

        let request_id = Self::new_id("request");
        transaction.execute(
            "INSERT INTO requests \
             (id, user_id, api_key_id, protocol, endpoint, model, request_hash, state, created_at_ms, updated_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'received', ?8, ?8)",
            params![
                &request_id,
                &input.request.user_id,
                &input.request.api_key_id,
                &input.request.protocol,
                &input.request.endpoint,
                &input.request.model,
                request_hash.to_vec(),
                now,
            ],
        )?;
        transaction.execute(
            "INSERT INTO idempotency_keys (scope, client_key, request_hash, request_id, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &scope,
                &input.request.idempotency_key,
                request_hash.to_vec(),
                &request_id,
                now,
            ],
        )?;

        let request = Self::request_handle_in_transaction(&transaction, &request_id)?;
        Self::transition_request_on_connection(
            &transaction,
            &request_id,
            RequestState::Received,
            RequestState::Validating,
            None,
            now,
        )?;
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &QuotaReserve {
                user_id: request.user_id.clone(),
                request_id: request_id.clone(),
                resource_kind: input.resource_kind,
                amount: input.amount,
                ttl_ms: input.ttl_ms,
            },
            &input.request.api_key_id,
            now,
            expires_at_ms,
        )? {
            crate::ReserveResult::Created(reservation) => reservation,
            crate::ReserveResult::Insufficient { available } => {
                return Ok(PreflightReserveResult::Insufficient {
                    available,
                    required: input.amount,
                });
            }
            crate::ReserveResult::Existing(_) => {
                return Err(CoreError::ReservationRequestConflict { request_id });
            }
        };
        Self::transition_request_on_connection(
            &transaction,
            &request_id,
            RequestState::Validating,
            RequestState::Reserved,
            None,
            now,
        )?;
        transaction.commit()?;
        Ok(PreflightReserveResult::Created {
            request: RequestHandle {
                state: RequestState::Reserved,
                ..request
            },
            reservation,
        })
    }

    pub fn transition_request(
        &self,
        request_id: &str,
        expected: RequestState,
        next: RequestState,
        result: Option<RequestResult>,
    ) -> Result<(), CoreError> {
        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::transition_request_on_connection(
            &connection,
            request_id,
            expected,
            next,
            result,
            now,
        )
    }

    pub(crate) fn transition_request_on_connection(
        connection: &rusqlite::Connection,
        request_id: &str,
        expected: RequestState,
        next: RequestState,
        result: Option<RequestResult>,
        now: i64,
    ) -> Result<(), CoreError> {
        if !expected.can_transition_to(next) {
            return Err(CoreError::InvalidTransition {
                request_id: request_id.to_owned(),
                expected,
                next,
            });
        }

        let changed = match result {
            Some(result) => connection.execute(
                "UPDATE requests SET state = ?1, result_status = ?2, error_code = ?3, updated_at_ms = ?4 \
                 WHERE id = ?5 AND state = ?6",
                params![next.as_str(), result.status, result.error_code, now, request_id, expected.as_str()],
            )?,
            None => connection.execute(
                "UPDATE requests SET state = ?1, updated_at_ms = ?2 WHERE id = ?3 AND state = ?4",
                params![next.as_str(), now, request_id, expected.as_str()],
            )?,
        };
        if changed == 1 {
            Ok(())
        } else {
            Err(CoreError::InvalidTransition {
                request_id: request_id.to_owned(),
                expected,
                next,
            })
        }
    }

    fn estimate_in_transaction(
        transaction: &Transaction<'_>,
        endpoint: &str,
        model: &str,
        body: &Value,
    ) -> Result<(), CoreError> {
        Self::estimate_in_connection(transaction, endpoint, model, body).map(|_| ())
    }

    fn estimate_in_connection(
        connection: &rusqlite::Connection,
        endpoint: &str,
        model: &str,
        body: &Value,
    ) -> Result<CostEstimate, CoreError> {
        let mut statement = connection.prepare(
            "SELECT id, endpoint, model_pattern, resource_kind, reserve_amount, max_actual_amount, version, enabled \
             FROM cost_policies WHERE endpoint = ?1 AND enabled = 1 ORDER BY version DESC, id ASC",
        )?;
        let policies = statement.query_map([endpoint], |row| {
            Ok(CostPolicy {
                id: row.get(0)?,
                endpoint: row.get(1)?,
                model_pattern: row.get(2)?,
                resource_kind: row.get(3)?,
                reserve_amount: row.get(4)?,
                max_actual_amount: row.get(5)?,
                version: row.get(6)?,
                enabled: row.get::<_, i64>(7)? != 0,
            })
        })?;
        for policy in policies {
            match policy?.estimate(endpoint, model, body) {
                Ok(estimate) => return Ok(estimate),
                Err(CostError::BudgetPolicyMissing { .. }) => {}
            }
        }
        Err(CoreError::BudgetPolicyMissing {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
        })
    }

    pub(crate) fn request_handle_in_transaction(
        transaction: &Transaction<'_>,
        request_id: &str,
    ) -> Result<RequestHandle, CoreError> {
        transaction.query_row(
            "SELECT id, user_id, api_key_id, protocol, endpoint, model, state, result_status, error_code FROM requests WHERE id = ?1",
            [request_id],
            |row| {
                let state = row.get::<_, String>(6)?;
                let state = RequestState::from_db(&state).ok_or_else(|| {
                    rusqlite::Error::FromSqlConversionFailure(
                        6,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid request state")),
                    )
                })?;
                Ok(RequestHandle {
                    id: row.get(0)?,
                    user_id: row.get(1)?,
                    api_key_id: row.get(2)?,
                    protocol: row.get(3)?,
                    endpoint: row.get(4)?,
                    model: row.get(5)?,
                    state,
                    result: {
                        let status: Option<i64> = row.get(7)?;
                        let error_code: Option<String> = row.get(8)?;
                        if status.is_some() || error_code.is_some() {
                            Some(RequestResult { status, error_code })
                        } else {
                            None
                        }
                    },
                })
            },
        ).map_err(CoreError::from)
    }

    fn request_handle_in_connection(
        connection: &rusqlite::Connection,
        request_id: &str,
    ) -> Result<RequestHandle, CoreError> {
        connection
            .query_row(
                "SELECT id, user_id, api_key_id, protocol, endpoint, model, state, result_status, error_code FROM requests WHERE id = ?1",
                [request_id],
                |row| {
                    let state = row.get::<_, String>(6)?;
                    let state = RequestState::from_db(&state).ok_or_else(|| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid request state")),
                        )
                    })?;
                    Ok(RequestHandle {
                        id: row.get(0)?,
                        user_id: row.get(1)?,
                        api_key_id: row.get(2)?,
                        protocol: row.get(3)?,
                        endpoint: row.get(4)?,
                        model: row.get(5)?,
                        state,
                        result: {
                            let status: Option<i64> = row.get(7)?;
                            let error_code: Option<String> = row.get(8)?;
                            if status.is_some() || error_code.is_some() {
                                Some(RequestResult { status, error_code })
                            } else {
                                None
                            }
                        },
                    })
                },
            )
            .map_err(CoreError::from)
    }

    fn upstream_lease_by_id(
        transaction: &Transaction<'_>, lease_id: &str,
    ) -> Result<Option<UpstreamLease>, CoreError> {
        transaction.query_row(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms FROM upstream_leases WHERE id = ?1",
            [lease_id], Self::upstream_lease_from_row,
        ).optional().map_err(CoreError::from)
    }

    fn upstream_lease_for_request_in_transaction(
        transaction: &Transaction<'_>, request_id: &str, resource_kind: &str,
    ) -> Result<Option<UpstreamLease>, CoreError> {
        transaction.query_row(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id, state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind, created_at_ms, updated_at_ms, settled_at_ms FROM upstream_leases WHERE request_id = ?1 AND resource_kind = ?2",
            params![request_id, resource_kind], Self::upstream_lease_from_row,
        ).optional().map_err(CoreError::from)
    }

    fn validate_lease_owner(
        transaction: &Transaction<'_>, lease: &UpstreamLease, principal: &crate::Principal,
    ) -> Result<(), ScheduleError> {
        let owner = transaction.query_row(
            "SELECT user_id, api_key_id FROM requests WHERE id = ?1", [&lease.request_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        ).optional()?;
        match owner {
            Some((user_id, key_id)) if user_id == principal.user_id && key_id == principal.key_id => Ok(()),
            _ => Err(ScheduleError::InvalidRequestIdentity),
        }
    }

    pub(crate) fn request_state_in_transaction(
        transaction: &Transaction<'_>, request_id: &str,
    ) -> Result<RequestState, CoreError> {
        let state = transaction.query_row(
            "SELECT state FROM requests WHERE id = ?1", [request_id], |row| row.get::<_, String>(0),
        ).optional()?.ok_or_else(|| CoreError::RequestNotFound { request_id: request_id.into() })?;
        RequestState::from_db(&state).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "requests.state".into(), value: state,
        })
    }

    pub(crate) fn select_upstream_candidate(
        transaction: &Transaction<'_>, input: &SchedulerLeaseRequest,
    ) -> Result<CandidateAccount, ScheduleError> {
        let mut statement = transaction.prepare(
            "SELECT id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency, state, cooldown_until_ms FROM upstream_accounts ORDER BY id",
        )?;
        let accounts = statement.query_map([], |row| Ok((
            row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?,
            row.get::<_, i64>(6)?, row.get::<_, String>(7)?, row.get::<_, Option<i64>>(8)?,
        )))?.collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let required_available = input.predicted_units.checked_add(input.safety_margin_units)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let mut matched_constraints = false;
        let mut usable_state = false;
        let mut fresh_observation = false;
        let mut candidates = Vec::new();
        for (id, provider, credentials_ref, region, capabilities_json, enabled, max_concurrency, state, cooldown_until_ms) in accounts {
            let capabilities: BTreeSet<String> = serde_json::from_str(&capabilities_json).map_err(|_| ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "upstream_accounts.capabilities_json".into(), value: id.clone(),
            }))?;
            if !account_matches_constraints(&id, &provider, region.as_deref(), &capabilities, input) {
                continue;
            }
            matched_constraints = true;
            if enabled == 0 || state != "available" || cooldown_until_ms.is_some_and(|until| until > input.now_ms) {
                continue;
            }
            usable_state = true;
            let observation = transaction.query_row(
                "SELECT id, observed_value, value_scale, source, status, observed_at_ms, stale_at_ms FROM upstream_observations WHERE account_ref = ?1 AND resource_kind = ?2 ORDER BY observed_at_ms DESC, id DESC LIMIT 1",
                params![&id, &input.preflight.resource_kind],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?, row.get::<_, i64>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?)),
            ).optional()?;
            let Some((observation_id, observed_value, value_scale, source, status, observed_at_ms, stale_at_ms)) = observation else { continue };
            if source != "reader" || status != ObservationStatus::Fresh.as_str() || observed_at_ms > input.now_ms || stale_at_ms <= input.now_ms
                || observed_at_ms < input.now_ms.saturating_sub(input.observation_max_age_ms) || value_scale <= 0 {
                continue;
            }
            let Some(observed_value) = observed_value else { continue };
            if observed_value < 0 { continue; }
            // Observations can carry different scales, so rank only normalized base units.
            let normalized_available_units = observed_value / value_scale;
            fresh_observation = true;
            let active_slots: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM upstream_leases WHERE account_ref = ?1 AND state IN ('held', 'active', 'unknown')",
                [&id], |row| row.get(0),
            )?;
            if normalized_available_units < required_available || active_slots >= max_concurrency {
                continue;
            }
            candidates.push(CandidateAccount {
                id, provider, credentials_ref, value_scale, normalized_available_units, observation_id,
                max_concurrency, active_slots,
            });
        }
        candidates.sort_by(|left, right| match input.selection_strategy {
            SelectionStrategy::HighestNormalizedAvailable => right.normalized_available_units.cmp(&left.normalized_available_units),
            SelectionStrategy::LeastActiveSlots => left.active_slots.cmp(&right.active_slots),
        }.then_with(|| left.id.cmp(&right.id)));
        candidates.into_iter().next().ok_or_else(|| {
            if !matched_constraints { ScheduleError::CapabilityMismatch }
            else if !usable_state { ScheduleError::AccountCooling }
            else if !fresh_observation { ScheduleError::NoFreshObservation }
            else { ScheduleError::NoUpstreamCapacity }
        })
    }
}

pub(crate) fn request_hash(endpoint: &str, model: &str, body: &Value) -> [u8; 32] {
    canonical_json_hash(&serde_json::json!({
        "endpoint": endpoint,
        "model": model,
        "body": body,
    }))
}

fn write_canonical_json(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(value) => output.extend_from_slice(value.to_string().as_bytes()),
        Value::String(value) => output.extend_from_slice(&serde_json::to_vec(value).expect("string serialization cannot fail")),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                write_canonical_json(value, output);
            }
            output.push(b']');
        }
        Value::Object(values) => {
            output.push(b'{');
            let mut keys: Vec<_> = values.keys().collect();
            keys.sort_unstable();
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    output.push(b',');
                }
                output.extend_from_slice(&serde_json::to_vec(key).expect("key serialization cannot fail"));
                output.push(b':');
                write_canonical_json(&values[key], output);
            }
            output.push(b'}');
        }
    }
}
