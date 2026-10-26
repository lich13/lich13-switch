//! Read-only lookup using the same classifier as live requests.
use super::{failure_policy, failure_rules, Gateway};
use crate::{
    errors::code,
    events::ErrorBasis,
    storage::{AppError, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    pub code: Option<String>,
    pub message: Option<String>,
    pub status: Option<u16>,
    pub upstream_type: Option<String>,
    pub upstream_reason: Option<String>,
    pub parameter: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    BuiltinCode,
    BuiltinType,
    BuiltinMessage,
    BuiltinCondition,
    CustomCode,
    CustomKeyword,
    Status,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Parameter {
    CapacityRetrySeconds,
    RateLimitSeconds,
    HardFailureCooldownSeconds,
    MaxRetries,
    RetryBudgetSeconds,
    FailureThreshold,
    TransientFailureThreshold,
    SuccessThreshold,
    CooldownSeconds,
    ErrorRate,
    MinRequests,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Match {
    pub category: Option<ErrorBasis>,
    pub source: Option<Source>,
    pub evidence: String,
    pub action: Option<&'static str>,
    pub custom_rule_id: Option<String>,
    pub conflict: bool,
    pub parameters: Vec<Parameter>,
    pub common_parameters: Vec<Parameter>,
}

impl Input {
    fn value(&self) -> Result<Value> {
        if self.status.is_some_and(|s| !(100..=599).contains(&s)) {
            return Err(AppError::new(code::SETTINGS, "HTTP 状态应为 100–599"));
        }
        let mut error = serde_json::Map::new();
        for (key, value) in [
            ("code", &self.code),
            ("message", &self.message),
            ("type", &self.upstream_type),
            ("reason", &self.upstream_reason),
            ("param", &self.parameter),
        ] {
            if let Some(value) = value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                error.insert(key.into(), Value::String(value.into()));
            }
        }
        let value = json!({"error": error});
        if value.to_string().len() > 128 * 1024 {
            return Err(AppError::new(code::SETTINGS, "报错信息过长，请缩短后重试"));
        }
        Ok(value)
    }
}

pub(super) fn lookup(input: &Input, rules: &[failure_rules::Rule]) -> Result<Match> {
    let value = input.value()?;
    let (details, source, evidence) = failure_policy::inspect_match(&value, input.status, rules);
    let policy = failure_policy::decision(input.status, &details);
    let category = details.error_basis.filter(|b| *b != ErrorBasis::Unknown);
    use Parameter::*;
    let parameters = if category == Some(ErrorBasis::Capacity) {
        vec![CapacityRetrySeconds]
    } else if policy.temporary {
        vec![RateLimitSeconds]
    } else if policy.hard {
        vec![HardFailureCooldownSeconds]
    } else if policy.counted_failure && category.is_some() {
        vec![
            MaxRetries,
            TransientFailureThreshold,
            FailureThreshold,
            CooldownSeconds,
            ErrorRate,
            MinRequests,
            SuccessThreshold,
        ]
    } else {
        vec![]
    };
    let common_parameters = if policy.temporary {
        vec![RetryBudgetSeconds]
    } else if policy.retryable && category.is_some() {
        if parameters.contains(&MaxRetries) {
            vec![RetryBudgetSeconds]
        } else {
            vec![MaxRetries, RetryBudgetSeconds]
        }
    } else {
        vec![]
    };
    let action = failure_rules::catalog()
        .into_iter()
        .find(|c| Some(c.category) == category)
        .map(|c| c.action);
    Ok(Match {
        category,
        source,
        evidence,
        action,
        custom_rule_id: details.custom_rule_id,
        conflict: details.rule_conflict == Some(true),
        parameters,
        common_parameters,
    })
}

impl Gateway {
    pub fn match_failure_policy(&self, input: Input) -> Result<Match> {
        let rules = self
            .request_state()?
            .store
            .settings
            .custom_failure_rules
            .clone();
        lookup(&input, &rules)
    }
}
