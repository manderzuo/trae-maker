//! Lease-aware upstream dispatch for the non-stream Chat route.
//!
//! Core chooses the account and hands this module an opaque lease grant. An
//! adapter may use the credential locator in that grant, but it never chooses
//! an account and it never falls back to an `ApiPool`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use aiwork_core::{
    ChatExecutionRequest, ChatExecutionResult, ChatExecutor, LeaseOutcome, UpstreamError,
    UpstreamLeaseGrant,
};
use serde_json::{json, Value};

use crate::api_server::{payload, routes, sse, wb_payload, wb_sse, wb_upstream};
use crate::api_server::pool::{ApiPool, PickedAccount};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamOutcome {
    Success {
        body: Value,
        actual_units: Option<i64>,
        upstream_request_ref: Option<String>,
    },
    Rejected {
        status: u16,
        code: String,
        accepted: bool,
    },
    TransportUnknown {
        reason: String,
        upstream_request_ref: Option<String>,
    },
}

impl UpstreamOutcome {
    pub fn lease_outcome(&self, now_ms: i64) -> LeaseOutcome {
        match self {
            Self::Success {
                actual_units,
                upstream_request_ref,
                ..
            } => LeaseOutcome::Success {
                actual_units: *actual_units,
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            },
            Self::Rejected {
                status,
                code,
                accepted,
            } => LeaseOutcome::Rejected {
                status: i64::from(*status),
                code: Some(code.clone()),
                accepted: *accepted,
                now_ms,
            },
            Self::TransportUnknown {
                reason,
                upstream_request_ref,
            } => LeaseOutcome::TransportUnknown {
                reason: reason.clone(),
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            },
        }
    }

    pub fn response(&self) -> Option<ChatExecutionResult> {
        match self {
            Self::Success {
                body,
                actual_units,
                ..
            } => Some(ChatExecutionResult {
                status: 200,
                body: body.clone(),
                actual_amount: *actual_units,
            }),
            Self::Rejected { status, code, .. } => Some(ChatExecutionResult {
                status: *status,
                body: json!({
                    "error": {
                        "type": "upstream_error",
                        "code": code,
                    }
                }),
                actual_amount: None,
            }),
            Self::TransportUnknown { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUsage {
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub data: Value,
    pub usage: Option<StreamUsage>,
    pub upstream_request_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelSupport {
    Confirmed,
    Unsupported,
    Unknown,
}

pub trait StreamSink {
    /// Returns false when the client channel has closed. A false return is
    /// never a successful upstream terminal outcome.
    fn emit(&mut self, event: StreamEvent) -> bool;

    fn cancel_requested(&self) -> bool;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamTerminalOutcome {
    Success {
        actual_units: Option<i64>,
        upstream_request_ref: Option<String>,
    },
    Rejected {
        status: u16,
        code: String,
        accepted: bool,
    },
    Canceled {
        upstream_request_ref: Option<String>,
    },
    TransportUnknown {
        reason: String,
        upstream_request_ref: Option<String>,
    },
}

impl StreamTerminalOutcome {
    pub fn lease_outcome(&self, now_ms: i64) -> LeaseOutcome {
        match self {
            Self::Success {
                actual_units,
                upstream_request_ref,
            } => LeaseOutcome::Success {
                actual_units: *actual_units,
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            },
            Self::Rejected {
                status,
                code,
                accepted,
            } => LeaseOutcome::Rejected {
                status: i64::from(*status),
                code: Some(code.clone()),
                accepted: *accepted,
                now_ms,
            },
            Self::Canceled {
                upstream_request_ref,
            } => LeaseOutcome::Canceled {
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            },
            Self::TransportUnknown {
                reason,
                upstream_request_ref,
            } => LeaseOutcome::TransportUnknown {
                reason: reason.clone(),
                upstream_request_ref: upstream_request_ref.clone(),
                now_ms,
            },
        }
    }
}

pub trait LeaseStreamAdapter: Send + Sync {
    fn execute_stream(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
        sink: &mut dyn StreamSink,
    ) -> StreamTerminalOutcome;

    fn cancel_stream(&self, _lease: &UpstreamLeaseGrant) -> CancelSupport {
        CancelSupport::Unsupported
    }
}

/// The only adapter boundary that can receive an upstream lease.
pub trait LeaseUpstreamAdapter: Send + Sync {
    fn execute_nonstream_chat(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome;
}

#[derive(Clone)]
enum RegisteredAdapter {
    Lease(Arc<dyn LeaseUpstreamAdapter>),
    Chat(Arc<dyn ChatExecutor>),
}

/// Provider-keyed adapter registry used after Core has already selected an
/// account. Every dispatch also requires an exact account-to-provider and
/// account-to-credentials binding. The Core lease is authoritative; a single
/// provider is never treated as a wildcard for an unbound account.
#[derive(Clone, Default)]
pub struct CoreUpstreamExecutor {
    adapters: BTreeMap<String, RegisteredAdapter>,
    stream_adapters: BTreeMap<String, Arc<dyn LeaseStreamAdapter>>,
    account_bindings: BTreeMap<String, AccountBinding>,
}

#[derive(Clone)]
struct AccountBinding {
    account_ref: String,
    provider: String,
    credentials_ref: String,
}

impl CoreUpstreamExecutor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_provider(
        mut self,
        provider: impl Into<String>,
        adapter: Arc<dyn LeaseUpstreamAdapter>,
    ) -> Self {
        self.adapters
            .insert(provider.into(), RegisteredAdapter::Lease(adapter));
        self
    }

    pub fn with_stream_provider(
        mut self,
        provider: impl Into<String>,
        adapter: Arc<dyn LeaseStreamAdapter>,
    ) -> Self {
        self.stream_adapters.insert(provider.into(), adapter);
        self
    }

    /// Bind one Core-selected account to one provider adapter and the exact
    /// opaque credential locator selected by Core.
    pub fn for_account(
        mut self,
        account_ref: impl Into<String>,
        provider: impl Into<String>,
        credentials_ref: impl Into<String>,
    ) -> Self {
        let account_ref = account_ref.into();
        self.account_bindings.insert(
            account_ref.clone(),
            AccountBinding {
                account_ref,
                provider: provider.into(),
                credentials_ref: credentials_ref.into(),
            },
        );
        self
    }

    pub fn with_account(
        self,
        account_ref: impl Into<String>,
        provider: impl Into<String>,
        credentials_ref: impl Into<String>,
    ) -> Self {
        self.for_account(account_ref, provider, credentials_ref)
    }

    pub fn from_adapter(adapter: Arc<dyn LeaseUpstreamAdapter>) -> Self {
        Self::new().with_provider("single", adapter)
    }

    pub fn from_adapter_for_account(
        adapter: Arc<dyn LeaseUpstreamAdapter>,
        account_ref: impl Into<String>,
        credentials_ref: impl Into<String>,
    ) -> Self {
        Self::from_adapter(adapter).for_account(account_ref, "single", credentials_ref)
    }

    pub fn from_chat_executors(
        executors: &BTreeMap<String, Arc<dyn ChatExecutor>>,
    ) -> Self {
        let mut result = Self::new();
        for (provider, executor) in executors {
            result.adapters.insert(
                provider.clone(),
                RegisteredAdapter::Chat(executor.clone()),
            );
        }
        result
    }

    pub fn from_chat_executors_with_accounts<I>(
        executors: &BTreeMap<String, Arc<dyn ChatExecutor>>,
        accounts: I,
    ) -> Self
    where
        I: IntoIterator<Item = (String, String, String)>,
    {
        let mut result = Self::from_chat_executors(executors);
        for (account_ref, provider, credentials_ref) in accounts {
            result = result.for_account(account_ref, provider, credentials_ref);
        }
        result
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty() && self.stream_adapters.is_empty()
    }

    pub fn can_dispatch_without_provider_binding(&self) -> bool {
        !self.account_bindings.is_empty()
            && self
                .account_bindings
                .values()
                .all(|binding| self.adapters.contains_key(&binding.provider))
    }

    /// Account refs that are safe to pass to Core's scheduler constraint.
    /// Returning an empty list is intentionally distinct from a wildcard.
    pub fn bound_account_refs(&self) -> Vec<String> {
        self.account_bindings.keys().cloned().collect()
    }

    pub fn can_dispatch_stream(&self) -> bool {
        !self.account_bindings.is_empty()
            && self
                .account_bindings
                .values()
                .all(|binding| self.stream_adapters.contains_key(&binding.provider))
    }

    pub fn execute_nonstream_chat(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome {
        let Some(binding) = self.account_bindings.get(&lease.account_ref) else {
            return UpstreamOutcome::TransportUnknown {
                reason: "account_binding_missing".into(),
                upstream_request_ref: None,
            };
        };
        if binding.credentials_ref != lease.credentials_ref {
            return UpstreamOutcome::TransportUnknown {
                reason: "lease_credentials_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        let Some(adapter) = self.adapters.get(&binding.provider) else {
            return UpstreamOutcome::TransportUnknown {
                reason: "adapter_unavailable".into(),
                upstream_request_ref: None,
            };
        };
        match adapter {
            RegisteredAdapter::Lease(adapter) => adapter.execute_nonstream_chat(lease, request),
            RegisteredAdapter::Chat(executor) => ChatExecutorAdapter {
                executor: executor.clone(),
                account_ref: binding.account_ref.clone(),
                credentials_ref: binding.credentials_ref.clone(),
            }
            .execute_nonstream_chat(lease, request),
        }
    }

    pub fn execute_stream(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
        sink: &mut dyn StreamSink,
    ) -> StreamTerminalOutcome {
        let Some(binding) = self.account_bindings.get(&lease.account_ref) else {
            return StreamTerminalOutcome::TransportUnknown {
                reason: "account_binding_missing".into(),
                upstream_request_ref: None,
            };
        };
        if binding.credentials_ref != lease.credentials_ref {
            return StreamTerminalOutcome::TransportUnknown {
                reason: "lease_credentials_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        let Some(adapter) = self.stream_adapters.get(&binding.provider) else {
            return StreamTerminalOutcome::TransportUnknown {
                reason: "stream_adapter_unavailable".into(),
                upstream_request_ref: None,
            };
        };
        adapter.execute_stream(lease, request, sink)
    }

    /// Ask the explicitly bound provider adapter to cancel the exact leased
    /// upstream request. Binding or credential mismatches are deliberately
    /// reported as unknown rather than treated as unsupported cancellation.
    pub fn cancel_stream(&self, lease: &UpstreamLeaseGrant) -> CancelSupport {
        let Some(binding) = self.account_bindings.get(&lease.account_ref) else {
            return CancelSupport::Unknown;
        };
        if binding.credentials_ref != lease.credentials_ref {
            return CancelSupport::Unknown;
        }
        let Some(adapter) = self.stream_adapters.get(&binding.provider) else {
            return CancelSupport::Unknown;
        };
        adapter.cancel_stream(lease)
    }
}

struct ChatExecutorAdapter {
    executor: Arc<dyn ChatExecutor>,
    account_ref: String,
    credentials_ref: String,
}

impl LeaseUpstreamAdapter for ChatExecutorAdapter {
    fn execute_nonstream_chat(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome {
        if lease.account_ref != self.account_ref || lease.credentials_ref != self.credentials_ref {
            return UpstreamOutcome::TransportUnknown {
                reason: "lease_binding_mismatch".into(),
                upstream_request_ref: None,
            };
        }
        match self.executor.execute(request) {
            Ok(result) if (200..300).contains(&result.status) => UpstreamOutcome::Success {
                body: result.body,
                actual_units: result.actual_amount,
                upstream_request_ref: None,
            },
            Ok(result) => UpstreamOutcome::Rejected {
                status: result.status,
                code: "upstream_rejected".into(),
                accepted: false,
            },
            Err(UpstreamError::Rejected { status, code }) => UpstreamOutcome::Rejected {
                status,
                code: code.unwrap_or_else(|| "upstream_rejected".into()),
                accepted: false,
            },
            Err(UpstreamError::Timeout) => UpstreamOutcome::TransportUnknown {
                reason: "transport_timeout".into(),
                upstream_request_ref: None,
            },
            Err(UpstreamError::Disconnected) => UpstreamOutcome::TransportUnknown {
                reason: "transport_unknown".into(),
                upstream_request_ref: None,
            },
            Err(UpstreamError::Failed { .. }) => UpstreamOutcome::TransportUnknown {
                reason: "upstream_execution_unknown".into(),
                upstream_request_ref: None,
            },
        }
    }
}

/// The production bridge for the legacy provider transports. Core has already
/// selected the account; this adapter only resolves that exact account and
/// never calls a pool-wide picker or rotates to another account.
pub trait LegacyChatTransport: Send + Sync {
    fn execute(
        &self,
        provider: &str,
        account: &PickedAccount,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome;
}

#[derive(Clone)]
pub struct LegacyPoolLeaseAdapter {
    provider: String,
    account_uids: BTreeMap<String, String>,
    resolve_account: Arc<dyn Fn(&str) -> Option<PickedAccount> + Send + Sync>,
    transport: Arc<dyn LegacyChatTransport>,
}

impl LegacyPoolLeaseAdapter {
    pub fn with_transport(
        provider: impl Into<String>,
        account_uids: BTreeMap<String, String>,
        resolve_account: Arc<dyn Fn(&str) -> Option<PickedAccount> + Send + Sync>,
        transport: Arc<dyn LegacyChatTransport>,
    ) -> Self {
        Self {
            provider: provider.into(),
            account_uids,
            resolve_account,
            transport,
        }
    }

    pub fn from_pool(
        provider: impl Into<String>,
        pool: ApiPool,
        account_uids: BTreeMap<String, String>,
    ) -> Self {
        let resolve_account = Arc::new(move |uid: &str| pool.pick_by_uid(uid));
        Self::with_transport(
            provider,
            account_uids,
            resolve_account,
            Arc::new(LegacyNetworkChatTransport),
        )
    }
}

impl LeaseUpstreamAdapter for LegacyPoolLeaseAdapter {
    fn execute_nonstream_chat(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome {
        let Some(uid) = self.account_uids.get(&lease.account_ref) else {
            return UpstreamOutcome::TransportUnknown {
                reason: "account_binding_missing".into(),
                upstream_request_ref: None,
            };
        };
        let Some(account) = (self.resolve_account)(uid) else {
            return UpstreamOutcome::TransportUnknown {
                reason: "account_unavailable".into(),
                upstream_request_ref: None,
            };
        };
        self.transport.execute(&self.provider, &account, request)
    }
}

struct LegacyNetworkChatTransport;

impl LegacyChatTransport for LegacyNetworkChatTransport {
    fn execute(
        &self,
        provider: &str,
        account: &PickedAccount,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome {
        let body = match serde_json::to_vec(&request.body) {
            Ok(body) => body,
            Err(_) => {
                return UpstreamOutcome::Rejected {
                    status: 400,
                    code: "invalid_request".into(),
                    accepted: false,
                }
            }
        };
        match provider {
            "trae" => execute_trae_chat(account, &request, &body),
            "workbuddy" => execute_workbuddy_chat(account, &request, &body),
            _ => UpstreamOutcome::TransportUnknown {
                reason: "adapter_unavailable".into(),
                upstream_request_ref: None,
            },
        }
    }
}

fn execute_trae_chat(
    account: &PickedAccount,
    request: &ChatExecutionRequest,
    body: &[u8],
) -> UpstreamOutcome {
    let converted = payload::prepare_llm_chat_body(
        body,
        &request.model,
        &account.uid,
        &account.device_id,
        &account.machine_id,
    );
    let reader = match routes::make_upstream_request(
        &account.jwt,
        &account.uid,
        &account.device_id,
        &account.machine_id,
        &converted,
    ) {
        Ok(reader) => reader,
        Err((status, _, _)) if status == 502 => {
            return UpstreamOutcome::TransportUnknown {
                reason: "transport_unknown".into(),
                upstream_request_ref: None,
            }
        }
        Err((status, _, _)) => {
            return UpstreamOutcome::Rejected {
                status,
                code: rejected_code(status).into(),
                accepted: false,
            }
        }
    };
    let chat_id = format!("chatcmpl-{}", request.request_id);
    let (response, error) = sse::aggregate(reader, &chat_id);
    match (response, error) {
        (Some(body), None) => UpstreamOutcome::Success {
            body,
            actual_units: None,
            upstream_request_ref: None,
        },
        _ => UpstreamOutcome::TransportUnknown {
            reason: "upstream_sse_unknown".into(),
            upstream_request_ref: None,
        },
    }
}

fn execute_workbuddy_chat(
    account: &PickedAccount,
    request: &ChatExecutionRequest,
    body: &[u8],
) -> UpstreamOutcome {
    let converted = wb_payload::prepare_wb_chat_body(
        body,
        &request.model,
        &request.request_id,
        None,
        true,
        &wb_payload::default_template_map(),
    );
    let reader = match wb_upstream::make_wb_request(
        &wb_upstream::WbCreds {
            id: account.uid.clone(),
            uid: account.uid.clone(),
            name: String::new(),
            token: account.jwt.clone(),
            domain: account.domain.clone(),
            enterprise_id: account.enterprise_id.clone(),
            global_region: account.global_region,
        },
        &converted,
    ) {
        Ok(reader) => reader,
        Err((status, _, _)) if status == 502 => {
            return UpstreamOutcome::TransportUnknown {
                reason: "transport_unknown".into(),
                upstream_request_ref: None,
            }
        }
        Err((status, _, _)) => {
            return UpstreamOutcome::Rejected {
                status,
                code: rejected_code(status).into(),
                accepted: false,
            }
        }
    };
    let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
        Ok(lines) => lines,
        Err(()) => {
            return UpstreamOutcome::TransportUnknown {
                reason: "transport_timeout".into(),
                upstream_request_ref: None,
            }
        }
    };
    let (response, _) = wb_sse::aggregate(lines, &format!("chatcmpl-{}", request.request_id));
    match response {
        Some(body) => UpstreamOutcome::Success {
            body,
            actual_units: None,
            upstream_request_ref: None,
        },
        None => UpstreamOutcome::TransportUnknown {
            reason: "upstream_sse_unknown".into(),
            upstream_request_ref: None,
        },
    }
}

fn rejected_code(status: u16) -> &'static str {
    match status {
        401 | 403 => "authentication_error",
        408 | 409 | 429 => "rate_limited",
        400..=499 => "invalid_request",
        _ => "upstream_rejected",
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockUpstreamCall {
    pub lease_id: String,
    pub account_ref: String,
    pub credentials_ref: String,
    pub request_id: String,
    pub body: Value,
}

#[cfg(test)]
#[derive(Clone)]
pub struct MockUpstreamExecutor {
    calls: Arc<Mutex<Vec<MockUpstreamCall>>>,
    outcome: UpstreamOutcome,
}

#[cfg(test)]
impl MockUpstreamExecutor {
    pub fn ok() -> Self {
        Self::with_outcome(UpstreamOutcome::Success {
            body: json!({"choices": []}),
            actual_units: Some(1),
            upstream_request_ref: None,
        })
    }

    pub fn with_outcome(outcome: UpstreamOutcome) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            outcome,
        }
    }

    pub fn calls(&self) -> Vec<MockUpstreamCall> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

#[cfg(test)]
impl LeaseUpstreamAdapter for MockUpstreamExecutor {
    fn execute_nonstream_chat(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
    ) -> UpstreamOutcome {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(MockUpstreamCall {
                lease_id: lease.lease_id.clone(),
                account_ref: lease.account_ref.clone(),
                credentials_ref: lease.credentials_ref.clone(),
                request_id: request.request_id,
                body: request.body,
            });
        self.outcome.clone()
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockStreamCall {
    pub lease_id: String,
    pub account_ref: String,
    pub request_id: String,
}

#[cfg(test)]
#[derive(Clone)]
pub struct MockStreamAdapter {
    calls: Arc<Mutex<Vec<MockStreamCall>>>,
    outcome: StreamTerminalOutcome,
    usage: Option<StreamUsage>,
    cancel_support: CancelSupport,
}

#[cfg(test)]
impl MockStreamAdapter {
    pub fn success_without_usage() -> Self {
        Self::with_outcome(StreamTerminalOutcome::Success {
            actual_units: None,
            upstream_request_ref: None,
        })
    }

    pub fn with_outcome(outcome: StreamTerminalOutcome) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            outcome,
            usage: None,
            cancel_support: CancelSupport::Unsupported,
        }
    }

    pub fn with_usage(mut self, usage: StreamUsage) -> Self {
        self.usage = Some(usage);
        self
    }

    pub fn with_cancel_support(mut self, cancel_support: CancelSupport) -> Self {
        self.cancel_support = cancel_support;
        self
    }

    pub fn calls(&self) -> Vec<MockStreamCall> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn upstream_request_ref(&self) -> Option<String> {
        match &self.outcome {
            StreamTerminalOutcome::Success {
                upstream_request_ref,
                ..
            }
            | StreamTerminalOutcome::Canceled {
                upstream_request_ref,
            }
            | StreamTerminalOutcome::TransportUnknown {
                upstream_request_ref,
                ..
            } => upstream_request_ref.clone(),
            StreamTerminalOutcome::Rejected { .. } => None,
        }
    }
}

#[cfg(test)]
impl LeaseStreamAdapter for MockStreamAdapter {
    fn execute_stream(
        &self,
        lease: &UpstreamLeaseGrant,
        request: ChatExecutionRequest,
        sink: &mut dyn StreamSink,
    ) -> StreamTerminalOutcome {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(MockStreamCall {
                lease_id: lease.lease_id.clone(),
                account_ref: lease.account_ref.clone(),
                request_id: request.request_id,
            });

        if !sink.emit(StreamEvent {
            data: json!({"type": "mock_text", "content": "hello"}),
            usage: None,
            upstream_request_ref: None,
        }) {
            return StreamTerminalOutcome::TransportUnknown {
                reason: "stream_sink_closed".into(),
                upstream_request_ref: self.upstream_request_ref(),
            };
        }
        if let Some(usage) = self.usage.clone() {
            if !sink.emit(StreamEvent {
                data: json!({"type": "mock_usage"}),
                usage: Some(usage),
                upstream_request_ref: None,
            }) {
                return StreamTerminalOutcome::TransportUnknown {
                    reason: "stream_sink_closed".into(),
                    upstream_request_ref: self.upstream_request_ref(),
                };
            }
        }
        if sink.cancel_requested() {
            return match self.cancel_support {
                CancelSupport::Confirmed => StreamTerminalOutcome::Canceled {
                    upstream_request_ref: self.upstream_request_ref(),
                },
                CancelSupport::Unsupported | CancelSupport::Unknown => {
                    StreamTerminalOutcome::TransportUnknown {
                        reason: "stream_cancel_unconfirmed".into(),
                        upstream_request_ref: self.upstream_request_ref(),
                    }
                }
            };
        }
        self.outcome.clone()
    }

    fn cancel_stream(&self, _lease: &UpstreamLeaseGrant) -> CancelSupport {
        self.cancel_support
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phase2MockChatReport {
    pub request_id: String,
    pub reservation_id: String,
    pub lease_id: String,
    pub account_ref: String,
    pub request_state: aiwork_core::RequestState,
    pub result: Option<aiwork_core::RequestResult>,
    pub reservation_state: aiwork_core::ReservationState,
    pub lease_state: aiwork_core::LeaseState,
    pub replay_request_id: String,
    pub replay_request_state: aiwork_core::RequestState,
    pub replay_result: Option<aiwork_core::RequestResult>,
    pub balance_available: i64,
    pub balance_held: i64,
    pub mock_calls: usize,
}

#[cfg(test)]
pub fn run_phase2_mock_chat(outcome: UpstreamOutcome) -> Phase2MockChatReport {
    use std::{collections::BTreeSet, fs, path::{Path, PathBuf}};

    use aiwork_core::{
        BeginRequestInput, CoreStore, CostPolicy, NewUser, PreflightReserveInput, Principal,
        QuotaGrant, RegisterUpstreamAccount, SchedulerLeaseRequest, SchedulerLeaseResult,
        SelectionStrategy, UserRole, UpstreamObservation,
    };
    use chrono::Utc;

    struct FixtureDir(PathBuf);

    impl FixtureDir {
        fn new() -> Self {
            let root = PathBuf::from(r"D:\gpt");
            fs::create_dir_all(&root).unwrap();
            let path = root.join(format!(
                "aiwork-task5-phase2-{}-{}",
                std::process::id(),
                rand::random::<u64>()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for FixtureDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    let dir = FixtureDir::new();
    let store = Arc::new(CoreStore::open(dir.path()).unwrap());
    store.migrate().unwrap();
    store
        .create_bootstrap_admin(
            NewUser {
                id: "phase2-admin".into(),
                name: "Phase 2 admin".into(),
                role: UserRole::Admin,
            },
            "bootstrap",
        )
        .unwrap();
    let admin_key = store
        .issue_api_key("phase2-admin", "phase2-admin", BTreeSet::new(), "bootstrap")
        .unwrap();
    let admin = Principal {
        user_id: "phase2-admin".into(),
        key_id: admin_key.id,
        scopes: BTreeSet::new(),
    };
    store
        .create_user(
            NewUser {
                id: "phase2-user".into(),
                name: "Phase 2 user".into(),
                role: UserRole::User,
            },
            "bootstrap",
        )
        .unwrap();
    let user_key = store
        .issue_api_key(
            "phase2-user",
            "phase2-user",
            BTreeSet::from(["chat:invoke".to_owned()]),
            "bootstrap",
        )
        .unwrap();
    let principal = Principal {
        user_id: "phase2-user".into(),
        key_id: user_key.id.clone(),
        scopes: user_key.scopes.clone(),
    };
    store
        .upsert_cost_policy(CostPolicy {
            id: "phase2-chat-policy".into(),
            endpoint: "chat".into(),
            model_pattern: "mock-*".into(),
            resource_kind: "chat_request".into(),
            reserve_amount: 1,
            max_actual_amount: Some(1),
            version: 1,
            enabled: true,
        })
        .unwrap();
    store
        .grant(QuotaGrant {
            user_id: "phase2-user".into(),
            resource_kind: "chat_request".into(),
            amount: 1,
            actor_user_id: "phase2-user".into(),
            reason: "phase2 fixture".into(),
        })
        .unwrap();
    let mut account = RegisterUpstreamAccount::new(
        "mock-account".into(),
        "mock".into(),
        "vault://mock/account".into(),
    );
    account.capabilities.insert("chat".into());
    store.upsert_upstream_account(account, &admin).unwrap();
    let now = Utc::now().timestamp_millis();
    store
        .append_upstream_observation(UpstreamObservation::new(
            "mock-observation".into(),
            "mock-account".into(),
            "chat_request".into(),
            Some(100),
            1,
            "reader".into(),
            aiwork_core::ObservationStatus::Fresh,
            now,
            now + 600_000,
            json!({"available": 100}),
        ))
        .unwrap();

    let body = json!({"model": "mock-1", "messages": []});
    let input = SchedulerLeaseRequest {
        preflight: PreflightReserveInput {
            request: BeginRequestInput {
                user_id: principal.user_id.clone(),
                api_key_id: principal.key_id.clone(),
                protocol: "openai".into(),
                endpoint: "chat".into(),
                model: "mock-1".into(),
                idempotency_key: "phase2-helper".into(),
                body: body.clone(),
            },
            resource_kind: "chat_request".into(),
            amount: 1,
            ttl_ms: 900_000,
        },
        provider_hint: Some("mock".into()),
        required_capabilities: vec!["chat".into()],
        region: None,
        predicted_units: 1,
        safety_margin_units: 0,
        observation_max_age_ms: 600_000,
        allowed_accounts: None,
        dedicated_account: None,
        selection_strategy: SelectionStrategy::HighestNormalizedAvailable,
        now_ms: now,
        lease_ttl_ms: 900_000,
        reconcile_ttl_ms: 600_000,
    };
    let grant = match store
        .preflight_reserve_with_lease(&principal, input.clone())
        .unwrap()
    {
        SchedulerLeaseResult::Acquired(grant) => grant,
        SchedulerLeaseResult::Replay { .. } => panic!("phase2 helper unexpectedly replayed"),
    };
    let (request, reservation) = match store.preflight_reserve(input.preflight.clone()).unwrap() {
        aiwork_core::PreflightReserveResult::Existing {
            request,
            reservation: Some(reservation),
        } => (request, reservation),
        other => panic!("phase2 helper did not recover acquired request: {other:?}"),
    };
    let executor = MockUpstreamExecutor::with_outcome(outcome);
    let outcome = executor.execute_nonstream_chat(
        &grant,
        ChatExecutionRequest {
            request_id: request.id.clone(),
            endpoint: "chat".into(),
            model: "mock-1".into(),
            body,
        },
    );
    let settlement = outcome.lease_outcome(now + 1);
    let settled = store
        .settle_upstream_lease(&principal, &grant.lease_id, settlement.clone())
        .unwrap();
    let repeated = store
        .settle_upstream_lease(&principal, &grant.lease_id, settlement)
        .unwrap();
    assert_eq!(settled, repeated);

    let settled_request = match store.preflight_reserve(input.preflight.clone()).unwrap() {
        aiwork_core::PreflightReserveResult::Existing { request, .. } => request,
        other => panic!("phase2 helper did not reread settled request: {other:?}"),
    };

    let mut conflict_input = input.clone();
    conflict_input.preflight.request.body = json!({
        "model": "mock-1",
        "messages": [{"role": "user", "content": "different body"}]
    });
    assert!(matches!(
        store.preflight_reserve_with_lease(&principal, conflict_input),
        Err(aiwork_core::ScheduleError::IdempotencyConflict)
    ));

    let mut replay_input = input.clone();
    replay_input.now_ms = now + 2;
    let replay = store
        .preflight_reserve_with_lease(&principal, replay_input)
        .unwrap();
    let (replay_request, replay_lease) = match replay {
        SchedulerLeaseResult::Replay { request, lease } => (request, lease),
        SchedulerLeaseResult::Acquired(_) => panic!("phase2 helper replay acquired a second lease"),
    };
    let report = Phase2MockChatReport {
        request_id: settled_request.id.clone(),
        reservation_id: reservation.id,
        lease_id: grant.lease_id,
        account_ref: grant.account_ref,
        request_state: store.request_state(&settled_request.id).unwrap(),
        result: settled_request.result.clone(),
        reservation_state: store
            .reservation_for_request(&settled_request.id)
            .unwrap()
            .unwrap()
            .state,
        lease_state: replay_lease.state,
        replay_request_id: replay_request.id.clone(),
        replay_request_state: replay_request.state,
        replay_result: replay_request.result,
        balance_available: store.balance("phase2-user", "chat_request").unwrap().available,
        balance_held: store.balance("phase2-user", "chat_request").unwrap().held,
        mock_calls: executor.calls().len(),
    };
    assert_eq!(report.request_state, report.replay_request_state);
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use aiwork_core::{LeaseState, ReservationState, RequestState};

    #[test]
    fn core_executor_phase2_mock_success_and_replay() {
        let report = run_phase2_mock_chat(UpstreamOutcome::Success {
            body: serde_json::json!({"choices": []}),
            actual_units: Some(1),
            upstream_request_ref: None,
        });
        assert!(!report.request_id.is_empty());
        assert!(!report.reservation_id.is_empty());
        assert!(!report.lease_id.is_empty());
        assert_eq!(report.account_ref, "mock-account");
        assert_eq!(report.request_state, RequestState::Settled);
        assert_eq!(report.result, report.replay_result);
        assert_eq!(report.reservation_state, ReservationState::Committed);
        assert_eq!(report.lease_state, LeaseState::Succeeded);
        assert_eq!(report.replay_request_id, report.request_id);
        assert_eq!(report.replay_request_state, report.request_state);
        assert_eq!(report.replay_result, None);
        assert_eq!(report.balance_available, 0);
        assert_eq!(report.balance_held, 0);
        assert_eq!(report.mock_calls, 1);
    }

    #[test]
    fn core_executor_rejection_releases_but_accepted_or_transport_retains() {
        for (outcome, reservation, lease) in [
            (
                UpstreamOutcome::Rejected {
                    status: 429,
                    code: "rate_limited".into(),
                    accepted: false,
                },
                ReservationState::Released,
                LeaseState::Failed,
            ),
            (
                UpstreamOutcome::Rejected {
                    status: 502,
                    code: "upstream_rejected".into(),
                    accepted: true,
                },
                ReservationState::Unknown,
                LeaseState::Unknown,
            ),
            (
                UpstreamOutcome::TransportUnknown {
                    reason: "transport_timeout".into(),
                    upstream_request_ref: None,
                },
                ReservationState::Unknown,
                LeaseState::Unknown,
            ),
        ] {
            let report = run_phase2_mock_chat(outcome);
            assert_eq!(report.result, report.replay_result);
            assert_eq!(report.reservation_state, reservation);
            assert_eq!(report.lease_state, lease);
            assert_eq!(report.replay_request_id, report.request_id);
            assert_eq!(report.replay_request_state, report.request_state);
            assert_eq!(report.mock_calls, 1);
        }
    }

    fn lease(account_ref: &str, credentials_ref: &str) -> UpstreamLeaseGrant {
        UpstreamLeaseGrant {
            lease_id: "lease-test".into(),
            account_ref: account_ref.into(),
            credentials_ref: credentials_ref.into(),
            observation_id: "observation-test".into(),
            predicted_units: 1,
            lease_expires_at_ms: 10,
        }
    }

    fn request() -> ChatExecutionRequest {
        ChatExecutionRequest {
            request_id: "request-test".into(),
            endpoint: "chat".into(),
            model: "mock-1".into(),
            body: serde_json::json!({"model": "mock-1", "messages": []}),
        }
    }

    struct RecordingStreamSink {
        events: Vec<StreamEvent>,
        emit_result: bool,
        cancel_requested: bool,
    }

    impl RecordingStreamSink {
        fn accepting() -> Self {
            Self {
                events: Vec::new(),
                emit_result: true,
                cancel_requested: false,
            }
        }

        fn closed() -> Self {
            Self {
                events: Vec::new(),
                emit_result: false,
                cancel_requested: false,
            }
        }
    }

    impl StreamSink for RecordingStreamSink {
        fn emit(&mut self, event: StreamEvent) -> bool {
            self.events.push(event);
            self.emit_result
        }

        fn cancel_requested(&self) -> bool {
            self.cancel_requested
        }
    }

    #[test]
    fn stream_executor_requires_exact_account_binding_and_rejects_unbound_accounts() {
        let mock = Arc::new(MockStreamAdapter::success_without_usage());
        let executor = CoreUpstreamExecutor::new()
            .with_stream_provider("mock", mock.clone())
            .for_account("account-a", "mock", "vault://a");
        assert!(executor.can_dispatch_stream());

        let mut sink = RecordingStreamSink::accepting();
        let exact = executor.execute_stream(
            &lease("account-a", "vault://a"),
            request(),
            &mut sink,
        );
        assert!(matches!(
            exact,
            StreamTerminalOutcome::Success { actual_units: None, .. }
        ));
        assert_eq!(sink.events.len(), 1);

        let mut unbound_sink = RecordingStreamSink::accepting();
        let unbound = executor.execute_stream(
            &lease("account-b", "vault://b"),
            request(),
            &mut unbound_sink,
        );
        assert!(matches!(
            unbound,
            StreamTerminalOutcome::TransportUnknown { reason, .. }
                if reason == "account_binding_missing"
        ));
        assert!(unbound_sink.events.is_empty());
        assert_eq!(mock.calls().len(), 1);
        assert_eq!(mock.calls()[0].request_id, "request-test");
    }

    #[test]
    fn stream_sink_emit_false_is_not_a_successful_terminal_outcome() {
        let mock = Arc::new(MockStreamAdapter::success_without_usage());
        let executor = CoreUpstreamExecutor::new()
            .with_stream_provider("mock", mock)
            .for_account("account-a", "mock", "vault://a");
        let mut sink = RecordingStreamSink::closed();

        let outcome = executor.execute_stream(
            &lease("account-a", "vault://a"),
            request(),
            &mut sink,
        );
        assert!(matches!(
            outcome,
            StreamTerminalOutcome::TransportUnknown { reason, .. }
                if reason == "stream_sink_closed"
        ));
        assert_eq!(sink.events.len(), 1);
    }

    #[test]
    fn mock_stream_without_usage_keeps_actual_units_unknown_and_records_identity() {
        let mock = Arc::new(MockStreamAdapter::success_without_usage());
        let executor = CoreUpstreamExecutor::new()
            .with_stream_provider("mock", mock.clone())
            .for_account("account-a", "mock", "vault://a");
        let mut sink = RecordingStreamSink::accepting();

        let outcome = executor.execute_stream(
            &lease("account-a", "vault://a"),
            request(),
            &mut sink,
        );
        assert!(matches!(
            outcome,
            StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            }
        ));
        assert_eq!(sink.events[0].usage, None);
        assert_eq!(mock.calls()[0].lease_id, "lease-test");
        assert_eq!(mock.calls()[0].account_ref, "account-a");
        assert_eq!(mock.calls()[0].request_id, "request-test");
    }

    #[test]
    fn stream_provider_registration_is_separate_from_nonstream_capability() {
        let mock = Arc::new(MockStreamAdapter::success_without_usage());
        let stream_only = CoreUpstreamExecutor::new()
            .with_stream_provider("mock", mock)
            .for_account("account-a", "mock", "vault://a");
        assert!(stream_only.can_dispatch_stream());
        assert!(!stream_only.can_dispatch_without_provider_binding());
    }

    #[test]
    fn unbound_account_fails_closed_even_with_one_provider() {
        let mock = Arc::new(MockUpstreamExecutor::ok());
        let executor = CoreUpstreamExecutor::from_adapter(mock.clone());
        let outcome = executor.execute_nonstream_chat(&lease("account-a", "vault://a"), request());
        assert!(matches!(
            outcome,
            UpstreamOutcome::TransportUnknown { reason, .. } if reason == "account_binding_missing"
        ));
        assert!(mock.calls().is_empty());
    }

    #[test]
    fn account_binding_checks_credentials_before_dispatch() {
        let mock = Arc::new(MockUpstreamExecutor::ok());
        let executor = CoreUpstreamExecutor::from_adapter_for_account(
            mock.clone(),
            "account-a",
            "vault://a",
        );
        let exact = executor.execute_nonstream_chat(&lease("account-a", "vault://a"), request());
        assert!(matches!(exact, UpstreamOutcome::Success { .. }));
        let mismatch = executor.execute_nonstream_chat(&lease("account-a", "vault://other"), request());
        assert!(matches!(
            mismatch,
            UpstreamOutcome::TransportUnknown { reason, .. } if reason == "lease_credentials_mismatch"
        ));
        assert_eq!(mock.calls().len(), 1);
    }

    #[test]
    fn chat_executor_registry_requires_explicit_binding_and_passes_lease_identity() {
        let chat = Arc::new(aiwork_core::MockChatExecutor::ok());
        let mut providers = BTreeMap::new();
        providers.insert("mock".into(), chat.clone() as Arc<dyn ChatExecutor>);
        let executor = CoreUpstreamExecutor::from_chat_executors_with_accounts(
            &providers,
            vec![
                (
                    "account-a".into(),
                    "mock".into(),
                    "vault://account-a".into(),
                ),
            ],
        );
        let outcome = executor.execute_nonstream_chat(
            &lease("account-a", "vault://account-a"),
            request(),
        );
        assert!(matches!(outcome, UpstreamOutcome::Success { .. }));
        let mismatch = executor.execute_nonstream_chat(
            &lease("account-a", "vault://other"),
            request(),
        );
        assert!(matches!(
            mismatch,
            UpstreamOutcome::TransportUnknown { reason, .. } if reason == "lease_credentials_mismatch"
        ));
        assert_eq!(chat.calls().len(), 1);
    }

    #[derive(Default)]
    struct RecordingLegacyTransport {
        calls: Mutex<Vec<String>>,
    }

    impl LegacyChatTransport for RecordingLegacyTransport {
        fn execute(
            &self,
            provider: &str,
            account: &crate::api_server::pool::PickedAccount,
            request: ChatExecutionRequest,
        ) -> UpstreamOutcome {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(format!("{provider}:{}:{}", account.uid, request.request_id));
            UpstreamOutcome::Success {
                body: serde_json::json!({"choices": []}),
                actual_units: Some(1),
                upstream_request_ref: Some(request.request_id),
            }
        }
    }

    #[test]
    fn legacy_lease_adapter_dispatches_only_the_core_bound_account() {
        let account = crate::api_server::pool::PickedAccount {
            uid: "uid-a".into(),
            jwt: "jwt-a".into(),
            device_id: "device-a".into(),
            machine_id: "machine-a".into(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
        };
        let resolver_account = account.clone();
        let transport = Arc::new(RecordingLegacyTransport::default());
        let adapter = LegacyPoolLeaseAdapter::with_transport(
            "trae",
            BTreeMap::from([(String::from("account-a"), String::from("uid-a"))]),
            Arc::new(move |uid| (uid == "uid-a").then_some(resolver_account.clone())),
            transport.clone(),
        );

        let exact = adapter.execute_nonstream_chat(
            &lease("account-a", "vault://a"),
            request(),
        );
        assert!(matches!(exact, UpstreamOutcome::Success { .. }));

        let unbound = adapter.execute_nonstream_chat(
            &lease("account-b", "vault://b"),
            request(),
        );
        assert!(matches!(
            unbound,
            UpstreamOutcome::TransportUnknown { reason, .. }
                if reason == "account_binding_missing"
        ));
        assert_eq!(
            transport
                .calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .as_slice(),
            &["trae:uid-a:request-test".to_string()]
        );
    }
}
