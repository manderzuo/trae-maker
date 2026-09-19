use std::sync::{Arc, Mutex};

use serde_json::Value;

/// A credential-free request handed from Core to an upstream adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatExecutionRequest {
    pub request_id: String,
    pub endpoint: String,
    pub model: String,
    pub body: Value,
}

/// The upstream response fields needed by Core settlement and gateway projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatExecutionResult {
    pub status: u16,
    pub body: Value,
    pub actual_amount: Option<i64>,
}

impl ChatExecutionResult {
    pub fn ok() -> Self {
        Self {
            status: 200,
            body: serde_json::json!({"ok": true}),
            actual_amount: Some(1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UpstreamError {
    #[error("upstream rejected the request with status {status}")]
    Rejected {
        status: u16,
        code: Option<String>,
    },
    #[error("upstream transport timed out")]
    Timeout,
    #[error("upstream transport disconnected")]
    Disconnected,
    #[error("upstream execution failed: {message}")]
    Failed { message: String },
}

impl UpstreamError {
    pub fn is_uncertain(&self) -> bool {
        matches!(self, Self::Timeout | Self::Disconnected)
    }
}

/// The narrow upstream boundary used by the gateway. It intentionally carries
/// no API key, JWT, session token, or account-pool handle.
pub trait ChatExecutor: Send + Sync {
    fn execute(&self, request: ChatExecutionRequest) -> Result<ChatExecutionResult, UpstreamError>;
}

/// Test-only construction helper. Runtime code never creates this executor by
/// default; it is a deterministic in-memory adapter for Core flow tests.
#[derive(Clone)]
pub struct MockChatExecutor {
    pub calls: Arc<Mutex<Vec<ChatExecutionRequest>>>,
    pub response: ChatExecutionResult,
    error: Option<UpstreamError>,
}

impl MockChatExecutor {
    pub fn ok() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: ChatExecutionResult::ok(),
            error: None,
        }
    }

    pub fn with_response(response: ChatExecutionResult) -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response,
            error: None,
        }
    }

    /// Deterministic transport-timeout outcome for Core integration tests.
    /// Runtime adapters never construct this test-only executor.
    pub fn timeout() -> Self {
        Self {
            calls: Arc::new(Mutex::new(Vec::new())),
            response: ChatExecutionResult::ok(),
            error: Some(UpstreamError::Timeout),
        }
    }

    pub fn calls(&self) -> Vec<ChatExecutionRequest> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl ChatExecutor for MockChatExecutor {
    fn execute(&self, request: ChatExecutionRequest) -> Result<ChatExecutionResult, UpstreamError> {
        self.calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(request);
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        Ok(self.response.clone())
    }
}
