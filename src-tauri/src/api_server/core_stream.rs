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
    CancelSupport, CoreUpstreamExecutor, StreamEvent, StreamSink, StreamTerminalOutcome,
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

    match bridge.lookup_chat_replay(&principal, &api_key_id, &idempotency_key, &body) {
        Ok(Some(replay)) => return core_lease_replay_response(&replay),
        Ok(None) => {}
        Err(error) => return core_lease_error_response(error),
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
    let cancel_watch_handle = spawn_cancel_watch(
        bridge.clone(),
        principal.clone(),
        lease.clone(),
        executor.clone(),
        tx.clone(),
        cancel_requested.clone(),
        client_closed.clone(),
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
    let worker_executor = executor.clone();
    let worker_handle = tokio::task::spawn_blocking(move || {
        let mut sink = CoreStreamSink {
            tx,
            proto,
            request_id: worker_request_id.clone(),
            cancel_requested: worker_cancel_requested.clone(),
            client_closed: worker_client_closed.clone(),
            started: false,
            usage: None,
            responses_output_started: false,
            responses_text: String::new(),
        };
        let mut terminal = worker_executor.execute_stream(&worker_lease, execution, &mut sink);
        let cancellation_intent = worker_cancel_requested.load(Ordering::Acquire)
            || worker_client_closed.load(Ordering::Acquire)
            || worker_heartbeat_failed.load(Ordering::Acquire);
        if cancellation_intent {
            terminal = cancellation_outcome(
                &worker_bridge,
                &worker_principal,
                &worker_lease,
                &worker_executor,
                terminal_request_ref(&terminal),
            );
        }
        if !sink.client_closed.load(Ordering::Acquire) {
            if !emit_terminal(&mut sink, &terminal) {
                terminal = StreamTerminalOutcome::TransportUnknown {
                    reason: "stream_terminal_frame_failed".into(),
                    upstream_request_ref: terminal_request_ref(&terminal),
                };
            }
        } else if matches!(
            &terminal,
            StreamTerminalOutcome::Success { .. } | StreamTerminalOutcome::Rejected { .. }
        ) {
            terminal = StreamTerminalOutcome::TransportUnknown {
                reason: "stream_client_closed".into(),
                upstream_request_ref: terminal_request_ref(&terminal),
            };
        }
        settle_stream_outcome(
            &worker_bridge,
            &worker_principal,
            &worker_lease,
            &terminal,
        );
        drop(guard);
    });
    tokio::spawn(async move {
        let result = worker_handle.await;
        stop_heartbeat.store(true, Ordering::Release);
        heartbeat_handle.abort();
        cancel_watch_handle.abort();
        if result.is_err() {
            settle_stream_outcome(
                &bridge,
                &principal,
                &lease,
                &StreamTerminalOutcome::TransportUnknown {
                    reason: "stream_worker_unknown".into(),
                    upstream_request_ref: None,
                },
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

fn spawn_cancel_watch(
    bridge: Arc<super::CoreBridge>,
    principal: Principal,
    lease: UpstreamLeaseGrant,
    executor: CoreUpstreamExecutor,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    cancel_requested: Arc<AtomicBool>,
    client_closed: Arc<AtomicBool>,
    heartbeat_failed: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(50));
        interval.tick().await;
        let triggered = loop {
            tokio::select! {
                _ = tx.closed() => {
                    client_closed.store(true, Ordering::Release);
                    cancel_requested.store(true, Ordering::Release);
                    break true;
                }
                _ = interval.tick() => {
                    if stop.load(Ordering::Acquire) {
                        break false;
                    }
                    if cancel_requested.load(Ordering::Acquire)
                        || heartbeat_failed.load(Ordering::Acquire)
                    {
                        break true;
                    }
                }
            }
        };
        if !triggered {
            return;
        }
        let _ = tokio::task::spawn_blocking(move || {
            let outcome = cancellation_outcome(&bridge, &principal, &lease, &executor, None);
            settle_stream_outcome(&bridge, &principal, &lease, &outcome);
        })
        .await;
    })
}

fn terminal_request_ref(outcome: &StreamTerminalOutcome) -> Option<String> {
    match outcome {
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

fn cancellation_outcome(
    bridge: &super::CoreBridge,
    principal: &Principal,
    lease: &UpstreamLeaseGrant,
    executor: &CoreUpstreamExecutor,
    upstream_request_ref: Option<String>,
) -> StreamTerminalOutcome {
    let cancel_result = bridge.store.request_upstream_cancel(
        principal,
        &lease.lease_id,
        chrono::Utc::now().timestamp_millis(),
    );
    let cancel_support = executor.cancel_stream(lease);
    if cancel_result.is_ok() && matches!(cancel_support, CancelSupport::Confirmed) {
        StreamTerminalOutcome::Canceled { upstream_request_ref }
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
    }
}

fn settle_stream_outcome(
    bridge: &super::CoreBridge,
    principal: &Principal,
    lease: &UpstreamLeaseGrant,
    outcome: &StreamTerminalOutcome,
) {
    if bridge
        .settle_chat_lease(
            principal,
            &lease.lease_id,
            outcome.lease_outcome(chrono::Utc::now().timestamp_millis()),
        )
        .is_err()
    {
        let _ = bridge.settle_chat_lease(
            principal,
            &lease.lease_id,
            StreamTerminalOutcome::TransportUnknown {
                reason: "stream_settlement_failed".into(),
                upstream_request_ref: None,
            }
            .lease_outcome(chrono::Utc::now().timestamp_millis()),
        );
    }
}

struct CoreStreamSink {
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    proto: Protocol,
    request_id: String,
    cancel_requested: Arc<AtomicBool>,
    client_closed: Arc<AtomicBool>,
    started: bool,
    usage: Option<super::core_bridge::StreamUsage>,
    responses_output_started: bool,
    responses_text: String,
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
        if let Some(usage) = event.usage.clone() {
            self.usage = Some(usage);
            if !matches!(self.proto, Protocol::OpenAi | Protocol::OpenAiText) {
                return true;
            }
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
            Protocol::Responses => {
                if text.is_empty() {
                    return true;
                }
                if !self.responses_output_started {
                    if !self.send_frame(format!(
                        "event: response.output_item.added\ndata: {}\n\n",
                        json!({
                            "type": "response.output_item.added",
                            "response_id": format!("resp_{}", self.request_id),
                            "output_index": 0,
                            "item": {
                                "id": format!("msg_{}", self.request_id),
                                "type": "message",
                                "role": "assistant",
                                "content": [],
                                "status": "in_progress"
                            }
                        })
                    )) {
                        return false;
                    }
                    if !self.send_frame(format!(
                        "event: response.content_part.added\ndata: {}\n\n",
                        json!({
                            "type": "response.content_part.added",
                            "response_id": format!("resp_{}", self.request_id),
                            "item_id": format!("msg_{}", self.request_id),
                            "output_index": 0,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []}
                        })
                    )) {
                        return false;
                    }
                    self.responses_output_started = true;
                }
                self.responses_text.push_str(&text);
                return self.send_frame(format!(
                    "event: response.output_text.delta\ndata: {}\n\n",
                    json!({
                        "type": "response.output_text.delta",
                        "response_id": format!("resp_{}", self.request_id),
                        "item_id": format!("msg_{}", self.request_id),
                        "output_index": 0,
                        "content_index": 0,
                        "delta": text,
                    })
                ));
            }
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

fn emit_terminal(sink: &mut CoreStreamSink, outcome: &StreamTerminalOutcome) -> bool {
    if !sink.ensure_started() {
        return false;
    }
    match outcome {
        StreamTerminalOutcome::Success { .. } => match sink.proto {
            Protocol::OpenAi | Protocol::OpenAiText => {
                sink.send_frame("data: [DONE]\n\n".into())
            }
            Protocol::Responses => {
                if sink.responses_output_started
                    && (!sink.send_frame(format!(
                        "event: response.output_text.done\ndata: {}\n\n",
                        json!({
                            "type": "response.output_text.done",
                            "response_id": format!("resp_{}", sink.request_id),
                            "item_id": format!("msg_{}", sink.request_id),
                            "output_index": 0,
                            "content_index": 0,
                            "text": sink.responses_text
                        })
                    )) || !sink.send_frame(format!(
                        "event: response.content_part.done\ndata: {}\n\n",
                        json!({
                            "type": "response.content_part.done",
                            "response_id": format!("resp_{}", sink.request_id),
                            "item_id": format!("msg_{}", sink.request_id),
                            "output_index": 0,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": sink.responses_text, "annotations": []}
                        })
                    )) || !sink.send_frame(format!(
                        "event: response.output_item.done\ndata: {}\n\n",
                        json!({
                            "type": "response.output_item.done",
                            "response_id": format!("resp_{}", sink.request_id),
                            "output_index": 0,
                            "item": {
                                "id": format!("msg_{}", sink.request_id),
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": sink.responses_text, "annotations": []}],
                                "status": "completed"
                            }
                        })
                    )))
                {
                    return false;
                }
                let mut response = json!({
                    "id": format!("resp_{}", sink.request_id),
                    "status": "completed"
                });
                if let Some(usage) = &sink.usage {
                    response["usage"] = json!({
                        "input_tokens": usage.prompt_tokens.unwrap_or(0),
                        "output_tokens": usage.completion_tokens.unwrap_or(0),
                        "total_tokens": usage.total_tokens.unwrap_or_else(|| {
                            usage.prompt_tokens.unwrap_or(0) + usage.completion_tokens.unwrap_or(0)
                        })
                    });
                }
                response["output"] = json!([{
                    "id": format!("msg_{}", sink.request_id),
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": sink.responses_text, "annotations": []}],
                    "status": "completed"
                }]);
                sink.send_frame(format!(
                    "event: response.completed\ndata: {}\n\n",
                    json!({"type": "response.completed", "response": response})
                ))
            }
            Protocol::Anthropic => {
                if !sink.send_frame(format!(
                    "event: content_block_stop\ndata: {}\n\n",
                    json!({"type": "content_block_stop", "index": 0})
                )) {
                    return false;
                }
                let mut message_delta = json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null}
                });
                if let Some(usage) = &sink.usage {
                    message_delta["usage"] = json!({
                        "input_tokens": usage.prompt_tokens.unwrap_or(0),
                        "output_tokens": usage.completion_tokens.unwrap_or(0)
                    });
                }
                if !sink.send_frame(format!(
                    "event: message_delta\ndata: {}\n\n",
                    message_delta
                )) {
                    return false;
                }
                sink.send_frame(format!(
                    "event: message_stop\ndata: {}\n\n",
                    json!({"type": "message_stop", "stop_reason": "end_turn"})
                ))
            }
        },
        StreamTerminalOutcome::Rejected { status, code, .. } => {
            let _ = (status, code);
            emit_error(sink, "upstream_rejected", "upstream request was rejected")
        }
        StreamTerminalOutcome::Canceled { .. } => {
            emit_error(sink, "canceled", "stream cancellation was confirmed")
        }
        StreamTerminalOutcome::TransportUnknown { .. } => {
            emit_error(sink, "upstream_unknown", "upstream stream outcome is unknown")
        }
    }
}

fn emit_error(sink: &mut CoreStreamSink, code: &str, message: &str) -> bool {
    match sink.proto {
        Protocol::OpenAi | Protocol::OpenAiText => {
            if !sink.send_frame(format!(
                "data: {}\n\n",
                json!({"error": {"type": "api_error", "code": code, "message": message}})
            )) {
                return false;
            }
            sink.send_frame("data: [DONE]\n\n".into())
        }
        Protocol::Responses => {
            sink.send_frame(format!(
                "event: response.failed\ndata: {}\n\n",
                json!({"type": "response.failed", "response": {"id": format!("resp_{}", sink.request_id), "status": "failed", "error": {"code": code, "message": message}}})
            ))
        }
        Protocol::Anthropic => {
            sink.send_frame(format!(
                "event: error\ndata: {}\n\n",
                json!({"type": "error", "error": {"type": code, "message": message}})
            ))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sink_for(
        proto: Protocol,
        capacity: usize,
    ) -> (
        CoreStreamSink,
        mpsc::Receiver<Result<Bytes, std::io::Error>>,
    ) {
        let (tx, rx) = mpsc::channel(capacity);
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let client_closed = Arc::new(AtomicBool::new(false));
        (
            CoreStreamSink {
                tx,
                proto,
                request_id: "request-test".into(),
                cancel_requested,
                client_closed,
                started: false,
                usage: None,
                responses_output_started: false,
                responses_text: String::new(),
            },
            rx,
        )
    }

    fn drain_frames(
        receiver: &mut mpsc::Receiver<Result<Bytes, std::io::Error>>,
    ) -> Vec<String> {
        let mut frames = Vec::new();
        while let Ok(frame) = receiver.try_recv() {
            frames.push(String::from_utf8(frame.expect("stream frame").to_vec()).unwrap());
        }
        frames
    }

    #[test]
    fn terminal_frame_failure_is_not_settled_as_success() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let client_closed = Arc::new(AtomicBool::new(false));
        let cancel_requested = Arc::new(AtomicBool::new(false));
        let mut sink = CoreStreamSink {
            tx,
            proto: Protocol::OpenAi,
            request_id: "request-test".into(),
            cancel_requested: cancel_requested.clone(),
            client_closed: client_closed.clone(),
            started: false,
            usage: None,
            responses_output_started: false,
            responses_text: String::new(),
        };

        assert!(!emit_terminal(
            &mut sink,
            &StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            }
        ));
        assert!(client_closed.load(Ordering::Acquire));
        assert!(cancel_requested.load(Ordering::Acquire));
    }

    #[test]
    fn responses_and_anthropic_terminal_sequences_preserve_usage() {
        let (mut responses, mut responses_rx) = sink_for(Protocol::Responses, 16);
        assert!(responses.emit(StreamEvent {
            data: json!({"content": "hello"}),
            usage: None,
            upstream_request_ref: None,
        }));
        assert!(responses.emit(StreamEvent {
            data: json!({}),
            usage: Some(super::super::core_bridge::StreamUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
                total_tokens: Some(5),
            }),
            upstream_request_ref: None,
        }));
        assert!(emit_terminal(
            &mut responses,
            &StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            }
        ));
        let responses_frames = drain_frames(&mut responses_rx).join("");
        assert!(responses_frames.find("response.output_item.added").unwrap()
            < responses_frames.find("response.output_text.delta").unwrap());
        assert!(responses_frames.find("response.output_text.done").unwrap()
            < responses_frames.find("response.completed").unwrap());
        assert!(responses_frames.contains("\"total_tokens\":5"));
        assert_eq!(responses_frames.matches("event: response.output_text.delta").count(), 1);

        let (mut anthropic, mut anthropic_rx) = sink_for(Protocol::Anthropic, 16);
        assert!(anthropic.emit(StreamEvent {
            data: json!({"content": "hello"}),
            usage: None,
            upstream_request_ref: None,
        }));
        assert!(anthropic.emit(StreamEvent {
            data: json!({}),
            usage: Some(super::super::core_bridge::StreamUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(2),
                total_tokens: Some(5),
            }),
            upstream_request_ref: None,
        }));
        assert!(emit_terminal(
            &mut anthropic,
            &StreamTerminalOutcome::Success {
                actual_units: None,
                upstream_request_ref: None,
            }
        ));
        let anthropic_frames = drain_frames(&mut anthropic_rx).join("");
        assert!(anthropic_frames.find("message_delta").unwrap()
            < anthropic_frames.find("message_stop").unwrap());
        assert!(anthropic_frames.contains("\"output_tokens\":2"));
    }
}
