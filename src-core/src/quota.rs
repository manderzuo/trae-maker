use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    CoreError, CoreStore, Principal, QuotaBalance, QuotaGrant, QuotaReserve, RequestResult,
    RequestState, Reservation, ReservationState, ReserveResult, Settlement,
};

impl CoreStore {
    pub fn grant(&self, input: QuotaGrant) -> Result<QuotaBalance, CoreError> {
        let amount = Self::absolute_amount(input.amount)?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        transaction.execute(
            "INSERT INTO quota_ledger \
             (entry_id, user_id, resource_kind, event_kind, amount, delta, actor_user_id, reason, created_at_ms) \
             VALUES (?1, ?2, ?3, 'adjust', ?4, ?5, ?6, ?7, ?8)",
            params![
                Self::new_id("quota"),
                input.user_id,
                input.resource_kind,
                amount,
                input.amount,
                input.actor_user_id,
                input.reason,
                now,
            ],
        )?;
        let balance = Self::balance_in_transaction(&transaction, &input.user_id, &input.resource_kind)?;
        if balance.available < 0 {
            return Err(CoreError::QuotaOverdrawn);
        }
        Self::insert_audit_event(
            &transaction,
            &input.actor_user_id,
            "quota.adjust",
            "quota",
            &format!("{}:{}", input.user_id, input.resource_kind),
            serde_json::json!({
                "user_id": input.user_id,
                "resource_kind": input.resource_kind,
                "amount": input.amount,
                "reason": input.reason,
            }),
            now,
        )?;
        transaction.commit()?;

        Ok(balance)
    }

    pub fn reserve(&self, input: QuotaReserve) -> Result<ReserveResult, CoreError> {
        if input.amount <= 0 || input.ttl_ms < 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = now.checked_add(input.ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let result = Self::reserve_in_transaction(&transaction, &input, now, expires_at_ms)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn reserve_request(&self, input: QuotaReserve) -> Result<ReserveResult, CoreError> {
        if input.amount <= 0 || input.ttl_ms < 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = now.checked_add(input.ttl_ms).ok_or(CoreError::InvalidQuotaAmount)?;
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (request_user_id, request_state) = transaction
            .query_row(
                "SELECT user_id, state FROM requests WHERE id = ?1",
                [&input.request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: input.request_id.clone(),
            })?;
        if request_user_id != input.user_id {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: request_user_id,
                api_key_id: String::new(),
            });
        }
        let request_state = RequestState::from_db(&request_state).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: request_state,
            }
        })?;
        if request_state != RequestState::Validating {
            return Err(CoreError::InvalidTransition {
                request_id: input.request_id.clone(),
                expected: RequestState::Validating,
                next: RequestState::Reserved,
            });
        }

        let result = Self::reserve_in_transaction(&transaction, &input, now, expires_at_ms)?;
        match &result {
            ReserveResult::Insufficient { .. } => {
                Self::transition_request_on_connection(
                    &transaction,
                    &input.request_id,
                    RequestState::Validating,
                    RequestState::Failed,
                    Some(RequestResult {
                        status: Some(429),
                        error_code: Some("insufficient_quota".into()),
                    }),
                    now,
                )
                .map_err(|source| CoreError::RequestContext {
                    request_id: input.request_id.clone(),
                    source: Box::new(source),
                })?;
            }
            ReserveResult::Created(_) => {
                Self::transition_request_on_connection(
                    &transaction,
                    &input.request_id,
                    RequestState::Validating,
                    RequestState::Reserved,
                    None,
                    now,
                )
                .map_err(|source| CoreError::RequestContext {
                    request_id: input.request_id.clone(),
                    source: Box::new(source),
                })?;
            }
            ReserveResult::Existing(_) => {}
        }
        transaction.commit()?;
        Ok(result)
    }

    pub fn reservation_for_request(&self, request_id: &str) -> Result<Option<Reservation>, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        connection
            .query_row(
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms \
                 FROM quota_reservations WHERE request_id = ?1",
                [request_id],
                Self::reservation_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub fn settle(
        &self,
        principal: &Principal,
        reservation_id: &str,
        settlement: Settlement,
    ) -> Result<QuotaBalance, CoreError> {
        self.settle_impl(principal, reservation_id, settlement, None, None)
    }

    pub fn settle_request(
        &self,
        principal: &Principal,
        reservation_id: &str,
        settlement: Settlement,
        final_state: RequestState,
        result: Option<RequestResult>,
    ) -> Result<QuotaBalance, CoreError> {
        self.settle_impl(
            principal,
            reservation_id,
            settlement,
            Some(final_state),
            Some(result),
        )
    }

    fn settle_impl(
        &self,
        principal: &Principal,
        reservation_id: &str,
        settlement: Settlement,
        final_state: Option<RequestState>,
        result: Option<Option<RequestResult>>,
    ) -> Result<QuotaBalance, CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = Self::reservation_by_id(&transaction, reservation_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: reservation_id.to_owned(),
            }
        })?;
        Self::validate_reservation_owner(&transaction, &reservation, principal)?;

        if reservation.state != ReservationState::Held {
            let balance = Self::balance_in_transaction(
                &transaction,
                &reservation.user_id,
                &reservation.resource_kind,
            )?;
            transaction.commit()?;
            return Ok(balance);
        }

        if let Some(final_state) = final_state {
            let request_state = transaction
                .query_row(
                    "SELECT state FROM requests WHERE id = ?1",
                    [&reservation.request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            if let Some(request_state) = request_state {
                let request_state = RequestState::from_db(&request_state).ok_or_else(|| {
                    CoreError::InvalidConfiguration {
                        key: "requests.state".into(),
                        value: request_state,
                    }
                })?;
                Self::settle_request_state(
                    &transaction,
                    &reservation.request_id,
                    request_state,
                    final_state,
                    result.flatten(),
                    now,
                )?;
            }
        }

        Self::apply_settlement(&transaction, &reservation, settlement, now)?;

        let balance = Self::balance_in_transaction(
            &transaction,
            &reservation.user_id,
            &reservation.resource_kind,
        )?;
        transaction.commit()?;
        Ok(balance)
    }

    pub(crate) fn reserve_in_transaction(
        transaction: &Transaction<'_>,
        input: &QuotaReserve,
        now: i64,
        expires_at_ms: i64,
    ) -> Result<ReserveResult, CoreError> {
        if let Some(reservation) = Self::reservation_by_request(transaction, &input.request_id)? {
            if reservation.user_id != input.user_id || reservation.resource_kind != input.resource_kind {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: input.request_id.clone(),
                });
            }
            return Ok(ReserveResult::Existing(reservation));
        }

        let balance = Self::balance_in_transaction(transaction, &input.user_id, &input.resource_kind)?;
        if balance.available < input.amount {
            return Ok(ReserveResult::Insufficient {
                available: balance.available,
            });
        }

        let reservation = Reservation {
            id: Self::new_id("reservation"),
            user_id: input.user_id.clone(),
            request_id: input.request_id.clone(),
            resource_kind: input.resource_kind.clone(),
            amount: input.amount,
            state: ReservationState::Held,
            expires_at_ms,
        };
        transaction.execute(
            "INSERT INTO quota_reservations \
             (id, user_id, request_id, resource_kind, amount, state, expires_at_ms, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                reservation.id,
                reservation.user_id,
                reservation.request_id,
                reservation.resource_kind,
                reservation.amount,
                reservation.state.as_str(),
                reservation.expires_at_ms,
                now,
            ],
        )?;
        Self::insert_ledger_entry(
            transaction,
            &reservation.user_id,
            &reservation.resource_kind,
            "reserve",
            reservation.amount,
            -reservation.amount,
            Some(&reservation.request_id),
            None,
            None,
            now,
        )?;
        Ok(ReserveResult::Created(reservation))
    }

    fn apply_settlement(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        settlement: Settlement,
        now: i64,
    ) -> Result<(), CoreError> {
        match settlement {
            Settlement::Release => {
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Released, now)?;
                Self::insert_ledger_entry(
                    transaction,
                    &reservation.user_id,
                    &reservation.resource_kind,
                    "release",
                    reservation.amount,
                    reservation.amount,
                    Some(&reservation.request_id),
                    None,
                    None,
                    now,
                )?;
            }
            Settlement::Commit { actual_amount } => {
                let actual_is_unknown = actual_amount.is_none();
                let actual_amount = actual_amount.unwrap_or(reservation.amount);
                if actual_amount < 0 {
                    return Err(CoreError::InvalidQuotaAmount);
                }
                if actual_amount > reservation.amount {
                    return Err(CoreError::ActualAmountExceedsReservation);
                }
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Committed, now)?;
                Self::insert_ledger_entry(
                    transaction,
                    &reservation.user_id,
                    &reservation.resource_kind,
                    "commit",
                    actual_amount,
                    reservation.amount - actual_amount,
                    Some(&reservation.request_id),
                    None,
                    actual_is_unknown.then_some("actual_unknown"),
                    now,
                )?;
            }
            Settlement::Unknown => {
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Unknown, now)?;
            }
        }
        Ok(())
    }

    fn validate_reservation_owner(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        principal: &Principal,
    ) -> Result<(), CoreError> {
        if reservation.user_id != principal.user_id {
            return Err(CoreError::ReservationOwnerMismatch {
                reservation_id: reservation.id.clone(),
            });
        }
        let request_owner = transaction
            .query_row(
                "SELECT user_id, api_key_id FROM requests WHERE id = ?1",
                [&reservation.request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        if let Some((user_id, api_key_id)) = request_owner {
            if user_id != principal.user_id || api_key_id != principal.key_id {
                return Err(CoreError::ReservationOwnerMismatch {
                    reservation_id: reservation.id.clone(),
                });
            }
        }
        Ok(())
    }

    fn settle_request_state(
        transaction: &Transaction<'_>,
        request_id: &str,
        current: RequestState,
        final_state: RequestState,
        result: Option<RequestResult>,
        now: i64,
    ) -> Result<(), CoreError> {
        let intermediate: Vec<RequestState> = match final_state {
            RequestState::Succeeded => vec![
                RequestState::Queued,
                RequestState::Dispatched,
                RequestState::Completing,
                RequestState::Succeeded,
            ],
            RequestState::Failed | RequestState::Unknown => vec![final_state],
            RequestState::Settled => return Ok(()),
            _ => {
                return Err(CoreError::InvalidTransition {
                    request_id: request_id.to_owned(),
                    expected: current,
                    next: final_state,
                })
            }
        };
        let mut expected = current;
        for (index, next) in intermediate.iter().copied().enumerate() {
            let transition_result = (index + 1 == intermediate.len()).then(|| result.clone()).flatten();
            Self::transition_request_on_connection(
                transaction,
                request_id,
                expected,
                next,
                transition_result,
                now,
            )?;
            expected = next;
        }
        Self::transition_request_on_connection(
            transaction,
            request_id,
            expected,
            RequestState::Settled,
            None,
            now,
        )
    }

    pub fn balance(&self, user_id: &str, resource_kind: &str) -> Result<QuotaBalance, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::balance_in_connection(&connection, user_id, resource_kind)
    }

    fn absolute_amount(amount: i64) -> Result<i64, CoreError> {
        amount.checked_abs().filter(|amount| *amount > 0).ok_or(CoreError::InvalidQuotaAmount)
    }

    pub(crate) fn balance_in_transaction(
        transaction: &Transaction<'_>,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<QuotaBalance, CoreError> {
        Self::balance_in_connection(transaction, user_id, resource_kind)
    }

    fn balance_in_connection(
        connection: &rusqlite::Connection,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<QuotaBalance, CoreError> {
        let (available, held) = connection.query_row(
            "SELECT \
             COALESCE((SELECT SUM(delta) FROM quota_ledger WHERE user_id = ?1 AND resource_kind = ?2), 0), \
             COALESCE((SELECT SUM(amount) FROM quota_reservations \
                       WHERE user_id = ?1 AND resource_kind = ?2 AND state IN ('held', 'unknown')), 0)",
            params![user_id, resource_kind],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(QuotaBalance {
            user_id: user_id.to_owned(),
            resource_kind: resource_kind.to_owned(),
            available,
            held,
        })
    }

    pub(crate) fn reservation_by_request(
        transaction: &Transaction<'_>,
        request_id: &str,
    ) -> Result<Option<Reservation>, CoreError> {
        transaction
            .query_row(
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms \
                 FROM quota_reservations WHERE request_id = ?1",
                [request_id],
                Self::reservation_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    fn reservation_by_id(
        transaction: &Transaction<'_>,
        reservation_id: &str,
    ) -> Result<Option<Reservation>, CoreError> {
        transaction
            .query_row(
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms \
                 FROM quota_reservations WHERE id = ?1",
                [reservation_id],
                Self::reservation_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    fn reservation_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Reservation> {
        let state = row.get::<_, String>(5)?;
        let state = ReservationState::from_db(&state).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid reservation state")),
            )
        })?;
        Ok(Reservation {
            id: row.get(0)?,
            user_id: row.get(1)?,
            request_id: row.get(2)?,
            resource_kind: row.get(3)?,
            amount: row.get(4)?,
            state,
            expires_at_ms: row.get(6)?,
        })
    }

    fn set_reservation_state(
        transaction: &Transaction<'_>,
        reservation_id: &str,
        state: ReservationState,
        now: i64,
    ) -> Result<(), CoreError> {
        transaction.execute(
            "UPDATE quota_reservations SET state = ?1, settled_at_ms = ?2 WHERE id = ?3",
            params![state.as_str(), now, reservation_id],
        )?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn insert_ledger_entry(
        transaction: &Transaction<'_>,
        user_id: &str,
        resource_kind: &str,
        event_kind: &str,
        amount: i64,
        delta: i64,
        request_id: Option<&str>,
        actor_user_id: Option<&str>,
        reason: Option<&str>,
        now: i64,
    ) -> Result<(), CoreError> {
        transaction.execute(
            "INSERT INTO quota_ledger \
             (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id, actor_user_id, reason, created_at_ms) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                Self::new_id("quota"),
                user_id,
                resource_kind,
                event_kind,
                amount,
                delta,
                request_id,
                actor_user_id,
                reason,
                now,
            ],
        )?;
        Ok(())
    }
}
