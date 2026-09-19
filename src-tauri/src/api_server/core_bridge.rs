use std::{path::Path, sync::Arc};

use aiwork_core::{
    require_scope, BeginRequest, BeginRequestInput, ChatExecutionRequest, ChatExecutionResult,
    CoreError, CoreStore, Principal, QuotaReserve, RequestResult, RequestState, Reservation,
    ReserveResult, Settlement, UpstreamError,
};
use serde_json::Value;

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
    pub reservation: Option<Reservation>,
    pub execution: Option<ChatExecutionRequest>,
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

        // Cost policy is checked before begin_request so a missing policy
        // cannot create a request record or consume a client idempotency key.
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
        let begin = self.store.begin_request(BeginRequestInput {
            user_id: principal.user_id.clone(),
            api_key_id: api_key_id.into(),
            protocol: "openai".into(),
            endpoint: CHAT_ENDPOINT.into(),
            model: model.into(),
            idempotency_key,
            body: body.clone(),
        })?;
        let handle = match begin {
            BeginRequest::Created(handle) => handle,
            BeginRequest::Existing(handle) => {
                return Ok(PreflightResult {
                    request_id: handle.id.clone(),
                    state: handle.state,
                    reservation: self.store.reservation_for_request(&handle.id)?,
                    execution: None,
                })
            }
            BeginRequest::Conflict => return Err(CoreError::IdempotencyConflict),
        };

        self.store
            .transition_request(&handle.id, RequestState::Received, RequestState::Validating, None)
            .map_err(|source| CoreError::RequestContext {
                request_id: handle.id.clone(),
                source: Box::new(source),
            })?;
        let reservation = self.store.reserve_request(QuotaReserve {
            user_id: principal.user_id.clone(),
            request_id: handle.id.clone(),
            resource_kind: estimate.resource_kind,
            amount: estimate.reserve_amount,
            ttl_ms: 15 * 60 * 1000,
        })?;
        let reservation = match reservation {
            ReserveResult::Created(reservation) => reservation,
            ReserveResult::Insufficient { available } => {
                return Err(CoreError::QuotaInsufficient {
                    available,
                    required: estimate.reserve_amount,
                });
            }
            ReserveResult::Existing(reservation) => {
                return Ok(PreflightResult {
                    request_id: handle.id,
                    state: RequestState::Reserved,
                    reservation: Some(reservation),
                    execution: None,
                });
            }
        };

        Ok(PreflightResult {
            request_id: handle.id.clone(),
            state: RequestState::Reserved,
            reservation: Some(reservation),
            execution: Some(ChatExecutionRequest {
                request_id: handle.id,
                endpoint: CHAT_ENDPOINT.into(),
                model: model.into(),
                body: body.clone(),
            }),
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
    use std::{collections::BTreeSet, fs, path::PathBuf, sync::Arc};

    use aiwork_core::{
        ChatExecutor, CoreError, CoreStore, CostPolicy, MockChatExecutor, NewUser, Principal,
        QuotaGrant, RequestState, UserRole, UpstreamError,
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

    fn bridge_with_grant(grant: i64) -> (CoreBridge, Principal) {
        bridge_with_grant_mode(grant, CoreMode::Enforce)
    }

    fn bridge_with_grant_mode(grant: i64, mode: CoreMode) -> (CoreBridge, Principal) {
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
        (CoreBridge::new(store, mode), principal)
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
        let replay = bridge
            .preflight_chat(
                &principal,
                &principal.key_id,
                Some("idem-insufficient"),
                &chat_body(),
            )
            .unwrap();
        assert!(replay.execution.is_none());
        assert_eq!(replay.state, RequestState::Failed);
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

        assert_eq!(first.reservation.as_ref().unwrap().id, second.reservation.as_ref().unwrap().id);
        assert!(second.execution.is_none());
        assert_eq!(second.state, RequestState::Reserved);
        assert_eq!(bridge.balance("u1", "chat_request").unwrap().held, 1);
    }

    #[test]
    fn timeout_and_disconnect_settle_as_unknown() {
        let (bridge, principal) = bridge_with_grant(1);
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
            let (bridge, principal) = bridge_with_grant_mode(1, mode);
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
        let (bridge, principal) = bridge_with_grant(1);
        let body = json!({"endpoint": "cheap", "model": "mock-1", "messages": []});
        let result = bridge
            .preflight_chat(&principal, &principal.key_id, Some("idem-endpoint"), &body)
            .unwrap();
        assert_eq!(result.execution.as_ref().unwrap().endpoint, "chat");
    }

    #[test]
    fn explicit_upstream_failure_releases_quota_and_settles_request() {
        let (bridge, principal) = bridge_with_grant(1);
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
        let (bridge, principal) = bridge_with_grant(2);
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
        let (bridge, principal) = bridge_with_grant(1);
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
