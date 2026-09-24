use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    BillingQuote, BillingReceipt, BillingReceiptResult, BillingReceiptStatus,
    BillingReservationResult, CoreError, CoreQuotaBalanceView, CoreQuotaLedgerView,
    CoreQuotaUsageView, CoreStore, CreditAmount,
    KeyQuotaGrant, LegacyQuotaAllocation, Principal, QuotaBalance, QuotaBudgetBalance,
    QuotaBudgetScope, QuotaGrant, QuotaMigrationState, QuotaReserve, RequestResult, RequestState,
    Reservation, ReservationState, ReserveResult, Settlement, UpstreamCreditSnapshot,
    UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS,
};

struct BudgetAccountRecord {
    id: String,
    scope: QuotaBudgetScope,
    user_id: String,
    api_key_id: Option<String>,
    resource_kind: String,
    enabled: bool,
    version: i64,
    migration_state: QuotaMigrationState,
}

impl CoreStore {
    /// Return a bounded, owner-checked quota projection for a user.
    ///
    /// The read transaction intentionally selects only public ledger fields;
    /// actor/reason metadata remains an administrator-only audit concern.
    pub fn quota_usage_for_principal(
        &self,
        principal: &Principal,
        limit: usize,
    ) -> Result<CoreQuotaUsageView, CoreError> {
        if !(1..=100).contains(&limit) {
            return Err(CoreError::Validation {
                field: "usage.limit".into(),
                reason: "must be between 1 and 100".into(),
            });
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;

        let resource_kinds = {
            let mut statement = transaction.prepare(
                "SELECT resource_kind FROM quota_ledger WHERE user_id = ?1
                 UNION
                 SELECT resource_kind FROM quota_reservations WHERE user_id = ?1
                 ORDER BY resource_kind",
            )?;
            let rows = statement.query_map([&principal.user_id], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let mut balances = Vec::with_capacity(resource_kinds.len());
        for resource_kind in resource_kinds {
            let (available, held, settled) = transaction.query_row(
                "SELECT
                   COALESCE((SELECT SUM(delta) FROM quota_ledger
                             WHERE user_id = ?1 AND resource_kind = ?2), 0),
                   COALESCE((SELECT SUM(amount) FROM quota_reservations
                             WHERE user_id = ?1 AND resource_kind = ?2
                               AND state IN ('held', 'unknown')), 0),
                   COALESCE((SELECT SUM(amount) FROM quota_ledger
                             WHERE user_id = ?1 AND resource_kind = ?2
                               AND event_kind = 'commit'), 0)",
                rusqlite::params![&principal.user_id, &resource_kind],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            balances.push(CoreQuotaBalanceView {
                resource_kind,
                available,
                held,
                settled,
            });
        }

        let ledger = {
            let mut statement = transaction.prepare(
                "SELECT resource_kind, event_kind, amount, delta, request_id, created_at_ms
                 FROM quota_ledger
                 WHERE user_id = ?1
                 ORDER BY created_at_ms DESC, entry_id DESC
                 LIMIT ?2",
            )?;
            let rows = statement.query_map(rusqlite::params![&principal.user_id, limit as i64], |row| {
                Ok(CoreQuotaLedgerView {
                    resource_kind: row.get(0)?,
                    event_kind: row.get(1)?,
                    amount: row.get(2)?,
                    delta: row.get(3)?,
                    request_id: row.get(4)?,
                    created_at_ms: row.get(5)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        transaction.commit()?;
        Ok(CoreQuotaUsageView {
            balances,
            ledger,
            key_available: None,
            user_cap_available: None,
            key_quota_configured: false,
        })
    }

    /// Return a bounded, owner-checked projection for the current API key.
    ///
    /// Key ledger entries are the only public consumption projection. A
    /// user-cap account constrains effective availability but is never added
    /// to held/settled, which prevents one request from appearing twice.
    pub fn key_quota_usage_for_principal(
        &self,
        principal: &Principal,
        limit: usize,
    ) -> Result<CoreQuotaUsageView, CoreError> {
        if !(1..=100).contains(&limit) {
            return Err(CoreError::Validation {
                field: "usage.limit".into(),
                reason: "must be between 1 and 100".into(),
            });
        }
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;

        let key_accounts = {
            let mut statement = transaction.prepare(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'key' AND user_id = ?1 AND api_key_id = ?2
                 ORDER BY resource_kind, id",
            )?;
            let rows = statement.query_map(
                params![&principal.user_id, &principal.key_id],
                Self::budget_account_from_row,
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        if key_accounts.is_empty() {
            return Err(CoreError::KeyQuotaNotConfigured {
                api_key_id: principal.key_id.clone(),
                resource_kind: "usage".into(),
            });
        }

        let mut balances = Vec::with_capacity(key_accounts.len());
        let mut boundary_values = Vec::with_capacity(key_accounts.len());
        for account in &key_accounts {
            Self::ensure_ready_budget_account(account)?;
            let key_balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
            let user_cap_account = transaction
                .query_row(
                    "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                     FROM quota_budget_accounts
                     WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = ?2
                     ORDER BY id LIMIT 1",
                    params![&principal.user_id, &account.resource_kind],
                    Self::budget_account_from_row,
                )
                .optional()?;
            let user_cap_available = if let Some(user_cap_account) = user_cap_account {
                Self::ensure_ready_budget_account(&user_cap_account)?;
                Some(Self::budget_balance_in_transaction(&transaction, &user_cap_account.id)?.available)
            } else {
                None
            };
            let effective_available = user_cap_available
                .map_or(key_balance.available, |user_available| key_balance.available.min(user_available));
            balances.push(CoreQuotaBalanceView {
                resource_kind: account.resource_kind.clone(),
                available: effective_available,
                held: key_balance.held,
                settled: key_balance.settled,
            });
            boundary_values.push((key_balance.available, user_cap_available));
        }

        let ledger = {
            let mut statement = transaction.prepare(
                "SELECT resource_kind, event_kind, amount, delta, request_id, created_at_ms
                 FROM quota_ledger
                 WHERE budget_account_id IN (
                     SELECT id FROM quota_budget_accounts
                     WHERE scope = 'key' AND user_id = ?1 AND api_key_id = ?2
                 ) AND api_key_id = ?2
                 ORDER BY created_at_ms DESC, entry_id DESC
                 LIMIT ?3",
            )?;
            let rows = statement.query_map(
                params![&principal.user_id, &principal.key_id, limit as i64],
                |row| {
                    Ok(CoreQuotaLedgerView {
                        resource_kind: row.get(0)?,
                        event_kind: row.get(1)?,
                        amount: row.get(2)?,
                        delta: row.get(3)?,
                        request_id: row.get(4)?,
                        created_at_ms: row.get(5)?,
                    })
                },
            )?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        let (key_available, user_cap_available) = if boundary_values.len() == 1 {
            boundary_values[0]
        } else {
            (0, None)
        };
        transaction.commit()?;
        Ok(CoreQuotaUsageView {
            balances,
            ledger,
            key_available: if boundary_values.len() == 1 { Some(key_available) } else { None },
            user_cap_available,
            key_quota_configured: true,
        })
    }

    pub fn quota_balance_as_admin(
        &self,
        principal: &Principal,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<QuotaBalance, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let user_exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM users WHERE id = ?1)",
            [user_id],
            |row| row.get(0),
        )?;
        if !user_exists {
            return Err(CoreError::UserNotFound { user_id: user_id.into() });
        }
        let balance = Self::balance_in_transaction(&transaction, user_id, resource_kind)?;
        transaction.commit()?;
        Ok(balance)
    }

    /// Add credits to the user-cap pool that backs all of the user's API keys.
    ///
    /// Legacy user-level ledger rows are attached to the pool account on first
    /// use. The old `grant_as_admin` API remains available for compatibility,
    /// but the Core admin UI uses this method so every new allocation has a
    /// bounded pool behind it.
    pub fn quota_pool_grant_as_admin(
        &self,
        principal: &Principal,
        input: QuotaGrant,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        if input.amount <= 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        if input.resource_kind.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "resource_kind".into(),
                reason: "must not be empty".into(),
            });
        }

        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, &input.user_id)?;
        let (mut account, created) = Self::ensure_user_cap_account_for_admin(
            &transaction,
            &input.user_id,
            &input.resource_kind,
            now,
            true,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let budget_version = if created {
            account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &account.id, now)?;
            account.version = version;
            version
        };
        let entry_id = Self::new_id("quota");
        let event_group_id = format!("admin-{entry_id}");
        Self::insert_budget_ledger_entry(
            &transaction,
            &input.user_id,
            &input.resource_kind,
            "adjust",
            input.amount,
            input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&account.id),
            Some(&event_group_id),
            None,
            budget_version,
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "quota.pool_adjust",
            "quota_budget_account",
            &account.id,
            serde_json::json!({
                "user_id": input.user_id,
                "resource_kind": input.resource_kind,
                "amount": input.amount,
                "reason": input.reason,
                "event_group_id": event_group_id,
            }),
            now,
        )?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    pub fn quota_pool_balance_as_admin(
        &self,
        principal: &Principal,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, user_id)?;
        let (account, _) = Self::ensure_user_cap_account_for_admin(
            &transaction,
            user_id,
            resource_kind,
            Utc::now().timestamp_millis(),
            false,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    pub fn quota_pool_allocatable_as_admin(
        &self,
        principal: &Principal,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<i64, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, user_id)?;
        let (account, _) = Self::ensure_user_cap_account_for_admin(
            &transaction,
            user_id,
            resource_kind,
            Utc::now().timestamp_millis(),
            false,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let pool = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        let allocated = Self::allocated_key_quota_in_transaction(&transaction, user_id, resource_kind)?;
        let available = pool.available.saturating_sub(allocated).max(0);
        transaction.commit()?;
        Ok(available)
    }

    /// Allocate from the user's pool to a specific API key in one transaction.
    /// The pool ledger is not duplicated: requests later reserve/settle both
    /// the key account and this user-cap account, while this adjustment only
    /// establishes the key's upper bound.
    pub fn key_quota_allocate_from_pool_as_admin(
        &self,
        principal: &Principal,
        input: KeyQuotaGrant,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        if input.amount <= 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        if input.resource_kind.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "resource_kind".into(),
                reason: "must not be empty".into(),
            });
        }

        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let user_id = Self::active_api_key_owner(&transaction, &input.api_key_id)?;
        let (pool_account, _) = Self::ensure_user_cap_account_for_admin(
            &transaction,
            &user_id,
            &input.resource_kind,
            now,
            false,
        )?;
        Self::ensure_ready_budget_account(&pool_account)?;
        let pool_balance = Self::budget_balance_in_transaction(&transaction, &pool_account.id)?;
        let allocated = Self::allocated_key_quota_in_transaction(
            &transaction,
            &user_id,
            &input.resource_kind,
        )?;
        let allocatable = pool_balance.available.saturating_sub(allocated).max(0);
        if input.amount > allocatable {
            return Err(CoreError::QuotaPoolInsufficient {
                available: allocatable,
                required: input.amount,
            });
        }

        let (mut account, created) = Self::ensure_key_budget_account(
            &transaction,
            &input.api_key_id,
            &user_id,
            &input.resource_kind,
            now,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let budget_version = if created {
            account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &account.id, now)?;
            account.version = version;
            version
        };
        let entry_id = Self::new_id("quota");
        let event_group_id = format!("admin-{entry_id}");
        Self::insert_budget_ledger_entry(
            &transaction,
            &user_id,
            &input.resource_kind,
            "adjust",
            input.amount,
            input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&account.id),
            Some(&event_group_id),
            Some(&input.api_key_id),
            budget_version,
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "quota.key_allocate",
            "quota_budget_account",
            &account.id,
            serde_json::json!({
                "api_key_id": input.api_key_id,
                "resource_kind": input.resource_kind,
                "amount": input.amount,
                "pool_allocatable_before": allocatable,
                "reason": input.reason,
                "event_group_id": event_group_id,
            }),
            now,
        )?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    /// Allocate one unified credits balance directly to a Key. The fresh
    /// upstream snapshot is checked against every Key's remaining available
    /// plus held amount inside the same immediate transaction as the grant.
    pub fn key_quota_allocate_from_upstream_as_admin(
        &self,
        principal: &Principal,
        input: KeyQuotaGrant,
        snapshot: UpstreamCreditSnapshot,
    ) -> Result<(QuotaBudgetBalance, i64), CoreError> {
        if input.amount <= 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        if input.resource_kind != "credits" {
            return Err(CoreError::Validation {
                field: "resource_kind".into(),
                reason: "only the unified credits balance can be allocated".into(),
            });
        }

        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_fresh_upstream_credit_snapshot(&snapshot, now)?;
        let (upstream_total, committed, allocatable) =
            Self::upstream_credit_capacity_in_transaction(&transaction, &snapshot)?;
        if input.amount > allocatable {
            return Err(CoreError::UpstreamCreditLimitExceeded {
                available: allocatable,
                required: input.amount,
            });
        }

        let user_id = Self::active_api_key_owner(&transaction, &input.api_key_id)?;
        let (mut account, created) = Self::ensure_key_budget_account(
            &transaction,
            &input.api_key_id,
            &user_id,
            "credits",
            now,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let budget_version = if created {
            account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &account.id, now)?;
            account.version = version;
            version
        };
        let entry_id = Self::new_id("quota");
        let event_group_id = format!("admin-{entry_id}");
        Self::insert_budget_ledger_entry(
            &transaction,
            &user_id,
            "credits",
            "adjust",
            input.amount,
            input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&account.id),
            Some(&event_group_id),
            Some(&input.api_key_id),
            budget_version,
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "quota.key_allocate_upstream",
            "quota_budget_account",
            &account.id,
            serde_json::json!({
                "api_key_id": input.api_key_id,
                "resource_kind": "credits",
                "amount": input.amount,
                "upstream_total": upstream_total,
                "committed_before": committed,
                "allocatable_before": allocatable,
                "allocatable_after": allocatable - input.amount,
                "reason": input.reason,
                "event_group_id": event_group_id,
            }),
            now,
        )?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        let remaining = allocatable - input.amount;
        transaction.commit()?;
        Ok((balance, remaining))
    }

    pub fn key_quota_grant_as_admin(
        &self,
        principal: &Principal,
        input: KeyQuotaGrant,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        if input.amount <= 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        if input.resource_kind.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "resource_kind".into(),
                reason: "must not be empty".into(),
            });
        }

        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let user_id = Self::active_api_key_owner(&transaction, &input.api_key_id)?;
        let (mut account, created) = Self::ensure_key_budget_account(
            &transaction,
            &input.api_key_id,
            &user_id,
            &input.resource_kind,
            now,
        )?;
        Self::ensure_ready_budget_account(&account)?;
        let budget_version = if created {
            account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &account.id, now)?;
            account.version = version;
            version
        };
        let entry_id = Self::new_id("quota");
        let event_group_id = format!("admin-{entry_id}");
        Self::insert_budget_ledger_entry(
            &transaction,
            &user_id,
            &input.resource_kind,
            "adjust",
            input.amount,
            input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&account.id),
            Some(&event_group_id),
            Some(&input.api_key_id),
            budget_version,
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "quota.key_adjust",
            "quota_budget_account",
            &account.id,
            serde_json::json!({
                "api_key_id": input.api_key_id,
                "resource_kind": input.resource_kind,
                "amount": input.amount,
                "reason": input.reason,
                "event_group_id": event_group_id,
            }),
            now,
        )?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    pub fn key_quota_balance_as_admin(
        &self,
        principal: &Principal,
        api_key_id: &str,
        resource_kind: &str,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        let user_id = Self::api_key_owner(&transaction, api_key_id)?;
        let account = transaction
            .query_row(
                "SELECT id FROM quota_budget_accounts
                 WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = ?3
                 ORDER BY id LIMIT 1",
                rusqlite::params![api_key_id, user_id, resource_kind],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::KeyQuotaNotConfigured {
                api_key_id: api_key_id.into(),
                resource_kind: resource_kind.into(),
            })?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account)?;
        transaction.commit()?;
        Ok(balance)
    }

    /// Return the quota projection visible to the active API-key principal.
    ///
    /// Unlike the legacy user balance, this projection is scoped to the
    /// authenticated key and refuses to silently fall back to a user-wide
    /// ledger.  That makes it safe for request preflight and fail-closed
    /// executor-unavailable paths.
    pub fn key_quota_balance_for_principal(
        &self,
        principal: &Principal,
        resource_kind: &str,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
        Self::ensure_principal_in_transaction(&transaction, principal)?;
        let account = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = ?3
                 ORDER BY id LIMIT 1",
                params![&principal.key_id, &principal.user_id, resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?
            .ok_or_else(|| CoreError::KeyQuotaNotConfigured {
                api_key_id: principal.key_id.clone(),
                resource_kind: resource_kind.into(),
            })?;
        Self::ensure_ready_budget_account(&account)?;
        let balance = Self::budget_balance_in_transaction(&transaction, &account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    /// Validate dual-ledger event groups during enforcing scheduler startup.
    ///
    /// A malformed, incomplete, version-inconsistent, or unknown group is
    /// quarantined by marking its budget accounts `reconcile_required`.  No
    /// ledger event is synthesized and no hold is released, so recovery never
    /// guesses about an upstream outcome.
    pub fn reconcile_quota_event_groups(&self, now_ms: i64) -> Result<u64, CoreError> {
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                    api_key_id, key_budget_account_id, user_cap_account_id, event_group_id
             FROM quota_reservations
             WHERE key_budget_account_id IS NOT NULL
             ORDER BY created_at_ms, id",
        )?;
        let reservations = statement
            .query_map([], Self::reservation_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut invalid_groups = 0_u64;
        for reservation in reservations {
            let (consistent, reason) = Self::validate_quota_event_group(&transaction, &reservation)?;
            if consistent {
                continue;
            }

            invalid_groups += 1;
            let mut account_ids = Vec::new();
            if let Some(account_id) = reservation.key_budget_account_id.as_deref() {
                account_ids.push(account_id.to_owned());
            }
            if let Some(account_id) = reservation.user_cap_account_id.as_deref() {
                if !account_ids.iter().any(|existing| existing == account_id) {
                    account_ids.push(account_id.to_owned());
                }
            }
            let mut transitioned = false;
            for account_id in &account_ids {
                transitioned |= transaction.execute(
                    "UPDATE quota_budget_accounts
                     SET migration_state = 'reconcile_required', updated_at_ms = ?1
                     WHERE id = ?2 AND migration_state <> 'reconcile_required'",
                    params![now_ms, account_id],
                )? > 0;
            }
            if transitioned {
                Self::insert_audit_event(
                    &transaction,
                    "system",
                    "quota.reconcile_required",
                    "quota_reservation",
                    &reservation.id,
                    serde_json::json!({
                        "request_id": reservation.request_id,
                        "event_group_id": reservation.event_group_id,
                        "reason": reason,
                        "hold_preserved": true,
                    }),
                    now_ms,
                )?;
            }
        }

        transaction.commit()?;
        Ok(invalid_groups)
    }

    fn validate_quota_event_group(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
    ) -> Result<(bool, String), CoreError> {
        let Some(event_group_id) = reservation.event_group_id.as_deref() else {
            return Ok((false, "missing_event_group_id".into()));
        };
        if event_group_id.is_empty() {
            return Ok((false, "empty_event_group_id".into()));
        }
        let Some(key_account_id) = reservation.key_budget_account_id.as_deref() else {
            return Ok((false, "missing_key_budget_account_id".into()));
        };
        let mut account_ids = vec![key_account_id.to_owned()];
        if let Some(user_account_id) = reservation.user_cap_account_id.as_deref() {
            if user_account_id == key_account_id {
                return Ok((false, "duplicate_budget_account_id".into()));
            }
            account_ids.push(user_account_id.to_owned());
        }

        let mut account_versions = Vec::with_capacity(account_ids.len());
        for (index, account_id) in account_ids.iter().enumerate() {
            let record = transaction
                .query_row(
                    "SELECT scope, version FROM quota_budget_accounts WHERE id = ?1",
                    [account_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()?;
            let Some((scope, version)) = record else {
                return Ok((false, format!("missing_budget_account:{account_id}")));
            };
            let expected_scope = if index == 0 { "key" } else { "user_cap" };
            if scope != expected_scope {
                return Ok((false, format!("unexpected_budget_scope:{account_id}")));
            }
            account_versions.push(version);
        }

        if reservation.state == ReservationState::Unknown {
            return Ok((false, "reservation_state_unknown".into()));
        }

        let mut statement = transaction.prepare(
            "SELECT budget_account_id, event_kind, amount, delta, request_id, budget_version
             FROM quota_ledger WHERE event_group_id = ?1 ORDER BY entry_id",
        )?;
        let events = statement
            .query_map([event_group_id], |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let final_kind = match reservation.state {
            ReservationState::Held => None,
            ReservationState::Released => Some("release"),
            ReservationState::Committed => Some("commit"),
            ReservationState::Unknown => None,
        };
        let expected_event_count = account_ids.len() * usize::from(final_kind.is_some() || reservation.state == ReservationState::Unknown);
        if reservation.state == ReservationState::Held && events.len() != account_ids.len() {
            return Ok((false, "reserve_event_count_mismatch".into()));
        }
        if final_kind.is_some() && events.len() != expected_event_count {
            return Ok((false, "settlement_event_count_mismatch".into()));
        }
        if reservation.state == ReservationState::Unknown && !events.is_empty() {
            return Ok((false, "unknown_group_has_ledger_events".into()));
        }

        for (event_account_id, event_kind, amount, delta, request_id, budget_version) in &events {
            let Some(event_account_id) = event_account_id.as_deref() else {
                return Ok((false, "ledger_event_missing_budget_account".into()));
            };
            let Some(account_index) = account_ids.iter().position(|id| id == event_account_id) else {
                return Ok((false, "ledger_event_has_unexpected_budget_account".into()));
            };
            if request_id.as_deref() != Some(reservation.request_id.as_str()) {
                return Ok((false, "ledger_event_request_mismatch".into()));
            }
            if *budget_version != Some(account_versions[account_index]) {
                return Ok((false, "ledger_event_version_mismatch".into()));
            }
            match event_kind.as_str() {
                "reserve" if *amount == reservation.amount && *delta == -reservation.amount => {}
                "release"
                    if reservation.state == ReservationState::Released
                        && *amount == reservation.amount
                        && *delta == reservation.amount => {}
                "commit"
                    if reservation.state == ReservationState::Committed
                        && *amount >= 0
                        && *amount <= reservation.amount
                        && *delta == reservation.amount - *amount => {}
                _ => return Ok((false, "ledger_event_shape_mismatch".into())),
            }
        }

        for account_id in &account_ids {
            let reserve_count = events
                .iter()
                .filter(|event| event.0.as_deref() == Some(account_id.as_str()) && event.1 == "reserve")
                .count();
            if reserve_count != 1 {
                return Ok((false, "reserve_event_missing_or_duplicate".into()));
            }
            if let Some(final_kind) = final_kind {
                let final_count = events
                    .iter()
                    .filter(|event| event.0.as_deref() == Some(account_id.as_str()) && event.1 == final_kind)
                    .count();
                if final_count != 1 {
                    return Ok((false, "settlement_event_missing_or_duplicate".into()));
                }
            }
        }
        Ok((true, String::new()))
    }

    pub fn key_quota_allocate_legacy_as_admin(
        &self,
        principal: &Principal,
        input: LegacyQuotaAllocation,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        if input.amount <= 0 {
            return Err(CoreError::InvalidQuotaAmount);
        }
        if input.migration_id.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "migration_id".into(),
                reason: "must not be empty".into(),
            });
        }
        if input.resource_kind.trim().is_empty() {
            return Err(CoreError::Validation {
                field: "resource_kind".into(),
                reason: "must not be empty".into(),
            });
        }

        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, &input.source_user_id)?;
        let key_user_id = Self::active_api_key_owner(&transaction, &input.api_key_id)?;
        if key_user_id != input.source_user_id {
            return Err(CoreError::ApiKeyOwnershipMismatch {
                api_key_id: input.api_key_id,
                user_id: input.source_user_id,
            });
        }

        let existing_events: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE event_group_id = ?1",
            [&input.migration_id],
            |row| row.get(0),
        )?;
        if existing_events > 0 {
            let target_events: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM quota_ledger
                 WHERE event_group_id = ?1 AND api_key_id = ?2
                   AND resource_kind = ?3 AND event_kind = 'adjust'",
                rusqlite::params![&input.migration_id, &input.api_key_id, &input.resource_kind],
                |row| row.get(0),
            )?;
            let source_events: i64 = transaction.query_row(
                "SELECT COUNT(*) FROM quota_ledger
                 WHERE event_group_id = ?1 AND api_key_id IS NULL
                   AND resource_kind = ?2 AND event_kind = 'adjust'",
                rusqlite::params![&input.migration_id, &input.resource_kind],
                |row| row.get(0),
            )?;
            if target_events != 1 || source_events != 1 || existing_events != 2 {
                return Err(CoreError::QuotaMigrationConflict {
                    migration_id: input.migration_id,
                });
            }
            let account_id = transaction
                .query_row(
                    "SELECT id FROM quota_budget_accounts
                     WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = ?3
                     ORDER BY id LIMIT 1",
                    rusqlite::params![&input.api_key_id, &input.source_user_id, &input.resource_kind],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| CoreError::QuotaMigrationConflict {
                    migration_id: input.migration_id.clone(),
                })?;
            let balance = Self::budget_balance_in_transaction(&transaction, &account_id)?;
            transaction.commit()?;
            return Ok(balance);
        }

        let (mut source_account, source_created) = Self::ensure_legacy_user_cap_account(
            &transaction,
            &input.source_user_id,
            &input.resource_kind,
            now,
        )?;
        if !source_created && source_account.migration_state != QuotaMigrationState::LegacyUnassigned {
            return Err(CoreError::QuotaMigrationPending {
                account_id: source_account.id,
            });
        }
        Self::ensure_ready_or_legacy_account(&source_account)?;
        let source_balance = Self::budget_balance_in_transaction(&transaction, &source_account.id)?;
        if source_balance.available < input.amount {
            return Err(CoreError::QuotaInsufficient {
                available: source_balance.available,
                required: input.amount,
            });
        }

        let (mut key_account, key_created) = Self::ensure_key_budget_account(
            &transaction,
            &input.api_key_id,
            &input.source_user_id,
            &input.resource_kind,
            now,
        )?;
        Self::ensure_ready_budget_account(&key_account)?;
        let source_version = if source_created {
            source_account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &source_account.id, now)?;
            source_account.version = version;
            version
        };
        let key_version = if key_created {
            key_account.version
        } else {
            let version = Self::advance_budget_account(&transaction, &key_account.id, now)?;
            key_account.version = version;
            version
        };
        Self::insert_budget_ledger_entry(
            &transaction,
            &input.source_user_id,
            &input.resource_kind,
            "adjust",
            input.amount,
            -input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&source_account.id),
            Some(&input.migration_id),
            None,
            source_version,
        )?;
        Self::insert_budget_ledger_entry(
            &transaction,
            &input.source_user_id,
            &input.resource_kind,
            "adjust",
            input.amount,
            input.amount,
            None,
            Some(&principal.user_id),
            Some(&input.reason),
            now,
            Some(&key_account.id),
            Some(&input.migration_id),
            Some(&input.api_key_id),
            key_version,
        )?;
        transaction.execute(
            "UPDATE quota_budget_accounts
             SET migration_state = 'ready', updated_at_ms = ?1
             WHERE id IN (?2, ?3)",
            rusqlite::params![now, &source_account.id, &key_account.id],
        )?;
        Self::insert_audit_event(
            &transaction,
            &principal.user_id,
            "quota.legacy_allocate",
            "quota_budget_account",
            &key_account.id,
            serde_json::json!({
                "migration_id": input.migration_id,
                "source_user_id": input.source_user_id,
                "api_key_id": input.api_key_id,
                "resource_kind": input.resource_kind,
                "amount": input.amount,
                "reason": input.reason,
            }),
            now,
        )?;
        let balance = Self::budget_balance_in_transaction(&transaction, &key_account.id)?;
        transaction.commit()?;
        Ok(balance)
    }

    fn api_key_owner(
        transaction: &Transaction<'_>,
        api_key_id: &str,
    ) -> Result<String, CoreError> {
        transaction
            .query_row(
                "SELECT user_id FROM api_keys WHERE id = ?1",
                [api_key_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::ApiKeyNotFound {
                api_key_id: api_key_id.into(),
            })
    }

    fn active_api_key_owner(
        transaction: &Transaction<'_>,
        api_key_id: &str,
    ) -> Result<String, CoreError> {
        let (user_id, status) = transaction
            .query_row(
                "SELECT user_id, status FROM api_keys WHERE id = ?1",
                [api_key_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::ApiKeyNotFound {
                api_key_id: api_key_id.into(),
            })?;
        if status != "active" {
            return Err(CoreError::InvalidRequestIdentity {
                user_id,
                api_key_id: api_key_id.into(),
            });
        }
        Self::ensure_active_user(transaction, &user_id)?;
        Ok(user_id)
    }

    fn ensure_key_budget_account(
        transaction: &Transaction<'_>,
        api_key_id: &str,
        user_id: &str,
        resource_kind: &str,
        now: i64,
    ) -> Result<(BudgetAccountRecord, bool), CoreError> {
        let existing = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'key' AND api_key_id = ?1 AND resource_kind = ?2
                 ORDER BY id LIMIT 1",
                rusqlite::params![api_key_id, resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?;
        if let Some(account) = existing {
            if account.user_id != user_id {
                return Err(CoreError::ApiKeyOwnershipMismatch {
                    api_key_id: api_key_id.into(),
                    user_id: user_id.into(),
                });
            }
            return Ok((account, false));
        }

        let id = Self::new_id("budget");
        transaction.execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version,
              migration_state, created_at_ms, updated_at_ms)
             VALUES (?1, 'key', ?2, ?3, ?4, 1, 1, 'ready', ?5, ?5)",
            rusqlite::params![&id, user_id, api_key_id, resource_kind, now],
        )?;
        Ok((
            BudgetAccountRecord {
                id,
                scope: QuotaBudgetScope::Key,
                user_id: user_id.into(),
                api_key_id: Some(api_key_id.into()),
                resource_kind: resource_kind.into(),
                enabled: true,
                version: 1,
                migration_state: QuotaMigrationState::Ready,
            },
            true,
        ))
    }

    fn ensure_legacy_user_cap_account(
        transaction: &Transaction<'_>,
        user_id: &str,
        resource_kind: &str,
        now: i64,
    ) -> Result<(BudgetAccountRecord, bool), CoreError> {
        let existing = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = ?2
                 ORDER BY id LIMIT 1",
                rusqlite::params![user_id, resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?;
        if let Some(account) = existing {
            return Ok((account, false));
        }

        let legacy_rows: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM quota_ledger
             WHERE user_id = ?1 AND resource_kind = ?2
               AND budget_account_id IS NULL AND api_key_id IS NULL",
            rusqlite::params![user_id, resource_kind],
            |row| row.get(0),
        )?;
        if legacy_rows == 0 {
            return Err(CoreError::Validation {
                field: "source_user_id".into(),
                reason: "legacy user quota is not configured".into(),
            });
        }

        let id = Self::new_id("budget");
        transaction.execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version,
              migration_state, created_at_ms, updated_at_ms)
             VALUES (?1, 'user_cap', ?2, NULL, ?3, 1, 1, 'legacy_unassigned', ?4, ?4)",
            rusqlite::params![&id, user_id, resource_kind, now],
        )?;
        transaction.execute(
            "UPDATE quota_ledger
             SET budget_account_id = ?1
             WHERE user_id = ?2 AND resource_kind = ?3
               AND budget_account_id IS NULL AND api_key_id IS NULL",
            rusqlite::params![&id, user_id, resource_kind],
        )?;
        Ok((
            BudgetAccountRecord {
                id,
                scope: QuotaBudgetScope::UserCap,
                user_id: user_id.into(),
                api_key_id: None,
                resource_kind: resource_kind.into(),
                enabled: true,
                version: 1,
                migration_state: QuotaMigrationState::LegacyUnassigned,
            },
            true,
        ))
    }

    fn ensure_user_cap_account_for_admin(
        transaction: &Transaction<'_>,
        user_id: &str,
        resource_kind: &str,
        now: i64,
        create_if_missing: bool,
    ) -> Result<(BudgetAccountRecord, bool), CoreError> {
        let existing = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = ?2
                 ORDER BY id LIMIT 1",
                rusqlite::params![user_id, resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?;
        if let Some(account) = existing {
            if account.migration_state == QuotaMigrationState::LegacyUnassigned {
                transaction.execute(
                    "UPDATE quota_budget_accounts
                     SET migration_state = 'ready', updated_at_ms = ?1
                     WHERE id = ?2",
                    rusqlite::params![now, &account.id],
                )?;
                let mut account = account;
                account.migration_state = QuotaMigrationState::Ready;
                return Ok((account, false));
            }
            return Ok((account, false));
        }

        let legacy_rows: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM quota_ledger
             WHERE user_id = ?1 AND resource_kind = ?2
               AND budget_account_id IS NULL AND api_key_id IS NULL",
            rusqlite::params![user_id, resource_kind],
            |row| row.get(0),
        )?;
        if legacy_rows == 0 && !create_if_missing {
            return Err(CoreError::QuotaPoolNotConfigured {
                user_id: user_id.into(),
                resource_kind: resource_kind.into(),
            });
        }

        let id = Self::new_id("budget");
        transaction.execute(
            "INSERT INTO quota_budget_accounts
             (id, scope, user_id, api_key_id, resource_kind, enabled, version,
              migration_state, created_at_ms, updated_at_ms)
             VALUES (?1, 'user_cap', ?2, NULL, ?3, 1, 1, 'ready', ?4, ?4)",
            rusqlite::params![&id, user_id, resource_kind, now],
        )?;
        if legacy_rows > 0 {
            transaction.execute(
                "UPDATE quota_ledger
                 SET budget_account_id = ?1
                 WHERE user_id = ?2 AND resource_kind = ?3
                   AND budget_account_id IS NULL AND api_key_id IS NULL",
                rusqlite::params![&id, user_id, resource_kind],
            )?;
            transaction.execute(
                "UPDATE quota_reservations
                 SET user_cap_account_id = ?1
                 WHERE user_id = ?2 AND resource_kind = ?3
                   AND key_budget_account_id IS NULL AND user_cap_account_id IS NULL",
                rusqlite::params![&id, user_id, resource_kind],
            )?;
        }
        Ok((
            BudgetAccountRecord {
                id,
                scope: QuotaBudgetScope::UserCap,
                user_id: user_id.into(),
                api_key_id: None,
                resource_kind: resource_kind.into(),
                enabled: true,
                version: 1,
                migration_state: QuotaMigrationState::Ready,
            },
            true,
        ))
    }

    pub(crate) fn allocated_key_quota_in_transaction(
        transaction: &Transaction<'_>,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<i64, CoreError> {
        let mut statement = transaction.prepare(
            "SELECT quota_budget_accounts.id
             FROM quota_budget_accounts
             INNER JOIN api_keys
               ON api_keys.id = quota_budget_accounts.api_key_id
              AND api_keys.user_id = quota_budget_accounts.user_id
              AND api_keys.status = 'active'
             WHERE quota_budget_accounts.scope = 'key'
               AND quota_budget_accounts.user_id = ?1
               AND quota_budget_accounts.resource_kind = ?2
               AND quota_budget_accounts.enabled = 1
             ORDER BY quota_budget_accounts.id",
        )?;
        let account_ids = statement
            .query_map(rusqlite::params![user_id, resource_kind], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        account_ids.into_iter().try_fold(0_i64, |total, account_id| {
            let balance = Self::budget_balance_in_transaction(transaction, &account_id)?;
            total
                .checked_add(balance.available.saturating_add(balance.held))
                .ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "quota_budget_accounts.total_allocated".into(),
                    value: "overflow".into(),
                })
        })
    }

    fn ensure_fresh_upstream_credit_snapshot(
        snapshot: &UpstreamCreditSnapshot,
        now: i64,
    ) -> Result<(), CoreError> {
        let age_ms = now.saturating_sub(snapshot.updated_at_ms);
        if snapshot.updated_at_ms <= 0
            || age_ms < 0
            || age_ms > UPSTREAM_CREDIT_SNAPSHOT_MAX_AGE_MS
        {
            return Err(CoreError::UpstreamCreditsUnavailable {
                reason: "AI Work 积分快照缺失、来自未来或已过期，请先刷新 AI Work 积分".into(),
            });
        }
        Ok(())
    }

    fn upstream_credit_capacity_in_transaction(
        transaction: &Transaction<'_>,
        snapshot: &UpstreamCreditSnapshot,
    ) -> Result<(i64, i64, i64), CoreError> {
        let mut statement = transaction.prepare(
            "SELECT id FROM quota_budget_accounts
             WHERE scope = 'key' AND resource_kind = 'credits'
             ORDER BY id",
        )?;
        let account_ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let committed = account_ids.into_iter().try_fold(0_i64, |total, account_id| {
            let balance = Self::budget_balance_in_transaction(transaction, &account_id)?;
            let account_commitment = balance
                .available
                .checked_add(balance.held)
                .ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "quota_budget_accounts.total_committed".into(),
                    value: "overflow".into(),
                })?
                .max(0);
            total
                .checked_add(account_commitment)
                .ok_or_else(|| CoreError::InvalidConfiguration {
                    key: "quota_budget_accounts.total_committed".into(),
                    value: "overflow".into(),
                })
        })?;
        let upstream_total = snapshot.total.as_microcredits();
        if committed > upstream_total {
            return Err(CoreError::UpstreamCommitmentsExceedBalance {
                upstream_total,
                committed,
            });
        }
        Ok((upstream_total, committed, upstream_total - committed))
    }

    fn ensure_ready_budget_account(account: &BudgetAccountRecord) -> Result<(), CoreError> {
        if account.version <= 0 {
            return Err(CoreError::InvalidConfiguration {
                key: "quota_budget_accounts.version".into(),
                value: account.version.to_string(),
            });
        }
        if !account.enabled {
            return Err(CoreError::Validation {
                field: "quota_budget_account.enabled".into(),
                reason: "budget account is disabled".into(),
            });
        }
        if account.migration_state != QuotaMigrationState::Ready {
            return Err(CoreError::QuotaMigrationPending {
                account_id: account.id.clone(),
            });
        }
        Ok(())
    }

    fn ensure_ready_or_legacy_account(account: &BudgetAccountRecord) -> Result<(), CoreError> {
        if account.version <= 0 {
            return Err(CoreError::InvalidConfiguration {
                key: "quota_budget_accounts.version".into(),
                value: account.version.to_string(),
            });
        }
        if !account.enabled {
            return Err(CoreError::Validation {
                field: "quota_budget_account.enabled".into(),
                reason: "budget account is disabled".into(),
            });
        }
        if !matches!(
            account.migration_state,
            QuotaMigrationState::LegacyUnassigned | QuotaMigrationState::Ready
        ) {
            return Err(CoreError::QuotaMigrationPending {
                account_id: account.id.clone(),
            });
        }
        Ok(())
    }

    fn advance_budget_account(
        transaction: &Transaction<'_>,
        account_id: &str,
        now: i64,
    ) -> Result<i64, CoreError> {
        let current: i64 = transaction.query_row(
            "SELECT version FROM quota_budget_accounts WHERE id = ?1",
            [account_id],
            |row| row.get(0),
        )?;
        let version = current.checked_add(1).ok_or_else(|| CoreError::InvalidConfiguration {
            key: "quota_budget_accounts.version".into(),
            value: current.to_string(),
        })?;
        transaction.execute(
            "UPDATE quota_budget_accounts SET version = ?1, updated_at_ms = ?2 WHERE id = ?3",
            rusqlite::params![version, now, account_id],
        )?;
        Ok(version)
    }

    fn budget_account_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<BudgetAccountRecord> {
        let scope_value: String = row.get(1)?;
        let scope = QuotaBudgetScope::from_db(&scope_value).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                1,
                rusqlite::types::Type::Text,
                "invalid quota budget scope".into(),
            )
        })?;
        let migration_state_value: String = row.get(7)?;
        let migration_state = QuotaMigrationState::from_db(&migration_state_value).ok_or_else(|| {
            rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                "invalid quota migration state".into(),
            )
        })?;
        Ok(BudgetAccountRecord {
            id: row.get(0)?,
            scope,
            user_id: row.get(2)?,
            api_key_id: row.get(3)?,
            resource_kind: row.get(4)?,
            enabled: row.get::<_, i64>(5)? != 0,
            version: row.get(6)?,
            migration_state,
        })
    }

    pub(crate) fn budget_balance_in_transaction(
        transaction: &Transaction<'_>,
        account_id: &str,
    ) -> Result<QuotaBudgetBalance, CoreError> {
        let (account, available, held, settled) = transaction.query_row(
            "SELECT
                a.id, a.scope, a.user_id, a.api_key_id, a.resource_kind,
                a.enabled, a.version, a.migration_state,
                COALESCE((SELECT SUM(delta) FROM quota_ledger
                          WHERE budget_account_id = a.id), 0),
                COALESCE((SELECT SUM(amount) FROM quota_reservations
                          WHERE (key_budget_account_id = a.id OR user_cap_account_id = a.id)
                            AND state IN ('held', 'unknown')), 0),
                COALESCE((SELECT SUM(amount) FROM quota_ledger
                          WHERE budget_account_id = a.id AND event_kind = 'commit'), 0)
             FROM quota_budget_accounts a
             WHERE a.id = ?1",
            [account_id],
            |row| {
                let account = Self::budget_account_from_row(row)?;
                Ok((account, row.get::<_, i64>(8)?, row.get::<_, i64>(9)?, row.get::<_, i64>(10)?))
            },
        )?;
        Ok(QuotaBudgetBalance {
            account_id: account.id,
            scope: account.scope,
            user_id: account.user_id,
            api_key_id: account.api_key_id,
            resource_kind: account.resource_kind,
            available,
            held,
            settled,
            version: account.version,
            enabled: account.enabled,
            migration_state: account.migration_state,
            key_quota_configured: account.scope == QuotaBudgetScope::Key,
        })
    }


    pub fn grant(&self, input: QuotaGrant) -> Result<QuotaBalance, CoreError> {
        self.grant_inner(input, false)
    }

    pub fn grant_as_admin(
        &self,
        mut input: QuotaGrant,
        principal: &Principal,
    ) -> Result<QuotaBalance, CoreError> {
        input.actor_user_id = principal.user_id.clone();
        self.grant_inner_with_principal(input, principal)
    }

    fn grant_inner(&self, input: QuotaGrant, require_admin: bool) -> Result<QuotaBalance, CoreError> {
        let amount = Self::absolute_amount(input.amount)?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if require_admin {
            Self::authorize_admin_in_transaction(&transaction, &input.actor_user_id)?;
        }
        Self::ensure_active_user(&transaction, &input.user_id)?;

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

    fn grant_inner_with_principal(
        &self,
        input: QuotaGrant,
        principal: &Principal,
    ) -> Result<QuotaBalance, CoreError> {
        let amount = Self::absolute_amount(input.amount)?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::authorize_admin_principal_in_transaction(&transaction, principal)?;
        Self::ensure_active_user(&transaction, &input.user_id)?;
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
                principal.user_id,
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
            &principal.user_id,
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
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                        api_key_id, key_budget_account_id, user_cap_account_id, event_group_id \
                 FROM quota_reservations WHERE request_id = ?1",
                [request_id],
                Self::reservation_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    /// Atomically validates an AI Work quote against a Core-generated request,
    /// reserves its maximum amount on that request's own Key, and stores the
    /// immutable quote. This method must only be called after bridge
    /// authentication has verified the quote source.
    pub fn reserve_credit_quote(
        &self,
        quote: BillingQuote,
    ) -> Result<BillingReservationResult, CoreError> {
        self.reserve_credit_quote_inner(quote, None)
    }

    /// Reserve a paid request only while a fresh AI Work total still covers
    /// the complete set of remaining Key commitments.
    pub fn reserve_credit_quote_with_upstream_snapshot(
        &self,
        quote: BillingQuote,
        snapshot: UpstreamCreditSnapshot,
    ) -> Result<BillingReservationResult, CoreError> {
        self.reserve_credit_quote_inner(quote, Some(snapshot))
    }

    fn reserve_credit_quote_inner(
        &self,
        quote: BillingQuote,
        upstream_snapshot: Option<UpstreamCreditSnapshot>,
    ) -> Result<BillingReservationResult, CoreError> {
        if quote.unit != "credits"
            || quote.max_credits.as_microcredits() <= 0
            || quote.quote_id.trim().is_empty()
            || quote.source_ref.trim().is_empty()
        {
            return Err(CoreError::BillingQuoteMismatch {
                request_id: quote.request_id,
            });
        }
        let supplied_fingerprint = URL_SAFE_NO_PAD
            .decode(&quote.request_fingerprint)
            .map_err(|_| CoreError::BillingQuoteMismatch {
                request_id: quote.request_id.clone(),
            })?;
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let request = transaction
            .query_row(
                "SELECT user_id, api_key_id, endpoint, model, request_hash, state
                 FROM requests WHERE id = ?1",
                [&quote.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: quote.request_id.clone(),
            })?;
        if request.2 != quote.endpoint
            || request.3 != quote.model
            || request.4 != supplied_fingerprint
            || quote.expires_at_ms <= now
        {
            return Err(if quote.expires_at_ms <= now {
                CoreError::BillingQuoteExpired {
                    request_id: quote.request_id,
                }
            } else {
                CoreError::BillingQuoteMismatch {
                    request_id: quote.request_id,
                }
            });
        }

        let existing_quote = transaction
            .query_row(
                "SELECT quote_id, request_fingerprint, endpoint, model, max_credits, unit,
                        source_ref, expires_at_ms
                 FROM billing_quotes WHERE request_id = ?1",
                [&quote.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
            .optional()?;
        if let Some(existing) = existing_quote {
            let exact_match = existing
                == (
                    quote.quote_id.clone(),
                    quote.request_fingerprint.clone(),
                    quote.endpoint.clone(),
                    quote.model.clone(),
                    quote.max_credits.as_microcredits(),
                    quote.unit.clone(),
                    quote.source_ref.clone(),
                    quote.expires_at_ms,
                );
            if !exact_match {
                return Err(CoreError::BillingQuoteConflict {
                    request_id: quote.request_id,
                });
            }
            let reservation = Self::reservation_by_request(&transaction, &quote.request_id)?
                .ok_or_else(|| CoreError::BillingQuoteConflict {
                    request_id: quote.request_id.clone(),
                })?;
            let request = Self::request_handle_in_transaction(&transaction, &quote.request_id)?;
            transaction.commit()?;
            return Ok(BillingReservationResult::Existing { request, reservation });
        }

        if let Some(snapshot) = upstream_snapshot.as_ref() {
            Self::ensure_fresh_upstream_credit_snapshot(snapshot, now)?;
            Self::upstream_credit_capacity_in_transaction(&transaction, snapshot)?;
        }

        let request_state = RequestState::from_db(&request.5).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: request.5.clone(),
            }
        })?;
        if request_state != RequestState::Received {
            return Err(CoreError::InvalidTransition {
                request_id: quote.request_id,
                expected: request_state,
                next: RequestState::Validating,
            });
        }

        let active_key = transaction.query_row(
            "SELECT max_concurrency FROM api_keys
             WHERE id = ?1 AND user_id = ?2 AND status = 'active'",
            params![&request.1, &request.0],
            |row| row.get::<_, i64>(0),
        ).optional()?.ok_or_else(|| CoreError::InvalidRequestIdentity {
            user_id: request.0.clone(),
            api_key_id: request.1.clone(),
        })?;
        let key_blocked = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM api_key_billing_blocks WHERE key_id = ?1)",
            [&request.1],
            |row| row.get::<_, bool>(0),
        )?;
        if key_blocked {
            return Err(CoreError::ApiKeyBillingBlocked {
                api_key_id: request.1,
            });
        }

        let active_concurrency: i64 = transaction.query_row(
            "SELECT COUNT(*) FROM requests
             WHERE api_key_id = ?1 AND id <> ?2
               AND state IN ('received','validating','reserved','queued','dispatched','completing','unknown')",
            params![&request.1, &quote.request_id],
            |row| row.get(0),
        )?;
        if active_concurrency >= active_key {
            return Err(CoreError::KeyConcurrencyExceeded {
                api_key_id: request.1,
                active_concurrency,
                max_concurrency: active_key,
            });
        }

        let ttl_ms = quote.expires_at_ms - now;
        let expires_at_ms = quote.expires_at_ms;
        let reserve = QuotaReserve {
            user_id: request.0.clone(),
            request_id: quote.request_id.clone(),
            resource_kind: "credits".into(),
            amount: quote.max_credits.as_microcredits(),
            ttl_ms,
        };
        Self::transition_request_on_connection(
            &transaction,
            &quote.request_id,
            RequestState::Received,
            RequestState::Validating,
            None,
            now,
        )?;
        let reservation = match Self::reserve_dual_in_transaction(
            &transaction,
            &reserve,
            &request.1,
            now,
            expires_at_ms,
        )? {
            ReserveResult::Created(reservation) => reservation,
            ReserveResult::Existing(_) => {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: quote.request_id,
                });
            }
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: reserve.amount,
                });
            }
        };
        transaction.execute(
            "INSERT INTO billing_quotes
             (quote_id, request_id, request_fingerprint, endpoint, model, max_credits, unit,
              source_ref, expires_at_ms, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &quote.quote_id,
                &quote.request_id,
                &quote.request_fingerprint,
                &quote.endpoint,
                &quote.model,
                quote.max_credits.as_microcredits(),
                &quote.unit,
                &quote.source_ref,
                quote.expires_at_ms,
                now,
            ],
        )?;
        Self::transition_request_on_connection(
            &transaction,
            &quote.request_id,
            RequestState::Validating,
            RequestState::Reserved,
            None,
            now,
        )?;
        let request = Self::request_handle_in_transaction(&transaction, &quote.request_id)?;
        transaction.commit()?;
        Ok(BillingReservationResult::Created { request, reservation })
    }

    /// Applies a billing observation to the Key recorded locally for the
    /// request. Receipt payloads never select a user or Key. Non-final and
    /// unverifiable observations keep the reservation held for reconciliation.
    pub fn apply_credit_receipt(
        &self,
        receipt: BillingReceipt,
    ) -> Result<BillingReceiptResult, CoreError> {
        if receipt.request_id.trim().is_empty()
            || receipt.source_ref.trim().is_empty()
            || receipt.observed_at_ms <= 0
        {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "request_id, source_ref, and observed_at_ms are required".into(),
            });
        }
        let receipt_hash = crate::canonical_json_hash(&serde_json::to_value(&receipt)?).to_vec();
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (user_id, api_key_id, request_state_value) = transaction
            .query_row(
                "SELECT user_id, api_key_id, state FROM requests WHERE id = ?1",
                [&receipt.request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| CoreError::RequestNotFound {
                request_id: receipt.request_id.clone(),
            })?;

        let duplicate = transaction
            .query_row(
                "SELECT status FROM billing_receipts WHERE request_id = ?1 AND receipt_hash = ?2",
                params![&receipt.request_id, &receipt_hash],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(status) = duplicate {
            transaction.commit()?;
            return Ok(if matches!(status.as_str(), "final" | "failed_no_charge" | "conflict") {
                BillingReceiptResult::Duplicate
            } else {
                BillingReceiptResult::Pending
            });
        }

        let quote = transaction
            .query_row(
                "SELECT quote_id, max_credits FROM billing_quotes WHERE request_id = ?1",
                [&receipt.request_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or_else(|| CoreError::BillingQuoteMismatch {
                request_id: receipt.request_id.clone(),
            })?;
        let reservation = Self::reservation_by_request(&transaction, &receipt.request_id)?
            .ok_or_else(|| CoreError::ReservationNotFound {
                reservation_id: receipt.request_id.clone(),
            })?;
        let key_account_matches = if let Some(account_id) = reservation.key_budget_account_id.as_ref() {
            transaction.query_row(
                "SELECT EXISTS(
                    SELECT 1 FROM quota_budget_accounts
                    WHERE id = ?1 AND scope = 'key' AND user_id = ?2
                      AND api_key_id = ?3 AND resource_kind = 'credits'
                )",
                params![account_id, &user_id, &api_key_id],
                |row| row.get::<_, bool>(0),
            )?
        } else {
            false
        };
        let reservation_key_matches = reservation.user_id == user_id
            && reservation.api_key_id.as_deref() == Some(api_key_id.as_str())
            && reservation.resource_kind == "credits"
            && reservation.amount == quote.1
            && key_account_matches;
        if !reservation_key_matches {
            return Err(CoreError::BillingReceiptInvalid {
                reason: "request, Key, quote, and reservation identity do not match".into(),
            });
        }

        let mut normalized_status = receipt.status;
        let mut actual_credits = receipt.actual_credits.map(CreditAmount::as_microcredits);
        let valid_unit = receipt.unit == "credits";
        let valid_final = match receipt.status {
            BillingReceiptStatus::Final => valid_unit && actual_credits.is_some(),
            BillingReceiptStatus::FailedNoCharge => {
                valid_unit && actual_credits.map_or(true, |actual| actual == 0)
            }
            BillingReceiptStatus::Pending
            | BillingReceiptStatus::Unknown
            | BillingReceiptStatus::Unverified => false,
        };
        if !valid_unit
            || (matches!(
                receipt.status,
                BillingReceiptStatus::Final | BillingReceiptStatus::FailedNoCharge
            ) && !valid_final)
        {
            normalized_status = BillingReceiptStatus::Unverified;
            actual_credits = None;
        }

        let settled = transaction
            .query_row(
                "SELECT actual_credits FROM billing_settlements WHERE request_id = ?1",
                [&receipt.request_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if let Some(existing_actual) = settled {
            if !valid_final || receipt.unit != "credits" {
                transaction.commit()?;
                return Ok(BillingReceiptResult::Duplicate);
            }
            let incoming_actual = if receipt.status == BillingReceiptStatus::FailedNoCharge {
                0
            } else {
                actual_credits.unwrap_or(0)
            };
            if incoming_actual == existing_actual {
                Self::insert_billing_receipt(
                    &transaction,
                    &receipt,
                    &receipt_hash,
                    normalized_status.as_str(),
                    Some(incoming_actual),
                    now,
                )?;
                transaction.commit()?;
                return Ok(BillingReceiptResult::Duplicate);
            }
            Self::insert_billing_receipt(
                &transaction,
                &receipt,
                &receipt_hash,
                "conflict",
                Some(incoming_actual),
                now,
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO api_key_billing_blocks
                 (key_id, request_id, reason, quote_max_credits, actual_credits,
                  excess_credits, source_ref, blocked_at_ms)
                 VALUES (?1, ?2, 'receipt_conflict', ?3, ?4, 0, ?5, ?6)",
                params![
                    &api_key_id,
                    &receipt.request_id,
                    quote.1,
                    incoming_actual,
                    &receipt.source_ref,
                    now,
                ],
            )?;
            transaction.execute(
                "UPDATE billing_settlements SET reconcile_required = 1 WHERE request_id = ?1",
                [&receipt.request_id],
            )?;
            transaction.commit()?;
            return Ok(BillingReceiptResult::Conflict);
        }

        let stored_actual = if normalized_status == BillingReceiptStatus::FailedNoCharge {
            Some(0)
        } else {
            actual_credits
        };
        let receipt_id = Self::insert_billing_receipt(
            &transaction,
            &receipt,
            &receipt_hash,
            normalized_status.as_str(),
            stored_actual,
            now,
        )?;

        if !valid_final || normalized_status == BillingReceiptStatus::Unverified {
            if reservation.state == ReservationState::Held {
                Self::set_reservation_state(
                    &transaction,
                    &reservation.id,
                    ReservationState::Unknown,
                    now,
                )?;
            }
            let request_state = RequestState::from_db(&request_state_value).ok_or_else(|| {
                CoreError::InvalidConfiguration {
                    key: "requests.state".into(),
                    value: request_state_value.clone(),
                }
            })?;
            if request_state != RequestState::Unknown {
                Self::transition_request_on_connection(
                    &transaction,
                    &receipt.request_id,
                    request_state,
                    RequestState::Unknown,
                    None,
                    now,
                )?;
            }
            transaction.commit()?;
            return Ok(BillingReceiptResult::Pending);
        }

        if !matches!(reservation.state, ReservationState::Held | ReservationState::Unknown) {
            return Err(CoreError::ReservationSettlementConflict {
                reservation_id: reservation.id,
            });
        }
        let actual = stored_actual.ok_or_else(|| CoreError::BillingReceiptInvalid {
            reason: "final receipt has no actual credit amount".into(),
        })?;
        let over_quote = actual > quote.1;
        let refund_delta = reservation
            .amount
            .checked_sub(actual)
            .ok_or(CoreError::InvalidQuotaAmount)?;
        Self::set_reservation_state(
            &transaction,
            &reservation.id,
            ReservationState::Committed,
            now,
        )?;
        if reservation.key_budget_account_id.is_some() {
            Self::insert_reservation_budget_event(
                &transaction,
                &reservation,
                "commit",
                actual,
                refund_delta,
                None,
                now,
                reservation.event_group_id.as_deref().ok_or_else(|| {
                    CoreError::InvalidConfiguration {
                        key: "quota_reservations.event_group_id".into(),
                        value: reservation.id.clone(),
                    }
                })?,
            )?;
        } else {
            Self::insert_ledger_entry(
                &transaction,
                &user_id,
                "credits",
                "commit",
                actual,
                refund_delta,
                Some(&receipt.request_id),
                None,
                None,
                now,
            )?;
        }
        transaction.execute(
            "INSERT INTO billing_settlements
             (request_id, quote_id, receipt_id, actual_credits, over_quote, reconcile_required, settled_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![
                &receipt.request_id,
                &quote.0,
                &receipt_id,
                actual,
                i64::from(over_quote),
                now,
            ],
        )?;
        let request_state = RequestState::from_db(&request_state_value).ok_or_else(|| {
            CoreError::InvalidConfiguration {
                key: "requests.state".into(),
                value: request_state_value,
            }
        })?;
        let final_state = if normalized_status == BillingReceiptStatus::FailedNoCharge {
            RequestState::Failed
        } else {
            RequestState::Succeeded
        };
        Self::settle_request_state(
            &transaction,
            &receipt.request_id,
            request_state,
            final_state,
            None,
            now,
        )?;
        if over_quote {
            let excess = actual.checked_sub(quote.1).ok_or(CoreError::InvalidQuotaAmount)?;
            transaction.execute(
                "INSERT OR IGNORE INTO api_key_billing_blocks
                 (key_id, request_id, reason, quote_max_credits, actual_credits,
                  excess_credits, source_ref, blocked_at_ms)
                 VALUES (?1, ?2, 'over_quote', ?3, ?4, ?5, ?6, ?7)",
                params![
                    &api_key_id,
                    &receipt.request_id,
                    quote.1,
                    actual,
                    excess,
                    &receipt.source_ref,
                    now,
                ],
            )?;
        }
        transaction.commit()?;

        Ok(BillingReceiptResult::Settled {
            api_key_id,
            actual_credits: CreditAmount::try_from_microcredits(actual).ok_or_else(|| {
                CoreError::BillingReceiptInvalid {
                    reason: "final receipt amount is outside the supported range".into(),
                }
            })?,
            over_quote,
        })
    }

    fn insert_billing_receipt(
        transaction: &Transaction<'_>,
        receipt: &BillingReceipt,
        receipt_hash: &[u8],
        status: &str,
        actual_credits: Option<i64>,
        now: i64,
    ) -> Result<String, CoreError> {
        let receipt_id = Self::new_id("billing-receipt");
        transaction.execute(
            "INSERT INTO billing_receipts
             (receipt_id, request_id, receipt_hash, status, actual_credits, unit,
              source_ref, task_ref, observed_at_ms, received_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                &receipt_id,
                &receipt.request_id,
                receipt_hash,
                status,
                actual_credits,
                &receipt.unit,
                &receipt.source_ref,
                receipt.task_ref.as_deref(),
                receipt.observed_at_ms,
                now,
            ],
        )?;
        Ok(receipt_id)
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
            if reservation.key_budget_account_id.is_some() {
                Self::validate_dual_settlement_replay(&transaction, &reservation, &settlement)?;
            }
            let balance = if let Some(account_id) = reservation.key_budget_account_id.as_deref() {
                Self::quota_balance_from_budget(&Self::budget_balance_in_transaction(&transaction, account_id)?)
            } else {
                Self::balance_in_transaction(
                    &transaction,
                    &reservation.user_id,
                    &reservation.resource_kind,
                )?
            };
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

        Self::apply_settlement_for_reservation(&transaction, &reservation, settlement, now)?;

        let balance = if let Some(account_id) = reservation.key_budget_account_id.as_deref() {
            Self::quota_balance_from_budget(&Self::budget_balance_in_transaction(&transaction, account_id)?)
        } else {
            Self::balance_in_transaction(
                &transaction,
                &reservation.user_id,
                &reservation.resource_kind,
            )?
        };
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
            api_key_id: None,
            key_budget_account_id: None,
            user_cap_account_id: None,
            event_group_id: None,
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

    pub(crate) fn reserve_dual_in_transaction(
        transaction: &Transaction<'_>,
        input: &QuotaReserve,
        api_key_id: &str,
        now: i64,
        expires_at_ms: i64,
    ) -> Result<ReserveResult, CoreError> {
        if let Some(reservation) = Self::reservation_by_request(transaction, &input.request_id)? {
            if reservation.user_id != input.user_id
                || reservation.resource_kind != input.resource_kind
                || reservation.api_key_id.as_deref() != Some(api_key_id)
            {
                return Err(CoreError::ReservationRequestConflict {
                    request_id: input.request_id.clone(),
                });
            }
            return Ok(ReserveResult::Existing(reservation));
        }

        let key_account = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'key' AND api_key_id = ?1 AND user_id = ?2 AND resource_kind = ?3
                 ORDER BY id LIMIT 1",
                params![api_key_id, &input.user_id, &input.resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?
            .ok_or_else(|| CoreError::KeyQuotaNotConfigured {
                api_key_id: api_key_id.into(),
                resource_kind: input.resource_kind.clone(),
            })?;
        Self::ensure_ready_budget_account(&key_account)?;
        let key_balance = Self::budget_balance_in_transaction(transaction, &key_account.id)?;

        let user_cap_account = transaction
            .query_row(
                "SELECT id, scope, user_id, api_key_id, resource_kind, enabled, version, migration_state
                 FROM quota_budget_accounts
                 WHERE scope = 'user_cap' AND user_id = ?1 AND resource_kind = ?2
                 ORDER BY id LIMIT 1",
                params![&input.user_id, &input.resource_kind],
                Self::budget_account_from_row,
            )
            .optional()?;
        if let Some(account) = user_cap_account.as_ref() {
            Self::ensure_ready_budget_account(account)?;
        }
        let user_cap_balance = user_cap_account
            .as_ref()
            .map(|account| Self::budget_balance_in_transaction(transaction, &account.id))
            .transpose()?;
        let available = user_cap_balance
            .as_ref()
            .map_or(key_balance.available, |balance| key_balance.available.min(balance.available));
        if available < input.amount {
            return Ok(ReserveResult::Insufficient { available });
        }

        let event_group_id = Self::new_id("quota-event");
        let reservation = Reservation {
            id: Self::new_id("reservation"),
            user_id: input.user_id.clone(),
            request_id: input.request_id.clone(),
            resource_kind: input.resource_kind.clone(),
            amount: input.amount,
            state: ReservationState::Held,
            expires_at_ms,
            api_key_id: Some(api_key_id.into()),
            key_budget_account_id: Some(key_account.id.clone()),
            user_cap_account_id: user_cap_account.as_ref().map(|account| account.id.clone()),
            event_group_id: Some(event_group_id.clone()),
        };
        transaction.execute(
            "INSERT INTO quota_reservations
             (id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
              created_at_ms, api_key_id, key_budget_account_id, user_cap_account_id, event_group_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                &reservation.id,
                &reservation.user_id,
                &reservation.request_id,
                &reservation.resource_kind,
                reservation.amount,
                reservation.state.as_str(),
                reservation.expires_at_ms,
                now,
                api_key_id,
                &key_account.id,
                reservation.user_cap_account_id.as_deref(),
                &event_group_id,
            ],
        )?;
        Self::insert_budget_ledger_entry(
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
            Some(&key_account.id),
            Some(&event_group_id),
            Some(api_key_id),
            key_account.version,
        )?;
        if let Some(account) = user_cap_account {
            Self::insert_budget_ledger_entry(
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
                Some(&account.id),
                Some(&event_group_id),
                Some(api_key_id),
                account.version,
            )?;
        }
        Ok(ReserveResult::Created(reservation))
    }

    pub(crate) fn apply_settlement(
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

    pub(crate) fn apply_settlement_for_reservation(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        settlement: Settlement,
        now: i64,
    ) -> Result<(), CoreError> {
        if reservation.key_budget_account_id.is_some() {
            Self::apply_dual_settlement(transaction, reservation, settlement, now)
        } else {
            Self::apply_settlement(transaction, reservation, settlement, now)
        }
    }

    fn apply_dual_settlement(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        settlement: Settlement,
        now: i64,
    ) -> Result<(), CoreError> {
        let event_group_id = reservation
            .event_group_id
            .as_deref()
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "quota_reservations.event_group_id".into(),
                value: reservation.id.clone(),
            })?;
        match settlement {
            Settlement::Release => {
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Released, now)?;
                Self::insert_reservation_budget_event(
                    transaction,
                    reservation,
                    "release",
                    reservation.amount,
                    reservation.amount,
                    None,
                    now,
                    event_group_id,
                )?;
            }
            Settlement::Commit { actual_amount: Some(actual_amount) } => {
                if actual_amount < 0 {
                    return Err(CoreError::InvalidQuotaAmount);
                }
                if actual_amount > reservation.amount {
                    return Err(CoreError::ActualAmountExceedsReservation);
                }
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Committed, now)?;
                Self::insert_reservation_budget_event(
                    transaction,
                    reservation,
                    "commit",
                    actual_amount,
                    reservation.amount - actual_amount,
                    None,
                    now,
                    event_group_id,
                )?;
            }
            Settlement::Commit { actual_amount: None } | Settlement::Unknown => {
                Self::set_reservation_state(transaction, &reservation.id, ReservationState::Unknown, now)?;
            }
        }
        Ok(())
    }

    fn insert_reservation_budget_event(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        event_kind: &str,
        amount: i64,
        delta: i64,
        reason: Option<&str>,
        now: i64,
        event_group_id: &str,
    ) -> Result<(), CoreError> {
        let key_account_id = reservation
            .key_budget_account_id
            .as_deref()
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "quota_reservations.key_budget_account_id".into(),
                value: reservation.id.clone(),
            })?;
        let key_version = Self::budget_account_version(transaction, key_account_id)?;
        Self::insert_budget_ledger_entry(
            transaction,
            &reservation.user_id,
            &reservation.resource_kind,
            event_kind,
            amount,
            delta,
            Some(&reservation.request_id),
            None,
            reason,
            now,
            Some(key_account_id),
            Some(event_group_id),
            reservation.api_key_id.as_deref(),
            key_version,
        )?;
        if let Some(user_account_id) = reservation.user_cap_account_id.as_deref() {
            let user_version = Self::budget_account_version(transaction, user_account_id)?;
            Self::insert_budget_ledger_entry(
                transaction,
                &reservation.user_id,
                &reservation.resource_kind,
                event_kind,
                amount,
                delta,
                Some(&reservation.request_id),
                None,
                reason,
                now,
                Some(user_account_id),
                Some(event_group_id),
                reservation.api_key_id.as_deref(),
                user_version,
            )?;
        }
        Ok(())
    }

    fn validate_dual_settlement_replay(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        settlement: &Settlement,
    ) -> Result<(), CoreError> {
        let compatible = match reservation.state {
            ReservationState::Released => matches!(settlement, Settlement::Release),
            ReservationState::Unknown => matches!(settlement, Settlement::Unknown | Settlement::Commit { actual_amount: None }),
            ReservationState::Committed => {
                if let Settlement::Commit { actual_amount: Some(actual_amount) } = settlement {
                    let committed_amount: Option<i64> = transaction.query_row(
                        "SELECT amount FROM quota_ledger
                         WHERE budget_account_id = ?1 AND event_group_id = ?2 AND event_kind = 'commit'
                         ORDER BY entry_id LIMIT 1",
                        params![reservation.key_budget_account_id.as_deref(), reservation.event_group_id.as_deref()],
                        |row| row.get(0),
                    ).optional()?;
                    committed_amount == Some(*actual_amount)
                } else {
                    false
                }
            }
            ReservationState::Held => true,
        };
        if compatible {
            Ok(())
        } else {
            Err(CoreError::ReservationSettlementConflict {
                reservation_id: reservation.id.clone(),
            })
        }
    }

    fn budget_account_version(
        transaction: &Transaction<'_>,
        account_id: &str,
    ) -> Result<i64, CoreError> {
        transaction
            .query_row(
                "SELECT version FROM quota_budget_accounts WHERE id = ?1",
                [account_id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "quota_budget_accounts.id".into(),
                value: account_id.into(),
            })
    }

    fn quota_balance_from_budget(balance: &QuotaBudgetBalance) -> QuotaBalance {
        QuotaBalance {
            user_id: balance.user_id.clone(),
            resource_kind: balance.resource_kind.clone(),
            available: balance.available,
            held: balance.held,
        }
    }

    pub(crate) fn validate_reservation_owner(
        transaction: &Transaction<'_>,
        reservation: &Reservation,
        principal: &Principal,
    ) -> Result<(), CoreError> {
        if reservation.user_id != principal.user_id {
            return Err(CoreError::ReservationOwnerMismatch {
                reservation_id: reservation.id.clone(),
            });
        }
        if reservation.api_key_id.as_deref().is_some_and(|key_id| key_id != principal.key_id) {
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

    pub(crate) fn settle_request_state(
        transaction: &Transaction<'_>,
        request_id: &str,
        current: RequestState,
        final_state: RequestState,
        result: Option<RequestResult>,
        now: i64,
    ) -> Result<(), CoreError> {
        let intermediate: Vec<RequestState> = match final_state {
            RequestState::Succeeded => {
                if current == RequestState::Unknown {
                    vec![RequestState::Succeeded]
                } else {
                    let progression = [
                        RequestState::Reserved,
                        RequestState::Queued,
                        RequestState::Dispatched,
                        RequestState::Completing,
                        RequestState::Succeeded,
                    ];
                    let Some(position) = progression.iter().position(|state| *state == current) else {
                        return Err(CoreError::InvalidTransition {
                            request_id: request_id.to_owned(), expected: current, next: final_state,
                        });
                    };
                    progression[position + 1..].to_vec()
                }
            }
            RequestState::Failed | RequestState::Unknown => vec![final_state],
            RequestState::Canceled => {
                if current != RequestState::CancelRequested && current != RequestState::Unknown {
                    return Err(CoreError::InvalidTransition {
                        request_id: request_id.to_owned(),
                        expected: current,
                        next: final_state,
                    });
                }
                vec![RequestState::Canceled]
            }
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
        if expected == RequestState::Settled { Ok(()) } else {
            Self::transition_request_on_connection(
                transaction,
                request_id,
                expected,
                RequestState::Settled,
                None,
                now,
            )
        }
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
             COALESCE((SELECT SUM(delta) FROM quota_ledger
                       WHERE user_id = ?1 AND resource_kind = ?2
                         AND (budget_account_id IS NULL OR budget_account_id IN
                              (SELECT id FROM quota_budget_accounts WHERE scope = 'user_cap'))), 0), \
             COALESCE((SELECT SUM(amount) FROM quota_reservations \
                       WHERE user_id = ?1 AND resource_kind = ?2
                         AND state IN ('held', 'unknown')
                         AND (key_budget_account_id IS NULL OR user_cap_account_id IS NOT NULL)), 0)",
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
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                        api_key_id, key_budget_account_id, user_cap_account_id, event_group_id \
                 FROM quota_reservations WHERE request_id = ?1",
                [request_id],
                Self::reservation_from_row,
            )
            .optional()
            .map_err(CoreError::from)
    }

    pub(crate) fn reservation_by_id(
        transaction: &Transaction<'_>,
        reservation_id: &str,
    ) -> Result<Option<Reservation>, CoreError> {
        transaction
            .query_row(
                "SELECT id, user_id, request_id, resource_kind, amount, state, expires_at_ms,
                        api_key_id, key_budget_account_id, user_cap_account_id, event_group_id \
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
            api_key_id: row.get(7)?,
            key_budget_account_id: row.get(8)?,
            user_cap_account_id: row.get(9)?,
            event_group_id: row.get(10)?,
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

    #[allow(clippy::too_many_arguments)]
    fn insert_budget_ledger_entry(
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
        budget_account_id: Option<&str>,
        event_group_id: Option<&str>,
        api_key_id: Option<&str>,
        budget_version: i64,
    ) -> Result<(), CoreError> {
        transaction.execute(
            "INSERT INTO quota_ledger
             (entry_id, user_id, resource_kind, event_kind, amount, delta, request_id,
              actor_user_id, reason, created_at_ms, budget_account_id, event_group_id,
              api_key_id, budget_version)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
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
                budget_account_id,
                event_group_id,
                api_key_id,
                budget_version,
            ],
        )?;
        Ok(())
    }
}
