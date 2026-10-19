use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Admission,
    Connect,
    Headers,
    Response,
    Stream,
    WsHandshake,
    WsSend,
    WsReceive,
    WsWait,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HealthScope {
    Provider,
    Model,
    Request,
}

/// Closed, non-sensitive evidence vocabulary shared by routing and diagnostics.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorBasis {
    QuotaExhausted,
    Rpm,
    Capacity,
    UsageLimit,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaVerdict {
    Exhausted,
    Available,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    Sub2Api,
    NewApi,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QuotaBasis {
    Wallet,
    Subscription,
    KeyQuota,
    SpendingWindow,
    Unlimited,
    Incomplete,
    QueryFailed,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuotaCheck {
    pub verdict: QuotaVerdict,
    pub source: QuotaSource,
    pub basis: QuotaBasis,
    pub checked_at: u64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RpmWaitReason {
    Pacing,
    SendWindow,
    CompletionWindow,
    UpstreamBackoff,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Details {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_basis: Option<ErrorBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_check: Option<QuotaCheck>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm_wait_reason: Option<RpmWaitReason>,
    #[serde(default)]
    pub health_scope: Option<HealthScope>,
    #[serde(default)]
    pub handoff_scope: Option<String>,
    pub upstream_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_reason: Option<String>,
    #[serde(default)]
    pub local_code: Option<String>,
    pub upstream_type: Option<String>,
    pub parameter: Option<String>,
    pub message: Option<String>,
    pub phase: Option<Phase>,
    pub ws_close_code: Option<u16>,
    pub counted_failure: Option<bool>,
    pub wait_seconds: Option<u64>,
    pub cause_id: Option<String>,
    pub circuit: Option<CircuitEvidence>,
    pub timeout_phase: Option<String>,
    pub timeout_seconds: Option<u64>,
    pub last_progress_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingDetails>,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RoutingOutcome {
    Attempted,
    OwnerPreserved,
    WindowConsumed,
    WindowReleased,
    BoundaryRegistered,
    OwnershipSaved,
    SaveFailed,
    Failed,
    #[serde(other)]
    Unknown,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoutingDetails {
    pub decision: crate::usage::model::RoutingDecision,
    pub evidence: crate::usage::model::RoutingEvidence,
    pub outcome: RoutingOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<crate::gateway::failover::Failure>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeliveryEvidence {
    pub duration_ms: u64,
    pub upstream_bytes: u64,
    /// Bytes handed to the HTTP/WS layer, not proof of client receipt.
    pub handed_bytes: u64,
    pub terminal_received: bool,
    pub terminal_handed_off: bool,
    pub request_hash: Option<String>,
    pub response_hash: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CircuitEvidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause_reason: Option<super::Reason>,
    /// Earliest permitted probe time, in Unix seconds; absent in old journals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<u64>,
    pub failures: u32,
    pub failure_threshold: u32,
    pub failed_requests: u32,
    pub requests: u32,
    pub error_rate: f64,
    pub min_requests: u32,
    pub trigger: String,
}
fn code(value: Option<String>) -> Option<String> {
    value.filter(|s| {
        s.len() <= 80
            && !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
            && !s.to_ascii_lowercase().contains("sk-")
            && !s.starts_with("eyJ")
            && !s
                .as_bytes()
                .windows(24)
                .any(|w| w.iter().all(u8::is_ascii_hexdigit))
    })
}

/// Error reasons are a closed vocabulary, never arbitrary upstream text.
pub(crate) fn safe_upstream_reason(value: &str) -> Option<String> {
    [
        "DAILY_LIMIT_EXCEEDED",
        "WEEKLY_LIMIT_EXCEEDED",
        "MONTHLY_LIMIT_EXCEEDED",
        "USER_PLATFORM_DAILY_QUOTA_EXHAUSTED",
        "USER_PLATFORM_WEEKLY_QUOTA_EXHAUSTED",
        "USER_PLATFORM_MONTHLY_QUOTA_EXHAUSTED",
        "GROUP_RPM_EXCEEDED",
        "USER_RPM_EXCEEDED",
        "USAGE_LIMIT_EXCEEDED",
        "insufficient_quota",
        "insufficient_balance",
        "insufficient_account_balance",
        "account_balance_insufficient",
        "api_key_quota_exhausted",
        "insufficient_user_quota",
        "balance_exhausted",
        "balance_not_enough",
        "credits_exhausted",
        "credit_limit_exceeded",
        "credit_balance_too_low",
        "billing_hard_limit_reached",
        "account_disabled",
        "account_deactivated",
        "invalid_api_key",
        "api_key_expired",
        "api_key_revoked",
        "api_key_disabled",
        "authentication_error",
        "account_suspended",
        "rate_limit_exceeded",
        "rate_limit_error",
        "overloaded_error",
        "model_capacity_exceeded",
        "server_is_overloaded",
        "slow_down",
        "usage_limit_reached",
    ]
    .into_iter()
    .find(|known| known.eq_ignore_ascii_case(value.trim()))
    .map(str::to_owned)
}

/// Error-only text: discard content-bearing errors, then mask identifiers.
pub fn safe_message(value: &str) -> Option<String> {
    if value.len() > 128 * 1024 {
        return None;
    }
    let lower = value.to_lowercase();
    if [
        "prompt",
        "request body",
        "request_body",
        "messages:",
        "messages=",
        "content:",
        "content=",
        "input:",
        "input=",
        "\"messages\"",
        "\"content\"",
        "\"input\"",
        "request payload",
        "query=",
        "<script",
        "<!doctype",
        "<html",
        "-----begin",
    ]
    .iter()
    .any(|s| lower.contains(s))
    {
        return None;
    }
    // Quoted/backticked fragments commonly echo a prompt, argument, or identifier.
    let mut scrubbed = String::new();
    let mut quote = None;
    let mut previous = None;
    for c in value.chars() {
        if let Some(end) = quote {
            if c == end {
                quote = None;
            }
            previous = Some(c);
            continue;
        }
        if matches!(c, '\"' | '`' | '“' | '「' | '‘')
            || (c == '\'' && previous.is_none_or(|p: char| !p.is_alphanumeric()))
        {
            quote = Some(match c {
                '“' => '”',
                '「' => '」',
                '‘' => '’',
                _ => c,
            });
            scrubbed.push_str("[已隐去]");
        } else {
            scrubbed.push(c);
        }
        previous = Some(c);
    }
    let mut result = Vec::new();
    let mut redact_next = false;
    for token in scrubbed.split_whitespace().take(160) {
        let s = token.trim_matches(|c: char| "\"'()[]{}<>,;".contains(c));
        let lower = s.to_lowercase();
        let credential = [
            "bearer",
            "authorization",
            "api_key",
            "apikey",
            "api-key",
            "password",
            "cookie",
            "id_token",
            "access_token",
            "refresh_token",
        ]
        .iter()
        .any(|v| lower.contains(v));
        let hidden = redact_next
            || credential
            || s.contains('@')
            || s.contains("://")
            || s.contains('/')
            || s.contains('\\')
            || s.contains("sk-")
            || s.starts_with("eyJ")
            || s.len() > 100
            || s.parse::<std::net::IpAddr>().is_ok()
            || s.split(':')
                .next()
                .is_some_and(|v| v.parse::<std::net::IpAddr>().is_ok())
            || s.rsplit_once('.').is_some_and(|(host, tld)| {
                !host.is_empty()
                    && (2..=24).contains(&tld.len())
                    && tld.chars().all(|c| c.is_ascii_alphabetic())
            })
            || [".com", ".net", ".org", ".xyz", ".invalid", ".cn", ".io"]
                .iter()
                .any(|v| lower.contains(v))
            || s.as_bytes()
                .windows(24)
                .any(|w| w.iter().all(u8::is_ascii_hexdigit));
        result.push(if hidden {
            "[已隐去]".into()
        } else {
            token
                .chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
        });
        redact_next = credential;
    }
    let result: String = result.join(" ").chars().take(512).collect();
    (!result.is_empty()).then_some(result)
}
impl Details {
    pub fn sanitized(mut self) -> Self {
        if let Some(routing) = &mut self.routing {
            routing.attempt_id = routing
                .attempt_id
                .take()
                .and_then(|v| uuid::Uuid::parse_str(&v).ok().map(|id| id.to_string()));
        }
        if let Some(evidence) = &mut self.delivery {
            let digest = |v: &String| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit());
            evidence.request_hash = evidence.request_hash.take().filter(digest);
            evidence.response_hash = evidence.response_hash.take().filter(digest);
        }
        self.handoff_scope = self
            .handoff_scope
            .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()));
        self.timeout_phase = self.timeout_phase.filter(|v| {
            matches!(
                v.as_str(),
                "response_headers"
                    | "first_byte"
                    | "first_event_idle"
                    | "error_body_idle"
                    | "stream_idle"
                    | "downstream_write"
                    | "ws_idle"
                    | "ws_first_byte"
                    | "ws_handshake"
            )
        });
        self.timeout_seconds = self.timeout_seconds.filter(|v| (1..=86400).contains(v));
        self.last_progress_ms = self.last_progress_ms.map(|v| v.min(86_400_000));
        self.upstream_code = code(self.upstream_code);
        self.upstream_reason = self
            .upstream_reason
            .as_deref()
            .and_then(safe_upstream_reason);
        self.local_code = code(self.local_code);
        self.upstream_type = code(self.upstream_type);
        self.parameter = self.parameter.filter(|v| {
            [
                "model",
                "input",
                "messages",
                "previous_response_id",
                "stream",
                "max_tokens",
            ]
            .contains(&v.as_str())
        });
        self.message = self.message.as_deref().and_then(safe_message);
        self.cause_id = self
            .cause_id
            .and_then(|v| uuid::Uuid::parse_str(&v).ok().map(|id| id.to_string()));
        self.ws_close_code = self.ws_close_code.filter(|v| (1000..=4999).contains(v));
        if let Some(c) = &mut self.circuit {
            c.retry_at = c.retry_at.filter(|v| (1..=253_402_300_799).contains(v));
            if ![
                "consecutive_failures",
                "transient_failures",
                "error_rate",
                "probe_failed",
            ]
            .contains(&c.trigger.as_str())
            {
                c.trigger = "consecutive_failures".into();
            }
        }
        self
    }
}

#[cfg(test)]
mod v016_tests {
    use super::*;
    #[test]
    fn error_details_are_bounded_and_do_not_keep_credentials_or_echoed_content() {
        let clean = safe_message("model not found; host https://example.invalid key Bearer fixture-secret quoted \"private fixture text\"").unwrap();
        assert!(!clean.contains("example.invalid"));
        assert!(!clean.contains("fixture-secret"));
        assert!(!clean.contains("private fixture text"));
        assert!(clean.contains("model not found"));
        assert!(safe_message("error request body: fixture").is_none());
        assert!(safe_message("bad input=fixture").is_none());
        assert!(!safe_message("invalid argument 'private fixture text'")
            .unwrap()
            .contains("private fixture text"));
        assert!(!safe_message("无效参数‘private fixture text’")
            .unwrap()
            .contains("private fixture text"));
        assert!(safe_message("model isn't available")
            .unwrap()
            .contains("isn't available"));
        assert!(!safe_message("failed at 192.0.2.1:443")
            .unwrap()
            .contains("192.0.2.1"));
        assert!(
            safe_message(&"模型不存在 ".repeat(200))
                .unwrap()
                .chars()
                .count()
                <= 512
        );
    }
    #[test]
    fn only_error_metadata_is_kept() {
        let d = Details {
            upstream_code: Some("model_not_found".into()),
            upstream_type: Some("invalid_request_error".into()),
            parameter: Some("model".into()),
            ws_close_code: Some(1013),
            message: Some("指定模型没有可用渠道".into()),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(d.upstream_code.as_deref(), Some("model_not_found"));
        assert_eq!(d.message.as_deref(), Some("指定模型没有可用渠道"));
        assert_eq!(d.ws_close_code, Some(1013));
        let bad = Details {
            upstream_code: Some("Bearer fixture".into()),
            parameter: Some("/tmp/fixture".into()),
            ..Default::default()
        }
        .sanitized();
        assert!(bad.upstream_code.is_none());
        assert!(bad.parameter.is_none());
    }
}
