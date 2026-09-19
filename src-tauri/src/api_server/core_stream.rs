use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;
use bytes::Bytes;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use aiwork_core::{require_scope, CoreError, Principal, UpstreamLeaseGrant};

use super::core_bridge::{
    CancelSupport, StreamEvent, StreamSink, StreamTerminalOutcome,
};
use super::routes::{
    anthropic_error, core_lease_error_response, core_lease_replay_response,
    scheduler_endpoint_not_enabled_response, openai_error, Protocol,
};
use super::{ApiSharedState, CoreMode};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const STREAM_CHANNEL_CAPACITY: usize = 32;

pub(super) fn core_stream_chat(
    state: Arc<ApiSharedState>,
    principal: Principal,
    api_key_id: String,
    mut body: Value,
    proto: Protocol,
    idempotency_key: Option<String>,
) -> Response {
    let Some(bridge) = state.core.as_ref().cloned() else {
        return scheduler_endpoint_not_enabled_response();
    };
    if bridge.mode != CoreMode::Enforce {
        return scheduler_endpoint_not_enabled_response();
    }
    if require_scope(&principal, "chat:invoke").is_err() {
        return openai_error(
            StatusCode::FORBIDDEN,
            "insufficient_scope",
            "missing required scope: chat:invoke",
        );
    }
    let Some(idempotency_key) = idempotency_key
        .filter(|key| !key.trim().is_empty())
    else {
        return protocol_error(
            proto,
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key is required in Core enforce mode",
        );
    };
    if body.get("model").and_then(Value::as_str).is_none() {
        body["model"] = json!(state.default_model.clone());
    }

    // Readiness is checked before the atomic preflight. A missing stream
    // adapter therefore creates neither a request row nor a quota hold.
    let executor = match bridge.upstream_executor() {
        Ok(executor) if executor.can_dispatch_stream() => executor,
        Ok(_) => return scheduler_endpoint_not_enabled_response(),
        Err(error) => return core_lease_error_response(error),
    };
    let bound_account_refs = executor.bound_account_refs();
    let preflight = match bridge.preflight_chat_with_lease_for_accounts(
        &principal,
        &api_key_id,
        Some(&idempotency_key),
        &body,
        &bound_account_refs,
    ) {
        Ok(preflight) => preflight,
        Err(error) => return core_lease_error_response(error),
    };
    if preflight.execution.is_none() {
        return core_lease_replay_response(&preflight);
    }
    let Some(lease) = preflight.lease else {
        return core_error_response_for_stream(
            proto,
            CoreError::InvalidConfiguration {
                key: "core.stream.lease".into(),
                value: "new preflight did not return an execution lease".into(),
            },
        );
    };
    let Some(_reservation) = preflight.reservation else {
        return core_error_response_for_stream(
            proto,
            CoreError::InvalidConfiguration {
                key: "core.stream.reservation".into(),
                value: "new preflight did not return a quota reservation".into(),
            },
        );
    };
    let Some(execution) = preflight.execution else {
        return core_error_response_for_stream(
            proto,
            CoreError::InvalidConfiguration {
                key: "core.stream.execution".into(),
                value: "new preflight did not return an execution request".into(),
            },
        );
    };

    let request_id = preflight.request_id;
    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
    let cancel_requested = Arc::new(AtomicBool::new(false));
    let client_closed = Arc::new(AtomicBool::new(false));
    let stop_heartbeat = Arc::new(AtomicBool::new(false));
    let heartbeat_failed = Arc::new(AtomicBool::new(false));
    let heartbeat_handle = spawn_heartbeat(
        bridge.clone(),
        principal.clone(),
        lease.clone(),
        cancel_requested.clone(),
        heartbeat_failed.clone(),
        stop_heartbeat.clone(),
    );
    let guard = state.inflight_guard();
    let worker_bridge = bridge.clone();
    let worker_principal = principal.clone();
    let worker_lease = lease.clone();
    let worker_request_id = request_id.clone();
    let worker_cancel_requested = cancel_requested.clone();
    let worker_client_closed = client_closed.clone();
    let worker_heartbeat_failed = heartbeat_failed.clone();
    let worker_handle = tokio::task::spawn_blocking(move || {
        let mut sink = CoreStreamSink {
            tx,
            proto,
            request_id: worker_request_id.clone(),
            cancel_requested: worker_cancel_requested.clone(),
            client_closed: worker_client_closed.clone(),
            started: false,
        };
        let mut terminal = executor.execute_stream(&worker_lease, execution, &mut sink);
        let cancellation_intent = worker_cancel_requested.load(Ordering::Acquire)
            || worker_client_closed.load(Ordering::Acquire)
            || worker_heartbeat_failed.load(Ordering::Acquire);
        if cancellation_intent {
            let cancel_result = worker_bridge.store.request_upstream_cancel(
                &worker_principal,
                &worker_lease.lease_id,
                chrono::Utc::now().timestamp_millis(),
            );
            let cancel_support = executor.cancel_stream(&worker_lease);
            terminal = if cancel_result.is_ok()
                && matches!(cancel_support, CancelSupport::Confirmed)
            {
                let upstream_request_ref = match terminal {
                    StreamTerminalOutcome::Canceled {
                        upstream_request_ref,
                    }
                    | StreamTerminalOutcome::Success {
                        upstream_request_ref,
                        ..
                    }
                    | StreamTerminalOutcome::TransportUnknown {
                        upstream_request_ref,
                        ..
                    } => upstream_request_ref,
                    StreamTerminalOutcome::Rejected { .. } => None,
                };
                StreamTerminalOutcome::Canceled {
                    upstream_request_ref,
                }
            } else {
                StreamTerminalOutcome::TransportUnknown {
                    reason: if cancel_result.is_err() {
                        "stream_cancel_record_failed".into()
                    } else {
                        match cancel_support {
                            CancelSupport::Unsupported => "stream_cancel_unsupported",
                            CancelSupport::Unknown => "stream_cancel_unknown",
                            CancelSupport::Confirmed => "stream_cancel_unconfirmed",
                        }
                        .into()
                    },
                    upstream_request_ref: None,
                }
            };
        }
        if !sink.client_closed.load(Ordering::Acquire) {
            emit_terminal(&mut sink, &terminal);
        }
        let _ = worker_bridge.settle_chat_lease(
            &worker_principal,
            &worker_lease.lease_id,
            terminal.lease_outcome(chrono::Utc::now().timestamp_millis()),
        );
        drop(guard);
    });
    tokio::spawn(async move {
        let result = worker_handle.await;
        stop_heartbeat.store(true, Ordering::Release);
        heartbeat_handle.abort();
        if result.is_err() {
            let _ = bridge.settle_chat_lease(
                &principal,
                &lease.lease_id,
                StreamTerminalOutcome::TransportUnknown {
                    reason: "stream_worker_unknown".into(),
                    upstream_request_ref: None,
                }
                .lease_outcome(chrono::Utc::now().timestamp_millis()),
            );
        }
    });

    let mut response = Response::new(Body::from_stream(ReceiverStream::new(rx)));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-cache, no-transform"),
    );
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

fn spawn_heartbeat(
    bridge: Arc<super::CoreBridge>,
    principal: Principal,
    lease: UpstreamLeaseGrant,
    cancel_requested: Arc<AtomicBool>,
    heartbeat_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        interval.tick().await;
        loop {
            interval.tick().await;
            if stop.load(Ordering::Acquire) {
                break;
            }
            if bridge
                .store
                .heartbeat_upstream_lease(
                    &principal,
                    &lease.lease_id,
                    chrono::Utc::now().timestamp_millis(),
                    15 * 60 * 1000,
                )
                .is_err()
            {
                heartbeat_failed.store(true, Ordering::Release);
                cancel_requested.store(true, Ordering::Release);
                break;
            }
        }
    })
}

struct CoreStreamSink {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    proto: Protocol,
    request_id: String,
    cancel_requested: Arc<AtomicBool>,
    client_closed: Arc<AtomicBool>,
    started: bool,
}

impl CoreStreamSink {
    fn send_frame(&mut self, frame: String) -> bool {
        if self.tx.blocking_send(Ok(Bytes::from(frame))).is_err() {
            self.client_closed.store(true, Ordering::Release);
            self.cancel_requested.store(true, Ordering::Release);
            return false;
        }
        true
    }

    fn ensure_started(&mut self) -> bool {
        if self.started {
            return true;
        }
        let frames = match self.proto {
            Protocol::Anthropic => vec![
                format!(
                    "event: message_start\ndata: {}\n\n",
                    json!({
                        "type": "message_start",
                        "message": {"id": format!("msg_{}", self.request_id), "type": "message", "role": "assistant", "content": [], "model": "mock", "stop_reason": null}
                    })
                ),
                format!(
                    "event: content_block_start\ndata: {}\n\n",
                    json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}})
                ),
            ],
            Protocol::Responses => vec![format!(
                "event: response.created\ndata: {}\n\n",
                json!({"type": "response.created", "response": {"id": format!("resp_{}", self.request_id), "status": "in_progress"}})
            )],
            Protocol::OpenAi | Protocol::OpenAiText => Vec::new(),
        };
        for frame in frames {
            if !self.send_frame(frame) {
                return false;
            }
        }
        self.started = true;
        true
    }
}

impl StreamSink for CoreStreamSink {
    fn emit(&mut self, event: StreamEvent) -> bool {
        if !self.ensure_started() {
            return false;
        }
        let text = event_text(&event.data).unwrap_or_default();
        let frame = match self.proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                let payload = if let Some(usage) = event.usage {
                    json!({
                        "id": format!("chatcmpl-{}", self.request_id),
                        "object": "chat.completion.chunk",
                        "choices": [],
                        "usage": {
                            "prompt_tokens": usage.prompt_tokens,
                            "completion_tokens": usage.completion_tokens,
                            "total_tokens": usage.total_tokens,
                        }
                    })
                } else {
                    json!({
                        "id": format!("chatcmpl-{}", self.request_id),
                        "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": null}]
                    })
                };
                format!("data: {payload}\n\n")
            }
            Protocol::Responses => format!(
                "event: response.output_text.delta\ndata: {}\n\n",
                json!({
                    "type": "response.output_text.delta",
                    "response_id": format!("resp_{}", self.request_id),
                    "delta": text,
                })
            ),
            Protocol::Anthropic => format!(
                "event: content_block_delta\ndata: {}\n\n",
                json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                })
            ),
        };
        self.send_frame(frame)
    }

    fn cancel_requested(&self) -> bool {
        self.cancel_requested.load(Ordering::Acquire)
            || self.client_closed.load(Ordering::Acquire)
    }
}

fn event_text(data: &Value) -> Option<String> {
    data.get("content")
        .and_then(Value::as_str)
        .or_else(|| data.get("text").and_then(Value::as_str))
        .or_else(|| data.get("delta").and_then(|delta| delta.get("content")).and_then(Value::as_str))
        .map(ToOwned::to_owned)
}

fn emit_terminal(sink: &mut CoreStreamSink, outcome: &StreamTerminalOutcome) {
    let _ = sink.ensure_started();
    match outcome {
        StreamTerminalOutcome::Success { .. } => match sink.proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                let _ = sink.send_frame("data: [DONE]\n\n".into());
            }
            Protocol::Responses => {
                let _ = sink.send_frame(format!(
                    "event: response.completed\ndata: {}\n\n",
                    json!({"type": "response.completed", "response": {"id": format!("resp_{}", sink.request_id), "status": "completed"}})
                ));
            }
            Protocol::Anthropic => {
                let _ = sink.send_frame(format!(
                    "event: content_block_stop\ndata: {}\n\n",
                    json!({"type": "content_block_stop", "index": 0})
                ));
                let _ = sink.send_frame(format!(
                    "event: message_stop\ndata: {}\n\n",
                    json!({"type": "message_stop", "stop_reason": "end_turn"})
                ));
            }
        },
        StreamTerminalOutcome::Rejected { status, code, .. } => {
            emit_error(sink, "upstream_rejected", &format!("upstream rejected with status {status}: {code}"));
        }
        StreamTerminalOutcome::Canceled { .. } => {
            emit_error(sink, "canceled", "stream cancellation was confirmed by the upstream");
        }
        StreamTerminalOutcome::TransportUnknown { reason, .. } => {
            emit_error(sink, "upstream_unknown", &format!("upstream stream outcome is unknown: {reason}"));
        }
    }
}

fn emit_error(sink: &mut CoreStreamSink, code: &str, message: &str) {
    match sink.proto {
        Protocol::OpenAi | Protocol::OpenAiText => {
            let _ = sink.send_frame(format!(
                "data: {}\n\n",
                json!({"error": {"type": "api_error", "code": code, "message": message}})
            ));
            let _ = sink.send_frame("data: [DONE]\n\n".into());
        }
        Protocol::Responses => {
            let _ = sink.send_frame(format!(
                "event: response.failed\ndata: {}\n\n",
                json!({"type": "response.failed", "response": {"id": format!("resp_{}", sink.request_id), "status": "failed", "error": {"code": code, "message": message}}})
            ));
        }
        Protocol::Anthropic => {
            let _ = sink.send_frame(format!(
                "event: error\ndata: {}\n\n",
                json!({"type": "error", "error": {"type": code, "message": message}})
            ));
        }
    }
}

fn protocol_error(proto: Protocol, status: StatusCode, code: &str, message: &str) -> Response {
    match proto {
        Protocol::Anthropic => anthropic_error(status, code, message),
        Protocol::OpenAi | Protocol::OpenAiText | Protocol::Responses => {
            openai_error(status, code, message)
        }
    }
}

fn core_error_response_for_stream(proto: Protocol, error: CoreError) -> Response {
    protocol_error(proto, StatusCode::INTERNAL_SERVER_ERROR, "core_error", &error.to_string())
}
