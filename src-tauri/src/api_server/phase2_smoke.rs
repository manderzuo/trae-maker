//! Phase 2 delivery smoke entry point.
//!
//! This module is intentionally test-only: it composes the deterministic Core
//! Mock adapter and never reads credentials, legacy account pools, or network
//! configuration. Production enforce remains fail-closed until explicit
//! `(account_ref, provider, credentials_ref)` startup bindings are wired.

#[cfg(test)]
use super::core_bridge::core_executor::{
    run_phase2_mock_chat as run_core_phase2_mock_chat, Phase2MockChatReport, UpstreamOutcome,
};

#[cfg(test)]
use aiwork_core::{LeaseState, ReservationState};

/// Result of the Tauri-side Phase 2 mock composition.
///
/// Each field is produced by a fresh isolated Core fixture. The adapter is
/// always the in-memory MockUpstreamExecutor; no runtime registry or upstream
/// credential path is consulted.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct Phase2SmokeReport {
    pub success: Phase2MockChatReport,
    pub rejected: Phase2MockChatReport,
    pub unknown: Phase2MockChatReport,
}

#[cfg(test)]
impl Phase2SmokeReport {
    pub fn is_mock_only(&self) -> bool {
        self.success.account_ref == "mock-account"
            && self.rejected.account_ref == "mock-account"
            && self.unknown.account_ref == "mock-account"
            && self.success.mock_calls == 1
            && self.rejected.mock_calls == 1
            && self.unknown.mock_calls == 1
    }
}

/// Run the Tauri integration smoke with deterministic Mock outcomes only.
///
/// This is compiled only for tests so it cannot accidentally become a
/// production upstream path. Enforce remains fail-closed until startup wires
/// real account/provider/credential bindings explicitly.
#[cfg(test)]
pub fn run_phase2_mock_chat() -> Phase2SmokeReport {
    let success = run_core_phase2_mock_chat(UpstreamOutcome::Success {
        body: serde_json::json!({"choices": []}),
        actual_units: Some(1),
        upstream_request_ref: None,
    });
    let rejected = run_core_phase2_mock_chat(UpstreamOutcome::Rejected {
        status: 429,
        code: "rate_limited".to_owned(),
        accepted: false,
    });
    let unknown = run_core_phase2_mock_chat(UpstreamOutcome::TransportUnknown {
        reason: "transport_timeout".to_owned(),
        upstream_request_ref: None,
    });
    Phase2SmokeReport {
        success,
        rejected,
        unknown,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn phase2_smoke_entrypoint_is_mock_only() {
        let report = super::run_phase2_mock_chat();
        assert!(report.is_mock_only());
        assert_eq!(report.success.lease_state, super::LeaseState::Succeeded);
        assert_eq!(
            report.rejected.reservation_state,
            super::ReservationState::Released
        );
        assert_eq!(report.unknown.lease_state, super::LeaseState::Unknown);
    }
}
