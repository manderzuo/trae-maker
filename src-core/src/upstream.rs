use serde_json::Value;

use crate::CoreError;

pub(crate) const MAX_OBSERVATION_SUMMARY_BYTES: usize = 4 * 1024;

pub(crate) fn validate_required(field: &str, value: &str) -> Result<(), CoreError> {
    if value.trim().is_empty() {
        return Err(CoreError::Validation {
            field: field.into(),
            reason: "must not be empty".into(),
        });
    }
    Ok(())
}

pub(crate) fn validate_observation_summary(summary: &Value) -> Result<String, CoreError> {
    let serialized = serde_json::to_string(summary)?;
    if serialized.len() > MAX_OBSERVATION_SUMMARY_BYTES {
        return Err(CoreError::Validation {
            field: "summary".into(),
            reason: format!("must not exceed {MAX_OBSERVATION_SUMMARY_BYTES} bytes"),
        });
    }
    Ok(serialized)
}
