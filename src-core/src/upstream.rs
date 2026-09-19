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

pub(crate) fn validate_opaque_credentials_ref(value: &str) -> Result<(), CoreError> {
    validate_required("credentials_ref", value)?;
    let Some((scheme, locator)) = value.split_once("://") else {
        return Err(CoreError::Validation {
            field: "credentials_ref".into(),
            reason: "must be an opaque vault or keychain reference".into(),
        });
    };
    if !matches!(
        scheme,
        "vault" | "keychain" | "keyring" | "credential" | "opaque" | "tauri" | "tauri-vault" | "legacy"
    ) || locator.is_empty()
        || locator.len() > 192
        || !locator.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '-' | '_' | '.' | '/')
        })
    {
        return Err(CoreError::Validation {
            field: "credentials_ref".into(),
            reason: "must be an opaque vault or keychain reference".into(),
        });
    }
    Ok(())
}

pub(crate) fn validate_observation_summary(summary: &Value) -> Result<String, CoreError> {
    let object = summary.as_object().ok_or_else(|| CoreError::Validation {
        field: "summary".into(),
        reason: "must be a redacted summary object".into(),
    })?;
    for (field, value) in object {
        let valid = match field.as_str() {
            "available" | "total" | "general" | "work" => value.is_number(),
            "expires_at_ms" | "observed_at_ms" | "value_scale" => value.as_i64().is_some(),
            "eligible" => value.is_boolean(),
            "provider" | "source" | "resource_kind" | "status" | "reason" | "observation_kind" => {
                value.as_str().is_some_and(valid_summary_label)
            }
            _ => false,
        };
        if !valid {
            return Err(CoreError::Validation {
                field: "summary".into(),
                reason: format!("contains a disallowed or non-redacted field: {field}"),
            });
        }
    }
    let serialized = serde_json::to_string(summary)?;
    if serialized.len() > MAX_OBSERVATION_SUMMARY_BYTES {
        return Err(CoreError::Validation {
            field: "summary".into(),
            reason: format!("must not exceed {MAX_OBSERVATION_SUMMARY_BYTES} bytes"),
        });
    }
    Ok(serialized)
}

fn valid_summary_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
}
