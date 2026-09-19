use aiwork_core::{ObservationError, ObservationReader, ObservationRequest, ObservationSnapshot};

use crate::{
    commands::{accounts, workbuddy},
    state::AppState,
};

/// Read-only adapter over the existing Trae entitlement and WorkBuddy cache
/// code. It deliberately owns no credentials and never writes Core state.
pub(crate) struct TauriObservationReader<'a> {
    state: &'a AppState,
    observed_at_ms: i64,
}

impl<'a> TauriObservationReader<'a> {
    pub(crate) fn new(state: &'a AppState, observed_at_ms: i64) -> Self {
        Self {
            state,
            observed_at_ms,
        }
    }

    pub(crate) fn read_trae(
        &self,
        request: &ObservationRequest,
    ) -> Result<ObservationSnapshot, ObservationError> {
        if request.provider != "trae" {
            return Err(ObservationError::Unavailable);
        }
        accounts::read_trae_observation(
            self.state,
            &request.account_ref,
            &request.resource_kind,
            self.observed_at_ms,
        )
        .map_err(|_| ObservationError::Unavailable)
    }

    pub(crate) fn read_workbuddy(
        &self,
        request: &ObservationRequest,
    ) -> Result<ObservationSnapshot, ObservationError> {
        if request.provider != "workbuddy" {
            return Err(ObservationError::Unavailable);
        }
        workbuddy::read_workbuddy_cached_observation(
            self.state,
            &request.account_ref,
            &request.resource_kind,
            self.observed_at_ms,
        )
        .map_err(|_| ObservationError::Unavailable)
    }

    pub(crate) fn snapshot_from_trae_values(
        account_ref: &str,
        resource_kind: &str,
        available: f64,
        expires_at_ms: i64,
        observed_at_ms: i64,
    ) -> Result<ObservationSnapshot, ObservationError> {
        let available_units = scaled_units(Some(available))?;
        let stale_at_ms = if expires_at_ms > observed_at_ms {
            expires_at_ms
        } else {
            observed_at_ms
        };
        Ok(ObservationSnapshot {
            account_ref: account_ref.into(),
            resource_kind: resource_kind.into(),
            available_units,
            value_scale: 100,
            source: "reader".into(),
            observed_at_ms,
            stale_at_ms,
            capabilities: vec!["chat".into()],
            region: Some("cn".into()),
            summary: serde_json::json!({
                "available": available_units,
                "expires_at_ms": stale_at_ms,
                "value_scale": 100,
                "source": "reader",
                "resource_kind": resource_kind,
                "observation_kind": "trae_entitlement",
            }),
        })
    }

    pub(crate) fn snapshot_from_workbuddy_cache(
        account_ref: &str,
        resource_kind: &str,
        available: Option<f64>,
        observed_at_ms: i64,
        region: Option<&str>,
    ) -> Result<ObservationSnapshot, ObservationError> {
        let available_units = scaled_units(available)?;
        let mut summary = serde_json::json!({
            "value_scale": 100,
            "source": "json_cache",
            "resource_kind": resource_kind,
            "status": "stale",
            "observation_kind": "workbuddy_cache",
        });
        if let Some(available_units) = available_units {
            summary["available"] = serde_json::json!(available_units);
        }
        Ok(ObservationSnapshot {
            account_ref: account_ref.into(),
            resource_kind: resource_kind.into(),
            available_units,
            value_scale: 100,
            source: "json_cache".into(),
            observed_at_ms,
            // Cache values are diagnostics only. Keeping stale_at equal to the
            // observation timestamp prevents freshness-based selection.
            stale_at_ms: observed_at_ms,
            capabilities: vec!["chat".into()],
            region: region.map(str::to_owned),
            summary,
        })
    }
}

impl ObservationReader for TauriObservationReader<'_> {
    fn read(&self, request: ObservationRequest) -> Result<ObservationSnapshot, ObservationError> {
        match request.provider.as_str() {
            "trae" => self.read_trae(&request),
            "workbuddy" => self.read_workbuddy(&request),
            _ => Err(ObservationError::Unavailable),
        }
    }
}

fn scaled_units(value: Option<f64>) -> Result<Option<i64>, ObservationError> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !value.is_finite() || value < 0.0 {
        return Err(ObservationError::Unavailable);
    }
    let scaled = (value * 100.0).round();
    if scaled > i64::MAX as f64 {
        return Err(ObservationError::Unavailable);
    }
    Ok(Some(scaled as i64))
}

#[cfg(test)]
mod tests {
    use super::TauriObservationReader;

    const NOW_MS: i64 = 1_725_000_000_000;

    #[test]
    fn trae_fixture_keeps_scale_resource_and_redacts_credentials() {
        let snapshot = TauriObservationReader::snapshot_from_trae_values(
            "trae-account",
            "chat.general",
            12.5,
            NOW_MS + 300_000,
            NOW_MS,
        )
        .unwrap();

        assert_eq!(snapshot.available_units, Some(1_250));
        assert_eq!(snapshot.value_scale, 100);
        assert_eq!(snapshot.resource_kind, "chat.general");
        assert_eq!(snapshot.source, "reader");
        let stored = snapshot.summary.to_string();
        assert!(!stored.contains("jwt") && !stored.contains("token") && !stored.contains("cookie"));
    }

    #[test]
    fn workbuddy_cache_fixture_is_explicitly_stale_without_network() {
        let snapshot = TauriObservationReader::snapshot_from_workbuddy_cache(
            "wb-account",
            "chat.work",
            Some(6.25),
            NOW_MS - 60_000,
            Some("us"),
        )
        .unwrap();

        assert_eq!(snapshot.available_units, Some(625));
        assert_eq!(snapshot.value_scale, 100);
        assert_eq!(snapshot.source, "json_cache");
        assert_eq!(snapshot.stale_at_ms, snapshot.observed_at_ms);
        assert_eq!(snapshot.region.as_deref(), Some("us"));
    }

    #[test]
    fn workbuddy_cache_without_a_balance_keeps_a_redacted_valid_summary() {
        let snapshot = TauriObservationReader::snapshot_from_workbuddy_cache(
            "wb-account",
            "chat.work",
            None,
            NOW_MS - 60_000,
            None,
        )
        .unwrap();

        assert_eq!(snapshot.available_units, None);
        assert!(snapshot.summary.get("available").is_none());
        assert_eq!(snapshot.summary["status"], "stale");
    }
}
