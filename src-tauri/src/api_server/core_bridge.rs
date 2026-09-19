use std::{path::Path, sync::Arc};

use aiwork_core::{
    require_scope, BeginRequest, BeginRequestInput, ChatExecutionRequest, ChatExecutionResult,
    CoreError, CoreStore, Principal, QuotaReserve, Reservation, ReserveResult,
    Settlement, UpstreamError,
};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreMode {
    Off,
    Shadow,
    Enforce,
}

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
    pub reservation: Reservation,
    pub execution: ChatExecutionRequest,
    pub replayed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatOutcome {
    Success(ChatExecutionResult),
    Failure(UpstreamError),
    Upstream(UpstreamError),
}

pub struct CoreBridge {
    pub store: Arc<CoreStore>,
    pub mode: CoreMode,
}

impl CoreBridge {
    pub fn new(store: Arc<CoreStore>, mode: CoreMode) -> Self {
        Self { store, mode }
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
        require_scope(principal, "chat:invoke").map_err(|_| CoreError::MissingScope {
            scope: "chat:invoke".into(),
        })?;
        let endpoint = body
            .get("endpoint")
            .and_then(Value::as_str)
            .unwrap_or("chat");
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| CoreError::InvalidConfiguration {
                key: "chat.model".into(),
                value: "missing or non-string".into(),
            })?;

        // Cost policy is checked before begin_request so a missing policy
        // cannot create a request record or consume a client idempotency key.
        let estimate = self.store.estimate_cost(endpoint, model, body)?;
        if principal.key_id != api_key_id {
            return Err(CoreError::InvalidRequestIdentity {
                user_id: principal.user_id.clone(),
                api_key_id: api_key_id.into(),
            });
        }
        let idempotency_key = client_idempotency_key
            .filter(|key| !key.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| generated_idempotency_key(endpoint, model, body));
        let begin = self.store.begin_request(BeginRequestInput {
            user_id: principal.user_id.clone(),
            api_key_id: api_key_id.into(),
            protocol: "openai".into(),
            endpoint: endpoint.into(),
            model: model.into(),
            idempotency_key,
            body: body.clone(),
        })?;
        let (handle, replayed) = match begin {
            BeginRequest::Created(handle) => (handle, false),
            BeginRequest::Existing(handle) => (handle, true),
            BeginRequest::Conflict => return Err(CoreError::IdempotencyConflict),
        };

        let reservation = match self.store.reserve(QuotaReserve {
            user_id: principal.user_id.clone(),
            request_id: handle.id.clone(),
            resource_kind: estimate.resource_kind,
            amount: estimate.reserve_amount,
            ttl_ms: 15 * 60 * 1000,
        })? {
            ReserveResult::Created(reservation) | ReserveResult::Existing(reservation) => reservation,
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: estimate.reserve_amount,
                })
            }
        };

        Ok(PreflightResult {
            reservation,
            execution: ChatExecutionRequest {
                request_id: handle.id,
                endpoint: endpoint.into(),
                model: model.into(),
                body: body.clone(),
            },
            replayed,
        })
    }

    pub fn settle_chat(&self, reservation_id: &str, outcome: ChatOutcome) -> Result<(), CoreError> {
        let settlement = match outcome {
            ChatOutcome::Success(result) => Settlement::Commit {
                actual_amount: result.actual_amount,
            },
            ChatOutcome::Failure(error) | ChatOutcome::Upstream(error) => {
                if error.is_uncertain() {
                    Settlement::Unknown
                } else {
                    Settlement::Release
                }
            }
        };
        self.store
            .settle(reservation_id, settlement)
            .map(|_| ())
            .map_err(|source| CoreError::ReservationContext {
                reservation_id: reservation_id.into(),
                source: Box::new(source),
            })
    }

    pub fn balance(
        &self,
        user_id: &str,
        resource_kind: &str,
    ) -> Result<aiwork_core::QuotaBalance, CoreError> {
        self.store.balance(user_id, resource_kind)
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
    use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

    use aiwork_core::{
        CoreError, CoreStore, CostPolicy, NewUser, QuotaGrant, ReservationState, UserRole,
        UpstreamError,
    };
    use serde_json::json;

    use super::*;

    fn test_dir(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aiwork-tauri-core-bridge-{prefix}-{}",
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn bridge_with_grant(grant: i64) -> (CoreBridge, aiwork_core::Principal) {
        let store = Arc::new(CoreStore::open(&test_dir("bridge")).unwrap());
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
        (CoreBridge::new(store, CoreMode::Enforce), principal)
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
        let (bridge, mut principal) = bridge_with_grant(1);
        principal.scopes.clear();
        principal.key_id = "missing-key".into();

        let error = bridge
            .preflight_chat(&principal, "missing-key", Some("idem-scope"), &chat_body())
            .unwrap_err();
        assert!(matches!(error, CoreError::MissingScope { scope } if scope == "chat:invoke"));
    }

    #[test]
    fn preflight_reports_missing_policy_before_identity_mismatch() {
        let (bridge, mut principal) = bridge_with_grant(1);
        principal.key_id = "different-key".into();
        let body = json!({"model": "not-priced", "messages": []});

        let error = bridge
            .preflight_chat(&principal, "missing-key", Some("idem-order"), &body)
            .unwrap_err();
        assert!(matches!(error, CoreError::BudgetPolicyMissing { .. }));
    }

    #[test]
    fn preflight_reserves_before_execution_and_success_commits() {
        let (bridge, principal) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-success"), &chat_body())
            .unwrap();
        assert_eq!(first.reservation.amount, 1);
        assert_eq!(first.execution.model, "mock-1");

        bridge
            .settle_chat(
                &first.reservation.id,
                ChatOutcome::Success(aiwork_core::ChatExecutionResult::ok()),
            )
            .unwrap();
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().available, 0);
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 0);
    }

    #[test]
    fn insufficient_budget_fails_preflight_without_reservation() {
        let (bridge, principal) = bridge_with_grant(0);
        let error = bridge
            .preflight_chat(
                &principal,
                &principal.key_id,
                Some("idem-insufficient"),
                &chat_body(),
            )
            .unwrap_err();
        assert!(matches!(error, CoreError::QuotaInsufficient { .. }));
    }

    #[test]
    fn same_idempotency_key_reuses_one_reservation() {
        let (bridge, principal) = bridge_with_grant(2);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-repeat"), &chat_body())
            .unwrap();
        let second = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-repeat"), &chat_body())
            .unwrap();

        assert_eq!(first.reservation.id, second.reservation.id);
        assert!(second.replayed);
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
    }

    #[test]
    fn timeout_and_disconnect_settle_as_unknown() {
        let (bridge, principal) = bridge_with_grant(1);
        let first = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-timeout"), &chat_body())
            .unwrap();
        bridge
            .settle_chat(&first.reservation.id, ChatOutcome::Upstream(UpstreamError::Timeout))
            .unwrap();

        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
        assert_eq!(first.reservation.state, ReservationState::Held);
    }
}
