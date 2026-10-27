//! Read-only lookup using the same classifier as live requests.
use super::{failure_internal, failure_policy, failure_rules, Gateway};
use crate::{
    errors::code,
    events::ErrorBasis,
    storage::{AppError, Result},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Input {
    pub query: Option<String>,
    pub raw_response: Option<String>,
    pub headers: Option<Vec<crate::event_raw::Header>>,
    pub event: Option<String>,
    pub event_id: Option<String>,
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
    ConnectSeconds,
    FirstByteSeconds,
    IdleSeconds,
    WebsocketRetrySeconds,
    QueueSeconds,
    MaxWaiting,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum CatalogEntry {
    Upstream(failure_rules::Category),
    Gateway(failure_internal::Policy),
}
pub fn catalog() -> Vec<CatalogEntry> {
    failure_rules::catalog()
        .into_iter()
        .map(CatalogEntry::Upstream)
        .chain(
            failure_internal::catalog()
                .into_iter()
                .map(CatalogEntry::Gateway),
        )
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Match {
    pub origin: Option<crate::events::Origin>,
    pub policy_id: Option<&'static str>,
    pub label: Option<&'static str>,
    pub parsed: Input,
    pub candidates: Vec<Candidate>,
    pub category: Option<ErrorBasis>,
    pub source: Option<Source>,
    pub evidence: String,
    pub action: Option<&'static str>,
    pub custom_rule_id: Option<String>,
    pub conflict: bool,
    pub parameters: Vec<Parameter>,
    pub common_parameters: Vec<Parameter>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub category: ErrorBasis,
    pub label: &'static str,
    pub action: &'static str,
    pub parameters: Vec<Parameter>,
    pub common_parameters: Vec<Parameter>,
}

fn split_query(text: &str) -> (&str, &str) {
    text.split_once(|c: char| c.is_whitespace() || c == ':' || c == '：')
        .map(|(head, tail)| {
            (
                head,
                tail.trim_start_matches(|c: char| {
                    c.is_whitespace() || c == ':' || c == '：' || c == '-'
                }),
            )
        })
        .unwrap_or((text, ""))
}

impl Input {
    fn parsed(&self, rules: &[failure_rules::Rule]) -> Result<(Self, Value, bool)> {
        let mut parsed = self.clone();
        let query = self.query.as_deref().unwrap_or("").trim();
        let text = query
            .strip_prefix("HTTP/")
            .and_then(|v| v.split_once(' ').map(|(_, tail)| tail))
            .or_else(|| query.strip_prefix("HTTP "))
            .unwrap_or(query);
        let (head, tail) = split_query(text);
        let status = head.parse::<u16>().ok().filter(|v| (100..=599).contains(v));
        let text = if self.raw_response.is_none() {
            if let Some(status) = status {
                parsed.status.get_or_insert(status);
                tail
            } else {
                text
            }
        } else {
            text
        };
        let raw = self.raw_response.as_deref().or_else(|| {
            (text.starts_with('{') || text.starts_with("data:") || text.starts_with("event:"))
                .then_some(text)
        });
        let explicit = raw.is_some();
        let mut value = if let Some(raw) = raw {
            serde_json::from_str(raw).unwrap_or_else(|_| {
                let data = raw
                    .lines()
                    .filter_map(|line| {
                        line.strip_prefix("data:")
                            .map(|v| v.strip_prefix(' ').unwrap_or(v))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                serde_json::from_str(&data).unwrap_or_else(|_| {
                    if data.is_empty() {
                        Value::String(raw.into())
                    } else {
                        json!({"message":data})
                    }
                })
            })
        } else {
            Value::Null
        };
        if raw.is_none() && !text.is_empty() {
            let (head, tail) = split_query(text);
            let code_like = !head.is_empty()
                && head
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
                && (tail.is_empty()
                    || head.contains('_')
                    || head.contains('.')
                    || head.chars().all(|c| !c.is_ascii_lowercase())
                    || text[head.len()..].starts_with(':')
                    || text[head.len()..].starts_with('：')
                    || rules.iter().any(|rule| {
                        rule.match_kind == failure_rules::MatchKind::Code
                            && rule.value.eq_ignore_ascii_case(head)
                    }));
            if code_like {
                parsed.code.get_or_insert_with(|| head.into());
                // A lone token may be a message; the classifier still checks codes first.
                parsed
                    .message
                    .get_or_insert_with(|| if tail.is_empty() { head } else { tail }.into());
            } else {
                parsed.message.get_or_insert_with(|| text.into());
            }
        }
        if parsed.status.is_some_and(|s| !(100..=599).contains(&s)) {
            return Err(AppError::new(code::SETTINGS, "HTTP 状态应为 100–599"));
        }
        let mut error = serde_json::Map::new();
        for (key, input) in [
            ("code", &parsed.code),
            ("message", &parsed.message),
            ("type", &parsed.upstream_type),
            ("reason", &parsed.upstream_reason),
            ("param", &parsed.parameter),
        ] {
            if let Some(input) = input.as_deref() {
                error.insert(
                    key.into(),
                    if input.is_empty() {
                        Value::Null
                    } else {
                        Value::String(input.into())
                    },
                );
            }
        }
        if !error.is_empty() {
            if explicit {
                if let Some(envelope) =
                    failure_policy::envelope(&value, parsed.status.is_some_and(|s| s >= 400), 0)
                {
                    let corrected = if let Some(message) = envelope.as_str() {
                        json!({"message":message})
                    } else {
                        envelope.clone()
                    };
                    value = json!({"error":corrected});
                }
                let target = if value.get("error").is_some_and(Value::is_object) {
                    value.get_mut("error").unwrap()
                } else {
                    &mut value
                };
                if let Some(target) = target.as_object_mut() {
                    target.extend(error);
                }
            } else {
                value = json!({"error": error});
            }
        } else if value.is_null() {
            value = json!({});
        }
        if let Some(event) = &parsed.event {
            if let Some(map) = value.as_object_mut() {
                map.entry("type").or_insert(Value::String(event.clone()));
            }
        }
        Ok((parsed, value, explicit))
    }
}

pub(super) fn lookup(input: &Input, rules: &[failure_rules::Rule]) -> Result<Match> {
    let (mut parsed, value, explicit) = input.parsed(rules)?;
    let envelope = failure_policy::envelope(&value, parsed.status.is_some_and(|s| s >= 400), 0)
        .unwrap_or(&value);
    let gateway_envelope =
        envelope.get("type").and_then(Value::as_str) == Some("gpt_switch_gateway");
    let local_code = parsed
        .code
        .as_deref()
        .or_else(|| envelope.get("code").and_then(Value::as_str));
    let message = parsed
        .message
        .as_deref()
        .or_else(|| envelope.get("message").and_then(Value::as_str))
        .unwrap_or("");
    let lower_message = message.to_lowercase();
    let local = failure_internal::catalog().into_iter().find(|p| {
        (gateway_envelope || !explicit)
            && (local_code
                .is_some_and(|code| p.codes.iter().any(|c| c.value.eq_ignore_ascii_case(code)))
                || (!explicit
                    && p.conditions
                        .iter()
                        .any(|condition| lower_message.contains(&condition.to_lowercase()))))
    });
    if gateway_envelope || local.is_some() {
        parsed.code = local_code.map(str::to_owned);
        parsed.message = (!message.is_empty()).then(|| message.to_owned());
        parsed.raw_response = None;
        parsed.headers = None;
        return Ok(Match {
            origin: Some(crate::events::Origin::Gateway),
            policy_id: local.as_ref().map(|p| p.policy_id),
            label: local.as_ref().map(|p| p.label),
            evidence: parsed
                .code
                .clone()
                .or_else(|| parsed.message.clone())
                .unwrap_or_default(),
            parsed,
            candidates: vec![],
            category: None,
            source: local.as_ref().map(|_| Source::BuiltinCondition),
            action: local.as_ref().map(|p| p.action),
            custom_rule_id: None,
            conflict: false,
            parameters: local.map(|p| p.parameters).unwrap_or_default(),
            common_parameters: vec![],
        });
    }
    let headers = crate::event_raw::header_map(parsed.headers.as_deref().unwrap_or_default());
    let catalog = failure_rules::catalog();
    let (mut details, mut source, mut evidence) =
        failure_policy::inspect_match_headers(&value, parsed.status, &headers, rules, &catalog);
    // A free-form search is not proof of a failed upstream request.
    if !explicit
        && details.error_basis == Some(ErrorBasis::ServiceError)
        && details.custom_rule_id.is_none()
        && source == Some(Source::Status)
        && parsed.status.is_none_or(|s| s < 400)
    {
        details.error_basis = None;
        source = None;
        evidence.clear();
    }
    if explicit
        && details.error_basis == Some(ErrorBasis::ServiceError)
        && source == Some(Source::Status)
        && parsed.status.is_none_or(|s| s < 400)
    {
        source = Some(Source::BuiltinCondition);
        evidence = "明确的业务失败，原因未细分".into();
    }
    parsed.code = details.upstream_code.clone().or(parsed.code).or_else(|| {
        value.get("code").and_then(|code| {
            code.as_str()
                .map(str::to_owned)
                .or_else(|| code.as_i64().map(|v| v.to_string()))
        })
    });
    parsed.message = details.message.clone().or(parsed.message);
    parsed.upstream_type = details.upstream_type.clone().or(parsed.upstream_type);
    parsed.upstream_reason = details.upstream_reason.clone().or(parsed.upstream_reason);
    parsed.parameter = details.parameter.clone().or(parsed.parameter);
    let category = details.error_basis.filter(|b| *b != ErrorBasis::Unknown);
    let (selected_parameters, common_parameters) = parameters(parsed.status, category);
    let query = input.query.as_deref().unwrap_or("").trim().to_lowercase();
    let broad = parsed.status.is_some()
        && !explicit
        && parsed.code.is_none()
        && parsed.message.is_none()
        && parsed.upstream_type.is_none()
        && parsed.upstream_reason.is_none();
    let candidates = catalog
        .iter()
        .filter(|c| {
            broad
                || Some(c.category) == category
                || (!query.is_empty()
                    && !explicit
                    && (c.label.to_lowercase().contains(&query)
                        || c.action.to_lowercase().contains(&query)
                        || c.codes
                            .iter()
                            .any(|code| code.value.to_lowercase().contains(&query))
                        || c.keywords
                            .iter()
                            .flat_map(|k| &k.all_of)
                            .flatten()
                            .any(|word| word.to_lowercase().contains(&query))
                        || c.conditions
                            .iter()
                            .any(|condition| condition.to_lowercase().contains(&query))
                        || rules.iter().any(|r| {
                            r.category == c.category && r.value.to_lowercase().contains(&query)
                        })))
        })
        .map(|c| {
            let (parameters, common_parameters) = parameters(parsed.status, Some(c.category));
            Candidate {
                category: c.category,
                label: c.label,
                action: c.action,
                parameters,
                common_parameters,
            }
        })
        .collect();
    let action = catalog
        .iter()
        .find(|c| Some(c.category) == category)
        .map(|c| c.action);
    // Do not echo the whole attachment in search results.
    parsed.raw_response = None;
    parsed.headers = None;
    Ok(Match {
        origin: category.map(|_| crate::events::Origin::Upstream),
        policy_id: None,
        label: catalog
            .iter()
            .find(|c| Some(c.category) == category)
            .map(|c| c.label),
        parsed,
        candidates,
        category,
        source,
        evidence,
        action,
        custom_rule_id: details.custom_rule_id,
        conflict: details.rule_conflict == Some(true),
        parameters: selected_parameters,
        common_parameters,
    })
}

fn parameters(
    status: Option<u16>,
    category: Option<ErrorBasis>,
) -> (Vec<Parameter>, Vec<Parameter>) {
    let details = crate::events::Details {
        error_basis: category,
        ..Default::default()
    };
    let policy = failure_policy::decision(status, &details);
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
    (parameters, common_parameters)
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
