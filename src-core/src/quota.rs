use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    CoreError, CoreStore, QuotaBalance, QuotaGrant, QuotaReserve, Reservation,
    ReservationState, ReserveResult, Settlement,
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

        if let Some(reservation) = Self::reservation_by_request(&transaction, &input.request_id)? {
            transaction.commit()?;
            return Ok(ReserveResult::Existing(reservation));
        }

        let balance = Self::balance_in_transaction(&transaction, &input.user_id, &input.resource_kind)?;
        if balance.available < input.amount {
            transaction.commit()?;
            return Ok(ReserveResult::Insufficient {
                available: balance.available,
            });
        }

        let reservation = Reservation {
            id: Self::new_id("reservation"),
            user_id: input.user_id,
            request_id: input.request_id,
            resource_kind: input.resource_kind,
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
            &transaction,
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
        transaction.commit()?;

        Ok(ReserveResult::Created(reservation))
    }

    pub fn settle(&self, reservation_id: &str, settlement: Settlement) -> Result<QuotaBalance, CoreError> {
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let reservation = Self::reservation_by_id(&transaction, reservation_id)?.ok_or_else(|| {
            CoreError::ReservationNotFound {
                reservation_id: reservation_id.to_owned(),
            }
        })?;

        if reservation.state != ReservationState::Held {
            let balance = Self::balance_in_transaction(
                &transaction,
                &reservation.user_id,
                &reservation.resource_kind,
            )?;
            transaction.commit()?;
            return Ok(balance);
        }

        match settlement {
            Settlement::Release => {
                Self::set_reservation_state(&transaction, reservation_id, ReservationState::Released, now)?;
                Self::insert_ledger_entry(
                    &transaction,
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
                Self::set_reservation_state(&transaction, reservation_id, ReservationState::Committed, now)?;
                Self::insert_ledger_entry(
                    &transaction,
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
                Self::set_reservation_state(&transaction, reservation_id, ReservationState::Unknown, now)?;
            }
        }

        let balance = Self::balance_in_transaction(
            &transaction,
            &reservation.user_id,
            &reservation.resource_kind,
        )?;
        transaction.commit()?;
        Ok(balance)
    }

    pub fn balance(&self, user_id: &str, resource_kind: &str) -> Result<QuotaBalance, CoreError> {
        let connection = self.connection.lock().expect("core store mutex poisoned");
        Self::balance_in_connection(&connection, user_id, resource_kind)
    }

    fn absolute_amount(amount: i64) -> Result<i64, CoreError> {
        amount.checked_abs().filter(|amount| *amount > 0).ok_or(CoreError::InvalidQuotaAmount)
    }

    fn balance_in_transaction(
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
        let available = connection.query_row(
            "SELECT COALESCE(SUM(delta), 0) FROM quota_ledger WHERE user_id = ?1 AND resource_kind = ?2",
            params![user_id, resource_kind],
            |row| row.get(0),
        )?;
        let held = connection.query_row(
            "SELECT COALESCE(SUM(amount), 0) FROM quota_reservations \
             WHERE user_id = ?1 AND resource_kind = ?2 AND state IN ('held', 'unknown')",
            params![user_id, resource_kind],
            |row| row.get(0),
        )?;
        Ok(QuotaBalance {
            user_id: user_id.to_owned(),
            resource_kind: resource_kind.to_owned(),
            available,
            held,
        })
    }

    fn reservation_by_request(
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
