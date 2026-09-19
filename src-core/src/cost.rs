use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostPolicy {
    pub id: String,
    pub endpoint: String,
    pub model_pattern: String,
    pub resource_kind: String,
    pub reserve_amount: i64,
    pub max_actual_amount: Option<i64>,
    pub version: i64,
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CostEstimate {
    pub policy_id: String,
    pub resource_kind: String,
    pub reserve_amount: i64,
    pub max_actual_amount: Option<i64>,
    pub policy_version: i64,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum CostError {
    #[error("no enabled cost policy matches endpoint {endpoint} and model {model}")]
    BudgetPolicyMissing { endpoint: String, model: String },
}

impl CostPolicy {
    pub fn estimate(&self, endpoint: &str, model: &str, _request: &Value) -> Result<CostEstimate, CostError> {
        if !self.enabled || self.endpoint != endpoint || !glob_matches(&self.model_pattern, model) {
            return Err(CostError::BudgetPolicyMissing {
                endpoint: endpoint.to_owned(),
                model: model.to_owned(),
            });
        }

        Ok(CostEstimate {
            policy_id: self.id.clone(),
            resource_kind: self.resource_kind.clone(),
            reserve_amount: self.reserve_amount,
            max_actual_amount: self.max_actual_amount,
            policy_version: self.version,
        })
    }
}

pub(crate) fn glob_matches(pattern: &str, value: &str) -> bool {
    let mut pattern_index = 0;
    let mut value_index = 0;
    let pattern = pattern.as_bytes();
    let value = value.as_bytes();
    let mut star = None;
    let mut retry_value = 0;

    while value_index < value.len() {
        if pattern_index < pattern.len() && pattern[pattern_index] == value[value_index] {
            pattern_index += 1;
            value_index += 1;
        } else if pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
            star = Some(pattern_index);
            pattern_index += 1;
            retry_value = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            retry_value += 1;
            value_index = retry_value;
        } else {
            return false;
        }
    }

    while pattern_index < pattern.len() && pattern[pattern_index] == b'*' {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}
