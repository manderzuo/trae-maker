use rusqlite::{params, OptionalExtension, Row, Transaction, TransactionBehavior};

use crate::{
    CoreError, CoreJob, CoreJobAttempt, CreateVideoJobInput, JobAttemptState, JobState, Principal,
    RequestState, UpstreamLease,
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
        let attempt = attempt_in_transaction(&transaction, principal, job_id)?.ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "jobs.attempt".into(),
                value: "video job has no attempt".into(),
            }
        })?;
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
        transaction.execute(
            "UPDATE job_attempts SET state = 'cancel_requested', updated_at_ms = ?1 WHERE id = ?2 AND state IN ('queued','running')",
            params![now_ms, attempt.id],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "video.job_cancel_requested",
            "job",
            &audit_hash(job_id),
            serde_json::json!({"job": audit_hash(job_id), "attempt": audit_hash(&attempt.id), "reason": "client_cancel"}),
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
                    last_heartbeat_ms = ?4 WHERE id = ?5",
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

fn job_for_request_in_transaction(
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
