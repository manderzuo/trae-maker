use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

use crate::{
    CoreError, CoreQuotaBalanceView, CoreQuotaLedgerView, CoreQuotaUsageView, CoreStore,
    KeyQuotaGrant, LegacyQuotaAllocation, Principal, QuotaBalance, QuotaBudgetBalance,
    QuotaBudgetScope, QuotaGrant, QuotaMigrationState, QuotaReserve, RequestResult, RequestState,
    Reservation, ReservationState, ReserveResult, Settlement,
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
        Ok(CoreQuotaUsageView { balances, ledger })
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

    fn ensure_ready_budget_account(account: &BudgetAccountRecord) -> Result<(), CoreError> {
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

    fn budget_balance_in_transaction(
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

    pub(crate) fn reservation_by_id(
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
