use chrono::Utc;
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{
    CostError, CostPolicy, BeginRequest, BeginRequestInput, CoreError, CoreStore, RequestHandle,
    RequestResult, RequestState,
};

pub fn canonical_json_hash(value: &Value) -> [u8; 32] {
    let mut canonical = Vec::new();
    write_canonical_json(value, &mut canonical);
    Sha256::digest(canonical).into()
}

impl CoreStore {
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
        let request_hash = request_hash(&input.endpoint, &input.model, &input.body);
        let scope = format!("{}:{}", input.user_id, input.endpoint);
        let now = Utc::now().timestamp_millis();
        let mut connection = self.connection.lock().expect("core store mutex poisoned");
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

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

        Self::estimate_in_transaction(&transaction, &input.endpoint, &input.model, &input.body)?;
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

    pub fn transition_request(
        &self,
        request_id: &str,
        expected: RequestState,
        next: RequestState,
        result: Option<RequestResult>,
    ) -> Result<(), CoreError> {
        if !expected.can_transition_to(next) {
            return Err(CoreError::InvalidTransition {
                request_id: request_id.to_owned(),
                expected,
                next,
            });
        }

        let now = Utc::now().timestamp_millis();
        let connection = self.connection.lock().expect("core store mutex poisoned");
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
        let mut statement = transaction.prepare(
            "SELECT id, endpoint, model_pattern, resource_kind, reserve_amount, max_actual_amount, version, enabled \
             FROM cost_policies WHERE endpoint = ?1 AND enabled = 1 ORDER BY version DESC",
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
                Ok(_) => return Ok(()),
                Err(CostError::BudgetPolicyMissing { .. }) => {}
            }
        }
        Err(CoreError::BudgetPolicyMissing {
            endpoint: endpoint.to_owned(),
            model: model.to_owned(),
        })
    }

    fn request_handle_in_transaction(
        transaction: &Transaction<'_>,
        request_id: &str,
    ) -> Result<RequestHandle, CoreError> {
        transaction.query_row(
            "SELECT id, user_id, api_key_id, protocol, endpoint, model, state FROM requests WHERE id = ?1",
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
                })
            },
        ).map_err(CoreError::from)
    }
}

fn request_hash(endpoint: &str, model: &str, body: &Value) -> [u8; 32] {
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
