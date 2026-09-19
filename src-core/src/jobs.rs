use rusqlite::{params, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde_json::Value;

use crate::{
    BeginRequestInput, CoreError, CoreJob, CoreJobAttempt, CreateVideoJobInput, JobAttemptState,
    JobState, LeaseState, PreflightReserveInput, Principal, RequestResult, RequestState,
    ReservationState, ScheduleError, SchedulerLeaseRequest, SelectionStrategy, Settlement,
    UpstreamLease,
    VideoJobQueueClaim,
};
use crate::upstream::audit_hash;

pub(crate) fn validate_video_job_input(input: &CreateVideoJobInput) -> Result<(), CoreError> {
    if input.id.is_empty()
        || input.id.len() > 128
        || !input
            .id
            .bytes()
            .all(|value| value.is_ascii_alphanumeric() || matches!(value, b'-' | b'_'))
    {
        return Err(CoreError::Validation {
            field: "jobs.id".into(),
            reason: "must be a safe opaque identifier".into(),
        });
    }
    if input.input_hash.len() != 32 {
        return Err(CoreError::Validation {
            field: "jobs.input_hash".into(),
            reason: "must contain a SHA-256 digest".into(),
        });
    }
    Ok(())
}

pub(crate) fn insert_video_job_and_attempt(
    transaction: &Transaction<'_>,
    principal: &Principal,
    request_id: &str,
    model: &str,
    lease: &UpstreamLease,
    input: &CreateVideoJobInput,
    now_ms: i64,
) -> Result<(), CoreError> {
    validate_video_job_input(input)?;
    if lease.resource_kind != "video_job" || lease.state.as_str() != "held" {
        return Err(CoreError::InvalidConfiguration {
            key: "jobs.lease".into(),
            value: "video job must attach to a held video_job lease".into(),
        });
    }
    let attempt_id = crate::CoreStore::new_id("attempt");
    transaction.execute(
        "INSERT INTO jobs
         (id, request_id, user_id, kind, model, input_hash, state, reconcile_required,
          created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 'video', ?4, ?5, 'queued', 0, ?6, ?6)",
        params![
            &input.id,
            request_id,
            &principal.user_id,
            model,
            &input.input_hash,
            now_ms
        ],
    )?;
    transaction.execute(
        "INSERT INTO job_attempts
         (id, job_id, attempt_no, account_ref, lease_id, state, retryable, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, 1, ?3, ?4, 'queued', 0, ?5, ?5)",
        params![&attempt_id, &input.id, &lease.account_ref, &lease.id, now_ms],
    )?;
    crate::CoreStore::transition_request_on_connection(
        transaction,
        request_id,
        crate::RequestState::Reserved,
        crate::RequestState::Queued,
        None,
        now_ms,
    )?;
    crate::CoreStore::insert_audit_event(
        transaction,
        &principal.user_id,
        "video.job_create",
        "job",
        &audit_hash(&input.id),
        serde_json::json!({
            "job": audit_hash(&input.id),
            "request": audit_hash(request_id),
            "lease": audit_hash(&lease.id),
            "attempt": audit_hash(&attempt_id),
            "model": crate::upstream::audit_label(model),
            "state": "queued"
        }),
        now_ms,
    )?;
    Ok(())
}

pub(crate) fn insert_video_job_without_attempt(
    transaction: &Transaction<'_>,
    principal: &Principal,
    request_id: &str,
    model: &str,
    input: &CreateVideoJobInput,
    now_ms: i64,
) -> Result<(), CoreError> {
    validate_video_job_input(input)?;
    transaction.execute(
        "INSERT INTO jobs
         (id, request_id, user_id, kind, model, input_hash, state, reconcile_required,
          created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 'video', ?4, ?5, 'queued', 0, ?6, ?6)",
        params![&input.id, request_id, &principal.user_id, model, &input.input_hash, now_ms],
    )?;
    crate::CoreStore::transition_request_on_connection(
        transaction,
        request_id,
        RequestState::Reserved,
        RequestState::Queued,
        None,
        now_ms,
    )?;
    crate::CoreStore::insert_audit_event(
        transaction,
        &principal.user_id,
        "video.job_enqueue",
        "job",
        &audit_hash(&input.id),
        serde_json::json!({
            "job": audit_hash(&input.id),
            "request": audit_hash(request_id),
            "model": crate::upstream::audit_label(model),
            "state": "queued",
        }),
        now_ms,
    )?;
    Ok(())
}

impl crate::CoreStore {
    pub fn video_job_for_user(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<Option<CoreJob>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::ensure_principal_on_connection(&connection, principal)?;
        connection
            .query_row(
                "SELECT id, request_id, user_id, kind, model, input_hash, state, output_ref,
                        artifact_ref, error_code, reconcile_required, created_at_ms, updated_at_ms,
                        last_heartbeat_ms, cancel_requested_at_ms
                 FROM jobs WHERE id = ?1 AND user_id = ?2",
                params![job_id, &principal.user_id],
                job_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub fn video_job_attempt_for_user(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<Option<CoreJobAttempt>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::ensure_principal_on_connection(&connection, principal)?;
        connection
            .query_row(
                "SELECT a.id, a.job_id, a.attempt_no, a.account_ref, a.lease_id,
                        a.upstream_request_ref, a.state, a.error_code, a.retryable,
                        a.created_at_ms, a.updated_at_ms, a.last_heartbeat_ms, a.finished_at_ms
                 FROM job_attempts a
                 INNER JOIN jobs j ON j.id = a.job_id
                 WHERE a.job_id = ?1 AND j.user_id = ?2
                 ORDER BY a.attempt_no DESC LIMIT 1",
                params![job_id, &principal.user_id],
                attempt_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub(crate) fn video_job_bundle_for_user(
        &self,
        principal: &Principal,
        job_id: &str,
    ) -> Result<Option<(CoreJob, CoreJobAttempt)>, CoreError> {
        let Some(job) = self.video_job_for_user(principal, job_id)? else {
            return Ok(None);
        };
        let Some(attempt) = self.video_job_attempt_for_user(principal, job_id)? else {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "video job has no attempt".into(),
            });
        };
        Ok(Some((job, attempt)))
    }

    pub(crate) fn video_job_bundle_for_request(
        &self,
        principal: &Principal,
        request_id: &str,
    ) -> Result<Option<(CoreJob, CoreJobAttempt)>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::ensure_principal_on_connection(&connection, principal)?;
        let Some(job_id) = connection
            .query_row(
                "SELECT j.id FROM jobs j
                 INNER JOIN requests r ON r.id = j.request_id
                 WHERE j.request_id = ?1 AND j.user_id = ?2
                   AND r.user_id = ?2 AND r.api_key_id = ?3",
                params![request_id, &principal.user_id, &principal.key_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
        else {
            return Ok(None);
        };
        drop(connection);
        self.video_job_bundle_for_user(principal, &job_id)
    }

    pub fn claim_next_video_job(
        &self,
        worker_id: &str,
        now_ms: i64,
    ) -> Result<Option<VideoJobQueueClaim>, ScheduleError> {
        let worker_id = worker_id.trim();
        if worker_id.is_empty() || worker_id.len() > 128 || worker_id.chars().any(char::is_control) {
            return Err(ScheduleError::Core(CoreError::Validation {
                field: "queue.worker_id".into(),
                reason: "must be a bounded non-control identifier".into(),
            }));
        }

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let last_user_id = transaction
            .query_row(
                "SELECT last_user_id FROM dispatch_queue_cursors WHERE resource_kind = 'video_job'",
                [],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();

        let user_id = match next_video_queue_user(&transaction, last_user_id.as_deref())? {
            Some(user_id) => Some(user_id),
            None if last_user_id.is_some() => next_video_queue_user(&transaction, None)?,
            None => None,
        };
        let Some(user_id) = user_id else {
            transaction.commit()?;
            return Ok(None);
        };

        let Some(job_id) = transaction
            .query_row(
                "SELECT j.id
                 FROM jobs j
                 INNER JOIN requests r ON r.id = j.request_id
                 WHERE j.user_id = ?1
                   AND j.kind = 'video'
                   AND j.state = 'queued'
                   AND r.user_id = j.user_id
                   AND r.state = 'queued'
                 ORDER BY j.created_at_ms ASC, j.id ASC
                 LIMIT 1",
                [&user_id],
                |row| row.get::<_, String>(0),
            )
            .optional()? else {
            transaction.commit()?;
            return Ok(None);
        };

        let job = job_by_id_in_transaction(&transaction, &job_id)?.ok_or_else(|| {
            ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue".into(),
                value: "queue candidate disappeared before claim".into(),
            })
        })?;
        let (attempt, lease) = if let Some(attempt) = attempt_by_job_in_transaction(&transaction, &job.id)? {
            let lease = lease_in_transaction(&transaction, &attempt.lease_id)?.ok_or_else(|| {
                ScheduleError::Core(CoreError::ReservationNotFound {
                    reservation_id: attempt.lease_id.clone(),
                })
            })?;
            (attempt, lease)
        } else {
            let queue_input = scheduler_request_for_queued_video_job(&transaction, &job, now_ms)?;
            let reservation = Self::reservation_by_request(&transaction, &job.request_id)?
                .ok_or_else(|| ScheduleError::Core(CoreError::ReservationNotFound {
                    reservation_id: job.request_id.clone(),
                }))?;
            if reservation.user_id != job.user_id
                || reservation.resource_kind != "video_job"
                || reservation.state != ReservationState::Held
            {
                return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                    key: "jobs.reservation".into(),
                    value: "queued video job does not have a held user reservation".into(),
                }));
            }
            let candidate = Self::select_upstream_candidate(&transaction, &queue_input)?;
            let lease_expires_at_ms = now_ms
                .checked_add(queue_input.lease_ttl_ms)
                .ok_or(CoreError::InvalidQuotaAmount)?;
            let reconcile_until_ms = now_ms
                .checked_add(queue_input.reconcile_ttl_ms)
                .ok_or(CoreError::InvalidQuotaAmount)?;
            let lease = UpstreamLease {
                id: Self::new_id("lease"),
                request_id: job.request_id.clone(),
                account_ref: candidate.id.clone(),
                resource_kind: "video_job".into(),
                predicted_units: queue_input.predicted_units,
                observation_id: Some(candidate.observation_id.clone()),
                state: LeaseState::Held,
                lease_expires_at_ms,
                reconcile_until_ms: Some(reconcile_until_ms),
                upstream_request_ref: None,
                error_kind: None,
                created_at_ms: now_ms,
                updated_at_ms: now_ms,
                settled_at_ms: None,
            };
            transaction.execute(
                "INSERT INTO upstream_leases
                 (id, request_id, account_ref, resource_kind, predicted_units, observation_id,
                  state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind,
                  created_at_ms, updated_at_ms, settled_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10, ?10, NULL)",
                params![
                    &lease.id,
                    &lease.request_id,
                    &lease.account_ref,
                    &lease.resource_kind,
                    lease.predicted_units,
                    &lease.observation_id,
                    lease.state.as_str(),
                    lease.lease_expires_at_ms,
                    lease.reconcile_until_ms,
                    now_ms,
                ],
            )?;
            let attempt_id = Self::new_id("attempt");
            transaction.execute(
                "INSERT INTO job_attempts
                 (id, job_id, attempt_no, account_ref, lease_id, state, retryable,
                  created_at_ms, updated_at_ms)
                 VALUES (?1, ?2, 1, ?3, ?4, 'queued', 0, ?5, ?5)",
                params![&attempt_id, &job.id, &lease.account_ref, &lease.id, now_ms],
            )?;
            Self::insert_audit_event(
                &transaction,
                "system",
                "upstream.lease_acquire",
                "upstream_lease",
                &audit_hash(&lease.id),
                serde_json::json!({
                    "request": audit_hash(&job.request_id),
                    "lease": audit_hash(&lease.id),
                    "account": audit_hash(&lease.account_ref),
                    "provider": crate::upstream::audit_label(&candidate.provider),
                    "resource_kind": "video_job",
                    "observation": audit_hash(&candidate.observation_id),
                    "reservation": audit_hash(&reservation.id),
                }),
                now_ms,
            )?;
            let attempt = attempt_by_job_in_transaction(&transaction, &job.id)?.ok_or_else(|| {
                ScheduleError::Core(CoreError::InvalidConfiguration {
                    key: "jobs.attempt".into(),
                    value: "queue claim did not persist attempt".into(),
                })
            })?;
            (attempt, lease)
        };
        let request_state = Self::request_state_in_transaction(&transaction, &job.request_id)?;
        if job.user_id != user_id
            || job.kind != "video"
            || job.state != JobState::Queued
            || request_state != RequestState::Queued
            || attempt.job_id != job.id
            || attempt.state != JobAttemptState::Queued
            || attempt.account_ref != lease.account_ref
            || lease.request_id != job.request_id
            || lease.resource_kind != "video_job"
            || lease.state != LeaseState::Held
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue".into(),
                value: "queued video job, attempt, request, and lease are inconsistent".into(),
            }));
        }

        Self::transition_request_on_connection(
            &transaction,
            &job.request_id,
            RequestState::Queued,
            RequestState::Dispatched,
            None,
            now_ms,
        )?;
        if transaction.execute(
            "UPDATE jobs
             SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1,
                 queue_claim_owner = ?2, queue_claim_expires_at_ms = ?3
             WHERE id = ?4 AND state = 'queued'",
            params![now_ms, worker_id, lease.lease_expires_at_ms, &job.id],
        )? != 1 {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: "queue claim could not transition job".into(),
            }));
        }
        if transaction.execute(
            "UPDATE job_attempts
             SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1
             WHERE id = ?2 AND state = 'queued'",
            params![now_ms, &attempt.id],
        )? != 1 {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "job_attempts.state".into(),
                value: "queue claim could not transition attempt".into(),
            }));
        }
        if transaction.execute(
            "UPDATE upstream_leases
             SET state = 'active', updated_at_ms = ?1
             WHERE id = ?2 AND state = 'held'",
            params![now_ms, &lease.id],
        )? != 1 {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "upstream_leases.state".into(),
                value: "queue claim could not activate lease".into(),
            }));
        }
        transaction.execute(
            "INSERT INTO dispatch_queue_cursors (resource_kind, last_user_id, updated_at_ms)
             VALUES ('video_job', ?1, ?2)
             ON CONFLICT(resource_kind) DO UPDATE SET
               last_user_id = excluded.last_user_id,
               updated_at_ms = excluded.updated_at_ms",
            params![&job.user_id, now_ms],
        )?;
        Self::insert_audit_event(
            &transaction,
            "system",
            "video.job_claim",
            "job",
            &audit_hash(&job.id),
            serde_json::json!({
                "worker": audit_hash(worker_id),
                "job": audit_hash(&job.id),
                "attempt": audit_hash(&attempt.id),
                "lease": audit_hash(&lease.id),
            }),
            now_ms,
        )?;

        let claimed_job = job_by_id_in_transaction(&transaction, &job.id)?.expect("claimed job remains in transaction");
        let claimed_attempt = attempt_by_job_in_transaction(&transaction, &job.id)?.expect("claimed attempt remains in transaction");
        let claimed_lease = lease_in_transaction(&transaction, &lease.id)?.expect("claimed lease remains in transaction");
        transaction.commit()?;
        Ok(Some(VideoJobQueueClaim {
            job: claimed_job,
            attempt: claimed_attempt,
            lease: claimed_lease,
        }))
    }

    pub fn claim_video_job_by_id(
        &self,
        worker_id: &str,
        job_id: &str,
        now_ms: i64,
    ) -> Result<Option<VideoJobQueueClaim>, ScheduleError> {
        let worker_id = worker_id.trim();
        if worker_id.is_empty() || worker_id.len() > 128 || worker_id.chars().any(char::is_control) {
            return Err(ScheduleError::Core(CoreError::Validation {
                field: "queue.worker_id".into(),
                reason: "must be a bounded non-control identifier".into(),
            }));
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(candidate_id) = transaction
            .query_row(
                "SELECT j.id
                 FROM jobs j INNER JOIN requests r ON r.id = j.request_id
                 INNER JOIN users u ON u.id = j.user_id
                 WHERE j.id = ?1 AND j.kind = 'video' AND j.state = 'queued'
                   AND r.user_id = j.user_id AND r.state = 'queued' AND u.status = 'active'",
                [job_id],
                |row| row.get::<_, String>(0),
            )
            .optional()? else {
            transaction.commit()?;
            return Ok(None);
        };
        let claim = claim_video_job_in_transaction(&transaction, worker_id, now_ms, &candidate_id)?;
        transaction.commit()?;
        Ok(Some(claim))
    }

    pub fn heartbeat_video_job(
        &self,
        worker_id: &str,
        job_id: &str,
        now_ms: i64,
        lease_ttl_ms: i64,
    ) -> Result<CoreJob, ScheduleError> {
        let worker_id = worker_id.trim();
        if worker_id.is_empty() || worker_id.len() > 128 || worker_id.chars().any(char::is_control) {
            return Err(ScheduleError::Core(CoreError::Validation {
                field: "queue.worker_id".into(),
                reason: "must be a bounded non-control identifier".into(),
            }));
        }
        if lease_ttl_ms <= 0 {
            return Err(ScheduleError::Core(CoreError::InvalidQuotaAmount));
        }
        let expires_at_ms = now_ms
            .checked_add(lease_ttl_ms)
            .ok_or(CoreError::InvalidQuotaAmount)?;

        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let job = job_by_id_in_transaction(&transaction, job_id)?
            .ok_or_else(|| CoreError::RequestNotFound { request_id: job_id.into() })?;
        if !matches!(job.state, JobState::Running | JobState::CancelRequested) {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: "video job heartbeat requires a running or cancel-requested job".into(),
            }));
        }
        let (queue_claim_owner, queue_claim_expires_at_ms) = transaction.query_row(
            "SELECT queue_claim_owner, queue_claim_expires_at_ms FROM jobs WHERE id = ?1",
            [job_id],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )?;
        if queue_claim_owner.as_deref() != Some(worker_id)
            || queue_claim_expires_at_ms.map_or(true, |value| value <= now_ms)
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue_claim".into(),
                value: "video job heartbeat owner does not hold a live queue claim".into(),
            }));
        }
        let attempt = attempt_by_job_in_transaction(&transaction, job_id)?.ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "running video job has no attempt".into(),
            }
        })?;
        if !matches!(attempt.state, JobAttemptState::Running | JobAttemptState::CancelRequested) {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "job_attempts.state".into(),
                value: "video job heartbeat requires an active attempt".into(),
            }));
        }
        let lease = lease_in_transaction(&transaction, &attempt.lease_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: attempt.lease_id.clone(),
            }
        })?;
        if lease.request_id != job.request_id
            || lease.resource_kind != "video_job"
            || lease.state != LeaseState::Active
            || lease.lease_expires_at_ms <= now_ms
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.lease".into(),
                value: "video job heartbeat requires a live active lease".into(),
            }));
        }
        if transaction.execute(
            "UPDATE jobs
             SET updated_at_ms = ?1, last_heartbeat_ms = ?1, queue_claim_expires_at_ms = ?2
             WHERE id = ?3 AND queue_claim_owner = ?4
               AND state IN ('running', 'cancel_requested')",
            params![now_ms, expires_at_ms, job_id, worker_id],
        )? != 1
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue_claim".into(),
                value: "video job heartbeat lost its queue claim".into(),
            }));
        }
        if transaction.execute(
            "UPDATE job_attempts
             SET updated_at_ms = ?1, last_heartbeat_ms = ?1
             WHERE id = ?2 AND state IN ('running', 'cancel_requested')",
            params![now_ms, attempt.id],
        )? != 1
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "job_attempts.state".into(),
                value: "video job heartbeat lost its attempt".into(),
            }));
        }
        if transaction.execute(
            "UPDATE upstream_leases
             SET lease_expires_at_ms = ?1, updated_at_ms = ?2
             WHERE id = ?3 AND state = 'active'",
            params![expires_at_ms, now_ms, lease.id],
        )? != 1
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.lease".into(),
                value: "video job heartbeat lost its active lease".into(),
            }));
        }
        Self::insert_audit_event(
            &transaction,
            "system",
            "video.job_heartbeat",
            "job",
            &audit_hash(job_id),
            serde_json::json!({
                "worker": audit_hash(worker_id),
                "job": audit_hash(job_id),
                "attempt": audit_hash(&attempt.id),
                "lease": audit_hash(&lease.id),
                "expires_at_ms": expires_at_ms,
            }),
            now_ms,
        )?;
        let updated = job_by_id_in_transaction(&transaction, job_id)?.expect("job remains in transaction");
        transaction.commit()?;
        Ok(updated)
    }

    pub fn mark_video_job_running(
        &self,
        principal: &Principal,
        job_id: &str,
        now_ms: i64,
    ) -> Result<CoreJob, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        let job = job_in_transaction(&transaction, principal, job_id)?
            .ok_or_else(|| CoreError::RequestNotFound { request_id: job_id.into() })?;
        if job.state == JobState::Running {
            transaction.commit()?;
            return Ok(job);
        }
        if job.state != JobState::Queued {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: format!("cannot start job {} from {:?}", job.id, job.state),
            });
        }
        let attempt = attempt_in_transaction(&transaction, principal, job_id)?.ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "queued video job has no attempt".into(),
            }
        })?;
        let lease = lease_in_transaction(&transaction, &attempt.lease_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: attempt.lease_id.clone(),
            }
        })?;
        if lease.account_ref != attempt.account_ref || lease.resource_kind != "video_job" {
            return Err(CoreError::InvalidConfiguration {
                key: "job_attempts.lease".into(),
                value: "attempt lease/account/resource mismatch".into(),
            });
        }
        let request_state = Self::request_state_in_transaction(&transaction, &job.request_id)?;
        if request_state == RequestState::Queued {
            Self::transition_request_on_connection(
                &transaction,
                &job.request_id,
                RequestState::Queued,
                RequestState::Dispatched,
                None,
                now_ms,
            )?;
        } else if request_state != RequestState::Dispatched {
            return Err(CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: format!("video job request is {request_state:?}"),
            });
        }
        transaction.execute(
            "UPDATE jobs SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1 WHERE id = ?2 AND state = 'queued'",
            params![now_ms, job_id],
        )?;
        transaction.execute(
            "UPDATE job_attempts SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1 WHERE id = ?2 AND state = 'queued'",
            params![now_ms, attempt.id],
        )?;
        transaction.execute(
            "UPDATE upstream_leases SET state = 'active', updated_at_ms = ?1 WHERE id = ?2 AND state = 'held'",
            params![now_ms, lease.id],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "video.job_running",
            "job",
            &audit_hash(job_id),
            serde_json::json!({"job": audit_hash(job_id), "attempt": audit_hash(&attempt.id), "lease": audit_hash(&lease.id)}),
            now_ms,
        )?;
        let updated = job_in_transaction(&transaction, principal, job_id)?.expect("job remains in transaction");
        transaction.commit()?;
        Ok(updated)
    }

    pub fn record_video_job_acceptance(
        &self,
        principal: &Principal,
        job_id: &str,
        upstream_request_ref: &str,
        now_ms: i64,
    ) -> Result<CoreJob, CoreError> {
        let upstream_request_ref = upstream_request_ref.trim();
        if upstream_request_ref.is_empty()
            || upstream_request_ref.len() > 128
            || !upstream_request_ref.chars().all(|value| {
                value.is_ascii_alphanumeric() || matches!(value, '-' | '_' | '.' | ':')
            })
        {
            return Err(CoreError::Validation {
                field: "job_attempts.upstream_request_ref".into(),
                reason: "must be a bounded opaque upstream reference".into(),
            });
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        let job = job_in_transaction(&transaction, principal, job_id)?
            .ok_or_else(|| CoreError::RequestNotFound { request_id: job_id.into() })?;
        let attempt = attempt_in_transaction(&transaction, principal, job_id)?.ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "video job has no attempt".into(),
            }
        })?;
        if !matches!(job.state, JobState::Running | JobState::CancelRequested)
            || !matches!(attempt.state, JobAttemptState::Running | JobAttemptState::CancelRequested)
        {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: "upstream acceptance requires a running video attempt".into(),
            });
        }
        transaction.execute(
            "UPDATE job_attempts SET upstream_request_ref = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![upstream_request_ref, now_ms, attempt.id],
        )?;
        transaction.execute(
            "UPDATE upstream_leases SET upstream_request_ref = ?1, updated_at_ms = ?2 WHERE id = ?3 AND state = 'active'",
            params![upstream_request_ref, now_ms, attempt.lease_id],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "video.job_accepted",
            "job",
            &audit_hash(job_id),
            serde_json::json!({
                "job": audit_hash(job_id),
                "attempt": audit_hash(&attempt.id),
                "upstream_request": audit_hash(upstream_request_ref),
            }),
            now_ms,
        )?;
        let updated = job_in_transaction(&transaction, principal, job_id)?.expect("job remains in transaction");
        transaction.commit()?;
        Ok(updated)
    }

    pub fn set_video_job_result_refs(
        &self,
        principal: &Principal,
        job_id: &str,
        output_ref: Option<&str>,
        artifact_ref: Option<&str>,
        now_ms: i64,
    ) -> Result<CoreJob, CoreError> {
        for (field, value) in [("jobs.output_ref", output_ref), ("jobs.artifact_ref", artifact_ref)] {
            if let Some(value) = value {
                if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                    return Err(CoreError::Validation {
                        field: field.into(),
                        reason: "must be a bounded non-control reference".into(),
                    });
                }
            }
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        let job = job_in_transaction(&transaction, principal, job_id)?
            .ok_or_else(|| CoreError::RequestNotFound { request_id: job_id.into() })?;
        if job.state != JobState::Succeeded {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: "result references require a succeeded video job".into(),
            });
        }
        transaction.execute(
            "UPDATE jobs SET output_ref = ?1, artifact_ref = ?2, updated_at_ms = ?3 WHERE id = ?4",
            params![output_ref, artifact_ref, now_ms, job_id],
        )?;
        let updated = job_in_transaction(&transaction, principal, job_id)?.expect("job remains in transaction");
        transaction.commit()?;
        Ok(updated)
    }

    pub fn request_video_cancel(
        &self,
        principal: &Principal,
        job_id: &str,
        now_ms: i64,
    ) -> Result<CoreJob, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        let job = job_in_transaction(&transaction, principal, job_id)?
            .ok_or_else(|| CoreError::RequestNotFound { request_id: job_id.into() })?;
        if matches!(job.state, JobState::Succeeded | JobState::Failed | JobState::Canceled | JobState::Unknown) {
            transaction.commit()?;
            return Ok(job);
        }
        if job.state == JobState::CancelRequested {
            transaction.commit()?;
            return Ok(job);
        }
        let attempt = attempt_in_transaction(&transaction, principal, job_id)?;
        let request_state = Self::request_state_in_transaction(&transaction, &job.request_id)?;
        if !matches!(request_state, RequestState::Queued | RequestState::Dispatched | RequestState::Completing) {
            return Err(CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: format!("cannot cancel video request in {request_state:?}"),
            });
        }
        Self::transition_request_on_connection(
            &transaction,
            &job.request_id,
            request_state,
            RequestState::CancelRequested,
            None,
            now_ms,
        )?;
        transaction.execute(
            "UPDATE jobs SET state = 'cancel_requested', cancel_requested_at_ms = ?1, updated_at_ms = ?1 WHERE id = ?2",
            params![now_ms, job_id],
        )?;
        if let Some(attempt) = &attempt {
            transaction.execute(
                "UPDATE job_attempts SET state = 'cancel_requested', updated_at_ms = ?1 WHERE id = ?2 AND state IN ('queued','running')",
                params![now_ms, attempt.id],
            )?;
        } else {
            let reservation = Self::reservation_by_request(&transaction, &job.request_id)?
                .ok_or_else(|| CoreError::ReservationNotFound {
                    reservation_id: job.request_id.clone(),
                })?;
            if reservation.state == ReservationState::Held {
                Self::apply_settlement(
                    &transaction,
                    &reservation,
                    Settlement::Release,
                    now_ms,
                )?;
            }
            transaction.execute(
                "UPDATE jobs SET state = 'canceled', updated_at_ms = ?1 WHERE id = ?2 AND state = 'cancel_requested'",
                params![now_ms, job_id],
            )?;
            Self::settle_request_state(
                &transaction,
                &job.request_id,
                RequestState::CancelRequested,
                RequestState::Canceled,
                Some(RequestResult {
                    status: Some(499),
                    error_code: Some("canceled".into()),
                }),
                now_ms,
            )?;
        }
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "video.job_cancel_requested",
            "job",
            &audit_hash(job_id),
            serde_json::json!({
                "job": audit_hash(job_id),
                "attempt": attempt.as_ref().map(|attempt| audit_hash(&attempt.id)),
                "reason": "client_cancel"
            }),
            now_ms,
        )?;
        let updated = job_in_transaction(&transaction, principal, job_id)?.expect("job remains in transaction");
        transaction.commit()?;
        Ok(updated)
    }

    pub(crate) fn sync_video_job_settlement(
        transaction: &Transaction<'_>,
        lease: &UpstreamLease,
        job_state: JobState,
        attempt_state: JobAttemptState,
        error_code: Option<&str>,
        upstream_request_ref: Option<&str>,
        reconcile_required: bool,
        now_ms: i64,
    ) -> Result<(), CoreError> {
        let Some(job) = job_for_request_in_transaction(transaction, &lease.request_id)? else {
            return Ok(());
        };
        let Some(attempt) = attempt_for_lease_in_transaction(transaction, &lease.id)? else {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "video lease has no attempt".into(),
            });
        };
        if job.user_id != job_for_request_user(transaction, &lease.request_id)? {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.user_id".into(),
                value: "job owner does not match request owner".into(),
            });
        }
        if !job.state.can_transition_to(job_state) && job.state != job_state {
            return Err(CoreError::InvalidConfiguration {
                key: "jobs.state".into(),
                value: format!("cannot settle {:?} as {job_state:?}", job.state),
            });
        }
        transaction.execute(
            "UPDATE jobs SET state = ?1, error_code = ?2, reconcile_required = ?3, updated_at_ms = ?4,
                    last_heartbeat_ms = ?4, queue_claim_owner = NULL,
                    queue_claim_expires_at_ms = NULL WHERE id = ?5",
            params![job_state.as_str(), error_code, if reconcile_required { 1 } else { 0 }, now_ms, job.id],
        )?;
        transaction.execute(
            "UPDATE job_attempts SET state = ?1, error_code = ?2, upstream_request_ref = ?3,
                    updated_at_ms = ?4, finished_at_ms = ?5 WHERE id = ?6",
            params![
                attempt_state.as_str(),
                error_code,
                upstream_request_ref,
                now_ms,
                if matches!(attempt_state, JobAttemptState::Unknown) { None } else { Some(now_ms) },
                attempt.id
            ],
        )?;
        Ok(())
    }
}

fn claim_video_job_in_transaction(
    transaction: &Transaction<'_>,
    worker_id: &str,
    now_ms: i64,
    job_id: &str,
) -> Result<VideoJobQueueClaim, ScheduleError> {
    let job = job_by_id_in_transaction(transaction, job_id)?.ok_or_else(|| {
        ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "jobs.queue".into(),
            value: "queue candidate disappeared before claim".into(),
        })
    })?;
    let (attempt, lease) = if let Some(attempt) = attempt_by_job_in_transaction(transaction, &job.id)? {
        let lease = lease_in_transaction(transaction, &attempt.lease_id)?.ok_or_else(|| {
            ScheduleError::Core(CoreError::ReservationNotFound {
                reservation_id: attempt.lease_id.clone(),
            })
        })?;
        (attempt, lease)
    } else {
        let queue_input = scheduler_request_for_queued_video_job(transaction, &job, now_ms)?;
        let reservation = crate::CoreStore::reservation_by_request(transaction, &job.request_id)?
            .ok_or_else(|| ScheduleError::Core(CoreError::ReservationNotFound {
                reservation_id: job.request_id.clone(),
            }))?;
        if reservation.user_id != job.user_id
            || reservation.resource_kind != "video_job"
            || reservation.state != ReservationState::Held
        {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.reservation".into(),
                value: "queued video job does not have a held user reservation".into(),
            }));
        }
        let candidate = crate::CoreStore::select_upstream_candidate(transaction, &queue_input)?;
        let lease_expires_at_ms = now_ms
            .checked_add(queue_input.lease_ttl_ms)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let reconcile_until_ms = now_ms
            .checked_add(queue_input.reconcile_ttl_ms)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        let lease = UpstreamLease {
            id: crate::CoreStore::new_id("lease"),
            request_id: job.request_id.clone(),
            account_ref: candidate.id.clone(),
            resource_kind: "video_job".into(),
            predicted_units: queue_input.predicted_units,
            observation_id: Some(candidate.observation_id.clone()),
            state: LeaseState::Held,
            lease_expires_at_ms,
            reconcile_until_ms: Some(reconcile_until_ms),
            upstream_request_ref: None,
            error_kind: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
            settled_at_ms: None,
        };
        transaction.execute(
            "INSERT INTO upstream_leases
             (id, request_id, account_ref, resource_kind, predicted_units, observation_id,
              state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref, error_kind,
              created_at_ms, updated_at_ms, settled_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, NULL, ?10, ?10, NULL)",
            params![
                &lease.id,
                &lease.request_id,
                &lease.account_ref,
                &lease.resource_kind,
                lease.predicted_units,
                &lease.observation_id,
                lease.state.as_str(),
                lease.lease_expires_at_ms,
                lease.reconcile_until_ms,
                now_ms,
            ],
        )?;
        let attempt_id = crate::CoreStore::new_id("attempt");
        transaction.execute(
            "INSERT INTO job_attempts
             (id, job_id, attempt_no, account_ref, lease_id, state, retryable,
              created_at_ms, updated_at_ms)
             VALUES (?1, ?2, 1, ?3, ?4, 'queued', 0, ?5, ?5)",
            params![&attempt_id, &job.id, &lease.account_ref, &lease.id, now_ms],
        )?;
        crate::CoreStore::insert_audit_event(
            transaction,
            "system",
            "upstream.lease_acquire",
            "upstream_lease",
            &audit_hash(&lease.id),
            serde_json::json!({
                "request": audit_hash(&job.request_id),
                "lease": audit_hash(&lease.id),
                "account": audit_hash(&lease.account_ref),
                "provider": crate::upstream::audit_label(&candidate.provider),
                "resource_kind": "video_job",
                "observation": audit_hash(&candidate.observation_id),
                "reservation": audit_hash(&reservation.id),
            }),
            now_ms,
        )?;
        let attempt = attempt_by_job_in_transaction(transaction, &job.id)?.ok_or_else(|| {
            ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "queue claim did not persist attempt".into(),
            })
        })?;
        (attempt, lease)
    };
    let request_state = crate::CoreStore::request_state_in_transaction(transaction, &job.request_id)?;
    if job.kind != "video"
        || job.state != JobState::Queued
        || request_state != RequestState::Queued
        || attempt.job_id != job.id
        || attempt.state != JobAttemptState::Queued
        || attempt.account_ref != lease.account_ref
        || lease.request_id != job.request_id
        || lease.resource_kind != "video_job"
        || lease.state != LeaseState::Held
    {
        return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "jobs.queue".into(),
            value: "queued video job, attempt, request, and lease are inconsistent".into(),
        }));
    }
    crate::CoreStore::transition_request_on_connection(
        transaction,
        &job.request_id,
        RequestState::Queued,
        RequestState::Dispatched,
        None,
        now_ms,
    )?;
    if transaction.execute(
        "UPDATE jobs
         SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1,
             queue_claim_owner = ?2, queue_claim_expires_at_ms = ?3
         WHERE id = ?4 AND state = 'queued'",
        params![now_ms, worker_id, lease.lease_expires_at_ms, &job.id],
    )? != 1 {
        return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "jobs.state".into(),
            value: "queue claim could not transition job".into(),
        }));
    }
    if transaction.execute(
        "UPDATE job_attempts
         SET state = 'running', updated_at_ms = ?1, last_heartbeat_ms = ?1
         WHERE id = ?2 AND state = 'queued'",
        params![now_ms, &attempt.id],
    )? != 1 {
        return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "job_attempts.state".into(),
            value: "queue claim could not transition attempt".into(),
        }));
    }
    if transaction.execute(
        "UPDATE upstream_leases SET state = 'active', updated_at_ms = ?1
         WHERE id = ?2 AND state = 'held'",
        params![now_ms, &lease.id],
    )? != 1 {
        return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "upstream_leases.state".into(),
            value: "queue claim could not activate lease".into(),
        }));
    }
    transaction.execute(
        "INSERT INTO dispatch_queue_cursors (resource_kind, last_user_id, updated_at_ms)
         VALUES ('video_job', ?1, ?2)
         ON CONFLICT(resource_kind) DO UPDATE SET
           last_user_id = excluded.last_user_id,
           updated_at_ms = excluded.updated_at_ms",
        params![&job.user_id, now_ms],
    )?;
    crate::CoreStore::insert_audit_event(
        transaction,
        "system",
        "video.job_claim",
        "job",
        &audit_hash(&job.id),
        serde_json::json!({
            "worker": audit_hash(worker_id),
            "job": audit_hash(&job.id),
            "attempt": audit_hash(&attempt.id),
            "lease": audit_hash(&lease.id),
        }),
        now_ms,
    )?;
    let claimed_job = job_by_id_in_transaction(transaction, &job.id)?.expect("claimed job remains in transaction");
    let claimed_attempt = attempt_by_job_in_transaction(transaction, &job.id)?.expect("claimed attempt remains in transaction");
    let claimed_lease = lease_in_transaction(transaction, &lease.id)?.expect("claimed lease remains in transaction");
    Ok(VideoJobQueueClaim {
        job: claimed_job,
        attempt: claimed_attempt,
        lease: claimed_lease,
    })
}

fn scheduler_request_for_queued_video_job(
    transaction: &Transaction<'_>,
    job: &CoreJob,
    now_ms: i64,
) -> Result<SchedulerLeaseRequest, ScheduleError> {
    let (
        user_id,
        api_key_id,
        protocol,
        endpoint,
        provider_hint,
        required_capabilities_json,
        region,
        predicted_units,
        safety_margin_units,
        observation_max_age_ms,
        allowed_accounts_json,
        dedicated_account,
        selection_strategy,
        lease_ttl_ms,
        reconcile_ttl_ms,
    ) = transaction.query_row(
        "SELECT r.user_id, r.api_key_id, r.protocol, r.endpoint,
                j.queue_provider_hint, j.queue_required_capabilities_json, j.queue_region,
                j.queue_predicted_units, j.queue_safety_margin_units, j.queue_observation_max_age_ms,
                j.queue_allowed_accounts_json, j.queue_dedicated_account, j.queue_selection_strategy,
                j.queue_lease_ttl_ms, j.queue_reconcile_ttl_ms
         FROM jobs j INNER JOIN requests r ON r.id = j.request_id
         WHERE j.id = ?1",
        [&job.id],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<i64>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, Option<String>>(12)?,
                row.get::<_, Option<i64>>(13)?,
                row.get::<_, Option<i64>>(14)?,
            ))
        },
    )?;
    if user_id != job.user_id || endpoint != "videos" {
        return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "jobs.queue.request".into(),
            value: "queued video job request ownership or endpoint is invalid".into(),
        }));
    }
    let required_capabilities_json = required_capabilities_json.ok_or_else(|| {
        CoreError::InvalidConfiguration {
            key: "jobs.queue.required_capabilities".into(),
            value: "queued video job has no scheduler constraints".into(),
        }
    })?;
    let required_capabilities = serde_json::from_str(&required_capabilities_json).map_err(|_| {
        ScheduleError::Core(CoreError::InvalidConfiguration {
            key: "jobs.queue.required_capabilities".into(),
            value: "queued video job has invalid scheduler constraints".into(),
        })
    })?;
    let allowed_accounts = allowed_accounts_json
        .map(|value| serde_json::from_str(&value))
        .transpose()
        .map_err(|_| {
            ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue.allowed_accounts".into(),
                value: "queued video job has invalid account constraints".into(),
            })
        })?;
    let selection_strategy = match selection_strategy.as_deref() {
        Some("highest_normalized_available") => SelectionStrategy::HighestNormalizedAvailable,
        Some("least_active_slots") => SelectionStrategy::LeastActiveSlots,
        _ => {
            return Err(ScheduleError::Core(CoreError::InvalidConfiguration {
                key: "jobs.queue.selection_strategy".into(),
                value: "queued video job has an invalid selection strategy".into(),
            }))
        }
    };
    let predicted_units = predicted_units.ok_or(CoreError::InvalidQuotaAmount)?;
    let safety_margin_units = safety_margin_units.ok_or(CoreError::InvalidQuotaAmount)?;
    let observation_max_age_ms = observation_max_age_ms.ok_or(CoreError::InvalidQuotaAmount)?;
    let lease_ttl_ms = lease_ttl_ms.ok_or(CoreError::InvalidQuotaAmount)?;
    let reconcile_ttl_ms = reconcile_ttl_ms.ok_or(CoreError::InvalidQuotaAmount)?;
    if predicted_units <= 0
        || safety_margin_units < 0
        || observation_max_age_ms < 0
        || lease_ttl_ms <= 0
        || reconcile_ttl_ms <= 0
    {
        return Err(ScheduleError::Core(CoreError::InvalidQuotaAmount));
    }
    let amount: i64 = transaction.query_row(
        "SELECT amount FROM quota_reservations WHERE request_id = ?1",
        [&job.request_id],
        |row| row.get(0),
    )?;
    Ok(SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id,
                api_key_id,
                protocol,
                endpoint,
                model: job.model.clone(),
                idempotency_key: "queue-claim".into(),
                body: Value::Null,
            },
            resource_kind: "video_job".into(),
            amount,
            ttl_ms: 0,
        },
        provider_hint,
        required_capabilities,
        region,
        predicted_units,
        safety_margin_units,
        observation_max_age_ms,
        allowed_accounts,
        dedicated_account,
        selection_strategy,
        now_ms,
        lease_ttl_ms,
        reconcile_ttl_ms,
    })
}

fn next_video_queue_user(
    transaction: &Transaction<'_>,
    after_user_id: Option<&str>,
) -> Result<Option<String>, CoreError> {
    transaction
        .query_row(
            "WITH queued_users AS (
                 SELECT j.user_id, MIN(j.created_at_ms) AS earliest_created_at_ms
                 FROM jobs j
                 INNER JOIN requests r ON r.id = j.request_id
                 INNER JOIN users u ON u.id = j.user_id
                 WHERE j.kind = 'video'
                   AND j.state = 'queued'
                   AND r.user_id = j.user_id
                   AND r.state = 'queued'
                   AND u.status = 'active'
                 GROUP BY j.user_id
             )
             SELECT user_id
             FROM queued_users
             WHERE (?1 IS NULL OR user_id > ?1)
             ORDER BY earliest_created_at_ms ASC, user_id ASC
             LIMIT 1",
            [after_user_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(CoreError::from)
}

fn job_by_id_in_transaction(
    transaction: &Transaction<'_>,
    job_id: &str,
) -> Result<Option<CoreJob>, CoreError> {
    transaction
        .query_row(
            "SELECT id, request_id, user_id, kind, model, input_hash, state,
                    output_ref, artifact_ref, error_code, reconcile_required,
                    created_at_ms, updated_at_ms, last_heartbeat_ms, cancel_requested_at_ms
             FROM jobs WHERE id = ?1",
            [job_id],
            job_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn attempt_by_job_in_transaction(
    transaction: &Transaction<'_>,
    job_id: &str,
) -> Result<Option<CoreJobAttempt>, CoreError> {
    transaction
        .query_row(
            "SELECT id, job_id, attempt_no, account_ref, lease_id,
                    upstream_request_ref, state, error_code, retryable,
                    created_at_ms, updated_at_ms, last_heartbeat_ms, finished_at_ms
             FROM job_attempts
             WHERE job_id = ?1
             ORDER BY attempt_no DESC
             LIMIT 1",
            [job_id],
            attempt_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn job_in_transaction(
    transaction: &Transaction<'_>,
    principal: &Principal,
    job_id: &str,
) -> Result<Option<CoreJob>, CoreError> {
    transaction
        .query_row(
            "SELECT j.id, j.request_id, j.user_id, j.kind, j.model, j.input_hash, j.state,
                    j.output_ref, j.artifact_ref, j.error_code, j.reconcile_required,
                    j.created_at_ms, j.updated_at_ms, j.last_heartbeat_ms, j.cancel_requested_at_ms
             FROM jobs j INNER JOIN requests r ON r.id = j.request_id
             WHERE j.id = ?1 AND j.user_id = ?2 AND r.user_id = ?2 AND r.api_key_id = ?3",
            params![job_id, &principal.user_id, &principal.key_id],
            job_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn attempt_in_transaction(
    transaction: &Transaction<'_>,
    principal: &Principal,
    job_id: &str,
) -> Result<Option<CoreJobAttempt>, CoreError> {
    transaction
        .query_row(
            "SELECT a.id, a.job_id, a.attempt_no, a.account_ref, a.lease_id,
                    a.upstream_request_ref, a.state, a.error_code, a.retryable,
                    a.created_at_ms, a.updated_at_ms, a.last_heartbeat_ms, a.finished_at_ms
             FROM job_attempts a INNER JOIN jobs j ON j.id = a.job_id
             INNER JOIN requests r ON r.id = j.request_id
             WHERE a.job_id = ?1 AND j.user_id = ?2 AND r.user_id = ?2 AND r.api_key_id = ?3
             ORDER BY a.attempt_no DESC LIMIT 1",
            params![job_id, &principal.user_id, &principal.key_id],
            attempt_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn lease_in_transaction(
    transaction: &Transaction<'_>,
    lease_id: &str,
) -> Result<Option<UpstreamLease>, CoreError> {
    transaction
        .query_row(
            "SELECT id, request_id, account_ref, resource_kind, predicted_units, observation_id,
                    state, lease_expires_at_ms, reconcile_until_ms, upstream_request_ref,
                    error_kind, created_at_ms, updated_at_ms, settled_at_ms
             FROM upstream_leases WHERE id = ?1",
            [lease_id],
            crate::CoreStore::upstream_lease_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

pub(crate) fn job_for_request_in_transaction(
    transaction: &Transaction<'_>,
    request_id: &str,
) -> Result<Option<CoreJob>, CoreError> {
    transaction
        .query_row(
            "SELECT id, request_id, user_id, kind, model, input_hash, state, output_ref,
                    artifact_ref, error_code, reconcile_required, created_at_ms, updated_at_ms,
                    last_heartbeat_ms, cancel_requested_at_ms
             FROM jobs WHERE request_id = ?1",
            [request_id],
            job_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn attempt_for_lease_in_transaction(
    transaction: &Transaction<'_>,
    lease_id: &str,
) -> Result<Option<CoreJobAttempt>, CoreError> {
    transaction
        .query_row(
            "SELECT id, job_id, attempt_no, account_ref, lease_id, upstream_request_ref,
                    state, error_code, retryable, created_at_ms, updated_at_ms,
                    last_heartbeat_ms, finished_at_ms
             FROM job_attempts WHERE lease_id = ?1",
            [lease_id],
            attempt_from_row,
        )
        .optional()
        .map_err(CoreError::from)
}

fn job_for_request_user(transaction: &Transaction<'_>, request_id: &str) -> Result<String, CoreError> {
    transaction
        .query_row("SELECT user_id FROM requests WHERE id = ?1", [request_id], |row| row.get(0))
        .map_err(CoreError::from)
}

fn job_from_row(row: &Row<'_>) -> rusqlite::Result<CoreJob> {
    let state = row.get::<_, String>(6)?;
    let state = JobState::from_db(&state).ok_or_else(|| invalid_state("jobs.state"))?;
    Ok(CoreJob {
        id: row.get(0)?,
        request_id: row.get(1)?,
        user_id: row.get(2)?,
        kind: row.get(3)?,
        model: row.get(4)?,
        input_hash: row.get(5)?,
        state,
        output_ref: row.get(7)?,
        artifact_ref: row.get(8)?,
        error_code: row.get(9)?,
        reconcile_required: row.get::<_, i64>(10)? != 0,
        created_at_ms: row.get(11)?,
        updated_at_ms: row.get(12)?,
        last_heartbeat_ms: row.get(13)?,
        cancel_requested_at_ms: row.get(14)?,
    })
}

fn attempt_from_row(row: &Row<'_>) -> rusqlite::Result<CoreJobAttempt> {
    let state = row.get::<_, String>(6)?;
    let state = JobAttemptState::from_db(&state).ok_or_else(|| invalid_state("job_attempts.state"))?;
    Ok(CoreJobAttempt {
        id: row.get(0)?,
        job_id: row.get(1)?,
        attempt_no: row.get(2)?,
        account_ref: row.get(3)?,
        lease_id: row.get(4)?,
        upstream_request_ref: row.get(5)?,
        state,
        error_code: row.get(7)?,
        retryable: row.get::<_, i64>(8)? != 0,
        created_at_ms: row.get(9)?,
        updated_at_ms: row.get(10)?,
        last_heartbeat_ms: row.get(11)?,
        finished_at_ms: row.get(12)?,
    })
}

fn invalid_state(field: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            field,
        )),
    )
}
