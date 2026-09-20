use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

use aiwork_core::{
    ChatExecutionRequest, ChatExecutor, CoreStore, BeginRequestInput, IssuedApiKey,
    KeyQuotaGrant, NewUser, PreflightReserveInput, PreflightReserveResult, Principal, QuotaGrant,
    QuotaBalance, RequestResult, RequestState, ReservationState, Settlement, UserRole,
    MockChatExecutor, UpstreamError,
};
use rusqlite::Connection;
use serde_json::json;

const USER_ID: &str = "phase1-user";
const RESOURCE_KIND: &str = "chat_request";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase1SmokeResult {
    pub request_id: String,
    pub reservation_id: String,
    pub final_status: RequestState,
    pub reservation_status: ReservationState,
    pub user_balance: QuotaBalance,
    pub mock_calls: usize,
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "aiwork-core-full-phase1-{}",
        rand::random::<u64>()
    ));
    fs::create_dir_all(&dir).expect("create isolated smoke directory");
    dir
}

fn scopes(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn user(id: &str, role: UserRole) -> NewUser {
    NewUser {
        id: id.to_owned(),
        name: format!("{id} name"),
        role,
    }
}

fn issue_user_key(store: &CoreStore, admin: &Principal) -> IssuedApiKey {
    store
        .issue_api_key_as_admin(
            USER_ID,
            "phase1 user key",
            scopes(&["chat:invoke"]),
            admin,
        )
        .expect("issue user API key")
}

fn chat_input(key_id: &str, idempotency_key: &str, amount: i64) -> PreflightReserveInput {
    PreflightReserveInput {
        request: BeginRequestInput {
            user_id: USER_ID.to_owned(),
            api_key_id: key_id.to_owned(),
            protocol: "openai".to_owned(),
            endpoint: "chat".to_owned(),
            model: "mock-1".to_owned(),
            idempotency_key: idempotency_key.to_owned(),
            body: json!({"model": "mock-1", "messages": []}),
        },
        resource_kind: RESOURCE_KIND.to_owned(),
        amount,
        ttl_ms: 60_000,
    }
}

fn execute_request(
    executor: &MockChatExecutor,
    request_id: &str,
    endpoint: &str,
    model: &str,
) -> Result<aiwork_core::ChatExecutionResult, UpstreamError> {
    executor.execute(ChatExecutionRequest {
        request_id: request_id.to_owned(),
        endpoint: endpoint.to_owned(),
        model: model.to_owned(),
        body: json!({"model": model, "messages": []}),
    })
}

fn assert_audit_and_ledger_are_bounded(path: &Path, balance: &QuotaBalance) {
    let connection = Connection::open(path.join("data").join("core.sqlite3"))
        .expect("open smoke database for audit assertions");
    let audit_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM audit_events WHERE actor_user_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count audit events");
    assert!(audit_events >= 5, "identity and grant actions must be audited");

    let request_ledger_events: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM quota_ledger WHERE request_id IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count request ledger events");
    assert_eq!(request_ledger_events, 3, "replay and insufficient requests must not add holds");

    assert!(balance.available >= 0);
    assert!(balance.held >= 0);
    assert!(balance.available + balance.held <= 2, "settlement exceeded the original grant");
}

/// Runs the Phase 0/1 closed loop with an in-memory Mock executor only.
///
/// The returned request is the timeout request: its request state is terminal
/// after settlement while its reservation remains `unknown` for reconciliation.
pub fn run_phase1_smoke() -> Phase1SmokeResult {
    let dir = temp_dir();
    let store = CoreStore::open(&dir).expect("open isolated CoreStore");
    store.migrate().expect("migrate isolated CoreStore");

    let admin = store
        .create_bootstrap_admin(user("phase1-admin", UserRole::Admin), "bootstrap")
        .expect("create bootstrap admin");
    let admin_key = store
        .issue_api_key(
            &admin.id,
            "phase1 admin key",
            scopes(&["admin", "chat:invoke"]),
            "bootstrap",
        )
        .expect("issue bootstrap admin key");
    let admin_principal = store
        .authenticate_api_key(&admin_key.plaintext)
        .expect("authenticate admin key from returned one-time value");

    store
        .create_user_as_admin(user(USER_ID, UserRole::User), &admin_principal)
        .expect("create phase1 user");
    let user_key = issue_user_key(&store, &admin_principal);
    let user_principal = store
        .authenticate_api_key(&user_key.plaintext)
        .expect("authenticate user key from returned one-time value");

    let granted = store
        .grant_as_admin(
            QuotaGrant {
                user_id: USER_ID.to_owned(),
                resource_kind: RESOURCE_KIND.to_owned(),
                amount: 2,
                actor_user_id: "ignored-by-principal".to_owned(),
                reason: "phase1 smoke grant".to_owned(),
            },
            &admin_principal,
        )
        .expect("grant two logical chat units");
    assert_eq!(granted.available, 2);
    store
        .key_quota_grant_as_admin(
            &admin_principal,
            KeyQuotaGrant {
                api_key_id: user_key.id.clone(),
                resource_kind: RESOURCE_KIND.to_owned(),
                amount: 2,
                actor_user_id: "ignored-by-principal".to_owned(),
                reason: "phase1 key grant".to_owned(),
            },
        )
        .expect("grant two key-scoped chat units");

    let success_input = chat_input(&user_key.id, "phase1-success", 1);
    let (success_request, success_reservation) = match store
        .preflight_reserve(success_input.clone())
        .expect("reserve successful chat request")
    {
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
        other => panic!("expected a new successful request, got {other:?}"),
    };
    let success_executor = MockChatExecutor::ok();
    let success_response = execute_request(
        &success_executor,
        &success_request.id,
        &success_request.endpoint,
        &success_request.model,
    )
    .expect("Mock executor success");
    store
        .settle_request(
            &user_principal,
            &success_reservation.id,
            Settlement::Commit {
                actual_amount: success_response.actual_amount,
            },
            RequestState::Succeeded,
            Some(RequestResult {
                status: Some(success_response.status as i64),
                error_code: None,
            }),
        )
        .expect("commit successful chat request");

    match store
        .preflight_reserve(success_input)
        .expect("replay successful idempotency key")
    {
        PreflightReserveResult::Existing {
            request,
            reservation: Some(reservation),
        } => {
            assert_eq!(request.id, success_request.id);
            assert_eq!(request.state, RequestState::Settled);
            assert_eq!(reservation.state, ReservationState::Committed);
        }
        other => panic!("expected terminal idempotent replay, got {other:?}"),
    }
    assert_eq!(success_executor.calls().len(), 1, "replay must not call Mock twice");

    let insufficient = store
        .preflight_reserve(chat_input(&user_key.id, "phase1-insufficient", 2))
        .expect("check insufficient budget before execution");
    assert!(matches!(
        insufficient,
        PreflightReserveResult::Insufficient {
            available: 1,
            required: 2,
        }
    ));
    assert_eq!(success_executor.calls().len(), 1);

    let timeout_input = chat_input(&user_key.id, "phase1-timeout", 1);
    let (timeout_request, timeout_reservation) = match store
        .preflight_reserve(timeout_input.clone())
        .expect("reserve timeout chat request")
    {
        PreflightReserveResult::Created { request, reservation } => (request, reservation),
        other => panic!("expected a new timeout request, got {other:?}"),
    };
    let timeout_executor = MockChatExecutor::timeout();
    assert!(matches!(
        execute_request(
            &timeout_executor,
            &timeout_request.id,
            &timeout_request.endpoint,
            &timeout_request.model,
        ),
        Err(UpstreamError::Timeout)
    ));
    store
        .settle_request(
            &user_principal,
            &timeout_reservation.id,
            Settlement::Unknown,
            RequestState::Unknown,
            Some(RequestResult {
                status: None,
                error_code: Some("upstream_uncertain".to_owned()),
            }),
        )
        .expect("mark timeout as unknown");
    assert_eq!(
        store
            .reservation_for_request(&timeout_request.id)
            .expect("read timeout reservation")
            .expect("timeout reservation exists")
            .state,
        ReservationState::Unknown
    );

    let mock_calls = success_executor.calls().len() + timeout_executor.calls().len();
    assert_eq!(mock_calls, 2, "success and timeout are the only Mock upstream calls");

    drop(store);
    let restarted = CoreStore::open(&dir).expect("reopen CoreStore after simulated restart");
    restarted.migrate().expect("re-migrate after simulated restart");
    assert_eq!(
        restarted.request_state(&timeout_request.id).expect("read timeout request after restart"),
        RequestState::Settled
    );
    let recovered = match restarted
        .preflight_reserve(timeout_input)
        .expect("replay timeout idempotency key after restart")
    {
        PreflightReserveResult::Existing {
            request,
            reservation: Some(reservation),
        } => {
            assert_eq!(request.state, RequestState::Settled);
            assert_eq!(reservation.state, ReservationState::Unknown);
            reservation
        }
        other => panic!("expected unknown reservation after restart, got {other:?}"),
    };
    assert_eq!(recovered.id, timeout_reservation.id);

    let key_balance = restarted
        .key_quota_balance_as_admin(&admin_principal, &user_key.id, RESOURCE_KIND)
        .expect("read bounded key balance");
    let balance = QuotaBalance {
        user_id: key_balance.user_id,
        resource_kind: key_balance.resource_kind,
        available: key_balance.available,
        held: key_balance.held,
    };
    assert_eq!(balance.available, 0);
    assert_eq!(balance.held, 1);
    assert_audit_and_ledger_are_bounded(&dir, &balance);
    let final_status = restarted
        .request_state(&timeout_request.id)
        .expect("read final request status");

    let result = Phase1SmokeResult {
        request_id: timeout_request.id,
        reservation_id: timeout_reservation.id,
        final_status,
        reservation_status: recovered.state,
        user_balance: balance,
        mock_calls,
    };
    drop(restarted);
    fs::remove_dir_all(&dir).expect("remove isolated smoke directory");
    result
}

#[test]
fn phase1_smoke_is_a_mock_only_budget_safe_restartable_flow() {
    let result = run_phase1_smoke();

    assert_eq!(result.final_status, RequestState::Settled);
    assert_eq!(result.reservation_status, ReservationState::Unknown);
    assert_eq!(result.user_balance.available, 0);
    assert_eq!(result.user_balance.held, 1);
    assert_eq!(result.mock_calls, 2);
    assert!(!result.request_id.is_empty());
    assert!(!result.reservation_id.is_empty());
}
