//! The literal conditions here are shared by classification and the rule directory.
use crate::{
    errors::code,
    events::ErrorBasis,
    storage::{AppError, Result},
};
use serde::{Deserialize, Serialize};
use ErrorBasis::*;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchKind {
    Code,
    Keyword,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Rule {
    pub id: String,
    pub match_kind: MatchKind,
    pub value: String,
    pub category: ErrorBasis,
}
impl Rule {
    pub fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.id).is_err()
            || self.value.trim().is_empty()
            || self.value.len() > 256
            || self.value != self.value.trim()
            || matches!(self.category, Unknown)
        {
            return Err(AppError::new(
                code::SETTINGS,
                "规则需要有效类别及非空的错误码或关键词",
            ));
        }
        Ok(())
    }
}
pub fn validate(rules: &[Rule]) -> Result<()> {
    for (i, rule) in rules.iter().enumerate() {
        rule.validate()?;
        if rules[..i].iter().any(|r| {
            r.id == rule.id
                || (r.match_kind == rule.match_kind
                    && r.value.to_lowercase() == rule.value.to_lowercase())
        }) {
            return Err(AppError::new(
                code::SETTINGS,
                "已有相同匹配条件，请编辑原规则",
            ));
        }
    }
    Ok(())
}
#[derive(Serialize)]
pub struct Code {
    pub value: &'static str,
    pub category: ErrorBasis,
}
pub(super) const CODES: &[Code] = &[
    Code {
        value: "credit_balance_exhausted",
        category: Billing,
    },
    Code {
        value: "organization_spend_limit_exceeded",
        category: Billing,
    },
    Code {
        value: "project_spend_limit_exceeded",
        category: Billing,
    },
    Code {
        value: "organization_usage_limit_exceeded",
        category: Billing,
    },
    Code {
        value: "insufficient_quota",
        category: Billing,
    },
    Code {
        value: "insufficient_balance",
        category: Billing,
    },
    Code {
        value: "insufficient_account_balance",
        category: Billing,
    },
    Code {
        value: "account_balance_insufficient",
        category: Billing,
    },
    Code {
        value: "insufficient_user_quota",
        category: Billing,
    },
    Code {
        value: "balance_exhausted",
        category: Billing,
    },
    Code {
        value: "balance_not_enough",
        category: Billing,
    },
    Code {
        value: "credits_exhausted",
        category: Billing,
    },
    Code {
        value: "credit_limit_exceeded",
        category: Billing,
    },
    Code {
        value: "credit_balance_too_low",
        category: Billing,
    },
    Code {
        value: "billing_hard_limit_reached",
        category: Billing,
    },
    Code {
        value: "billing_error",
        category: Billing,
    },
    Code {
        value: "enforced_spend_limit_reached",
        category: QuotaExhausted,
    },
    Code {
        value: "quota_exceeded",
        category: QuotaExhausted,
    },
    Code {
        value: "api_key_quota_exhausted",
        category: Billing,
    },
    Code {
        value: "invalid_api_key",
        category: Authentication,
    },
    Code {
        value: "api_key_expired",
        category: Authentication,
    },
    Code {
        value: "api_key_revoked",
        category: Authentication,
    },
    Code {
        value: "api_key_disabled",
        category: Authentication,
    },
    Code {
        value: "authentication_error",
        category: Authentication,
    },
    Code {
        value: "account_disabled",
        category: Authentication,
    },
    Code {
        value: "account_deactivated",
        category: Authentication,
    },
    Code {
        value: "account_suspended",
        category: Authentication,
    },
    Code {
        value: "permission_denied",
        category: Permission,
    },
    Code {
        value: "permission_error",
        category: Permission,
    },
    Code {
        value: "ip_not_authorized",
        category: Permission,
    },
    Code {
        value: "unsupported_country_region_territory",
        category: Permission,
    },
    Code {
        value: "tokens_per_minute_limit_exceeded",
        category: Tpm,
    },
    Code {
        value: "tpm_limit_exceeded",
        category: Tpm,
    },
    Code {
        value: "input_token_rate_limit_exceeded",
        category: InputTokens,
    },
    Code {
        value: "itpm_limit_exceeded",
        category: InputTokens,
    },
    Code {
        value: "output_token_rate_limit_exceeded",
        category: OutputTokens,
    },
    Code {
        value: "otpm_limit_exceeded",
        category: OutputTokens,
    },
    Code {
        value: "requests_per_second_limit_exceeded",
        category: Rps,
    },
    Code {
        value: "rps_limit_exceeded",
        category: Rps,
    },
    Code {
        value: "rpm_limit_exceeded",
        category: Rpm,
    },
    Code {
        value: "group_rpm_exceeded",
        category: Rpm,
    },
    Code {
        value: "user_rpm_exceeded",
        category: Rpm,
    },
    Code {
        value: "requests_per_minute_limit_exceeded",
        category: Rpm,
    },
    Code {
        value: "concurrency_limit_exceeded",
        category: Concurrency,
    },
    Code {
        value: "too_many_concurrent_requests",
        category: Concurrency,
    },
    Code {
        value: "connection_limit_exceeded",
        category: Concurrency,
    },
    Code {
        value: "slow_down",
        category: Acceleration,
    },
    Code {
        value: "acceleration_limit_exceeded",
        category: Acceleration,
    },
    Code {
        value: "spend_rate_limit_exceeded",
        category: SpendingRate,
    },
    Code {
        value: "spending_rate_limit_exceeded",
        category: SpendingRate,
    },
    Code {
        value: "usage_limit_exceeded",
        category: UsageLimit,
    },
    Code {
        value: "usage_limit_reached",
        category: UsageLimit,
    },
    Code {
        value: "overloaded_error",
        category: Capacity,
    },
    Code {
        value: "server_is_overloaded",
        category: Capacity,
    },
    Code {
        value: "model_capacity_exceeded",
        category: Capacity,
    },
    Code {
        value: "context_length_exceeded",
        category: RequestTooLarge,
    },
    Code {
        value: "request_too_large",
        category: RequestTooLarge,
    },
    Code {
        value: "max_tokens_exceeded",
        category: RequestTooLarge,
    },
    Code {
        value: "invalid_request",
        category: InvalidRequest,
    },
    Code {
        value: "invalid_request_error",
        category: InvalidRequest,
    },
    Code {
        value: "unsupported_endpoint",
        category: InvalidRequest,
    },
    Code {
        value: "unsupported_protocol",
        category: InvalidRequest,
    },
    Code {
        value: "conflict",
        category: InvalidRequest,
    },
    Code {
        value: "api_error",
        category: ServiceError,
    },
    Code {
        value: "server_error",
        category: ServiceError,
    },
    Code {
        value: "internal_error",
        category: ServiceError,
    },
    Code {
        value: "internal_server_error",
        category: ServiceError,
    },
    Code {
        value: "upstream_stream_read_error",
        category: ServiceError,
    },
    Code {
        value: "upstream_http2_stream_error",
        category: ServiceError,
    },
    Code {
        value: "content_filter",
        category: SafetyRejection,
    },
    Code {
        value: "content_policy_violation",
        category: SafetyRejection,
    },
    Code {
        value: "safety_error",
        category: SafetyRejection,
    },
    Code {
        value: "cyber_policy",
        category: SafetyRejection,
    },
];
pub(super) const MSG_0: &[&str] = &[
    "request too large",
    "request_too_large",
    "context_length_exceeded",
    "request exceeds the token limit",
    "maximum context length",
    "请求体过大",
    "上下文长度超限",
];
pub(super) const MSG_1: &[&str] = &["tokens", "token"];
pub(super) const MSG_2: &[&str] = &["single request exceeds"];
pub(super) const MSG_3: &[&str] = &[
    "content_policy_violation",
    "content policy",
    "safety policy",
    "cyber_policy",
    "安全策略拒绝",
];
pub(super) const MSG_4: &[&str] = &[
    "spend rate",
    "spending rate",
    "spend-based rate",
    "rolling 10-minute",
    "短时消费",
    "消费速率",
];
pub(super) const MSG_5: &[&str] = &[
    "specified api usage limits",
    "specified workspace api usage limits",
    "monthly api usage threshold",
];
pub(super) const MSG_6: &[&str] = &[
    "invalid_api_key",
    "invalid api key",
    "incorrect api key",
    "api_key_expired",
    "api_key_revoked",
    "api_key_disabled",
    "authentication_error",
    "account_disabled",
    "account_deactivated",
    "account_suspended",
    "account has been disabled",
    "api key is invalid",
    "api key has expired",
    "密钥无效",
    "密钥已过期",
    "密钥已撤销",
    "账号已禁用",
    "账户已禁用",
];
pub(super) const MSG_7: &[&str] = &[
    "ip not authorized",
    "ip allowlist",
    "country, region, or territory not supported",
    "project has been denied access",
];
pub(super) const MSG_8: &[&str] = &["input tokens per minute", "input token rate", "itpm"];
pub(super) const MSG_9: &[&str] = &["output tokens per minute", "output token rate", "otpm"];
pub(super) const MSG_10: &[&str] = &[
    "tokens per minute",
    "tokens-per-minute",
    "tokens_per_minute",
    "tpm",
    "每分钟 token",
    "每分钟token",
];
pub(super) const MSG_11: &[&str] = &[
    "concurrent requests",
    "concurrency limit",
    "concurrent connections",
    "too many connections",
    "并发数",
    "并发限制",
    "并发上限",
];
pub(super) const MSG_12: &[&str] = &[
    "requests per second",
    "requests-per-second",
    "rps_limit",
    "每秒请求",
];
pub(super) const MSG_13: &[&str] = &[
    "slow_down",
    "acceleration limit",
    "traffic increased too quickly",
    "流量增长过快",
];
pub(super) const MSG_14: &[&str] = &[
    "at capacity",
    "overload",
    "model_capacity",
    "server_is_overloaded",
    "容量",
    "繁忙",
    "负载过高",
];
pub(super) const MSG_15: &[&str] = &["usage_limit"];
pub(super) const MSG_16: &[&str] = &[
    "too many requests",
    "rate limit",
    "rate_limit",
    "resource_exhausted",
    "限流",
    "请求过多",
    "请求太多",
    "请求频率过高",
];
pub(super) const MODEL_ZH: &[&str] = &[
    "不存在",
    "未找到",
    "不支持",
    "无权",
    "没有权限",
    "未授权",
    "无权限",
    "无可用渠道",
    "没有可用渠道",
    "不可用渠道",
    "无可用通道",
    "无可用上游",
    "暂不可用",
    "未开通",
    "未启用",
    "不允许",
];
pub(super) const MODEL_EN: &[&str] = &[
    "does not exist",
    "not found",
    "not supported",
    "unsupported",
    "do not have access",
    "does not have access",
    "access denied",
    "don't have access",
    "no access to",
    "not allowed",
    "not permitted",
    "not available for",
    "not available to",
    "is not available",
    "no available channel",
    "no available provider",
    "no endpoints found",
    "no channel",
    "not enabled",
    "not authorized",
    "not entitled",
];
pub(super) const MODEL_CODES: &[&str] = &[
    "model_not_found",
    "unsupported_model",
    "model_not_supported",
    "model_not_available",
    "model_unavailable",
    "model_access_denied",
    "model_permission_denied",
    "invalid_model",
    "no_available_model",
    "model_not_exist",
    "model_not_exists",
];
pub(super) const BILLING_WORDS: &[&str] = &[
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
    "insufficient account balance",
    "insufficient credit balance",
    "account balance is insufficient",
    "credit balance exhausted",
    "balance has been exhausted",
    "run out of credits",
    "out of credit",
    "api key quota exhausted",
    "api key quota exceeded",
    "credit_balance_too_low",
    "billing_hard_limit_reached",
    "insufficient credits",
    "insufficient balance",
    "credit balance is too low",
    "余额不足",
    "余额已耗尽",
    "余额耗尽",
    "余额用尽",
    "余额已用完",
    "账户欠费",
    "账号欠费",
    "账户额度已耗尽",
    "账户额度已用完",
    "api key 额度已用完",
    "api key额度已用完",
    "令牌额度已用完",
    "令牌额度不足",
];
pub(super) const QUOTA_WORDS: &[&str] = &[
    "daily usage limit exceeded",
    "weekly usage limit exceeded",
    "monthly usage limit exceeded",
    "daily quota exhausted",
    "weekly quota exhausted",
    "monthly quota exhausted",
    "daily quota exceeded",
    "weekly quota exceeded",
    "monthly quota exceeded",
    "日额度已耗尽",
    "日额度已用尽",
    "日额度已用完",
    "每日额度已耗尽",
    "每日用量已达上限",
    "日用量已达上限",
    "每周额度已耗尽",
    "每月额度已耗尽",
    "周额度已耗尽",
    "周额度已用尽",
    "每周用量已达上限",
    "月额度已耗尽",
    "月额度已用尽",
    "每月用量已达上限",
];
pub(super) const QUOTA_NEGATIVES: &[&str] = &[
    "documentation:",
    "example:",
    "describes the",
    "not exceeded",
    "not exhausted",
    "not reached",
    "not yet",
    "未达到",
    "未耗尽",
    "尚未",
    "没有超",
    "未超",
    "并未",
];
pub(super) const RPM_WORDS: &[&str] = &[
    "group_rpm_exceeded",
    "user_rpm_exceeded",
    "requests-per-minute limit exceeded",
    "requests per minute limit exceeded",
    "requests per minute limit reached",
    "rpm limit exceeded",
    "rpm_limit_exceeded",
    "每分钟请求数超限",
    "每分钟请求次数已达上限",
];
pub(super) const RPM_NEGATIVES: &[&str] = &[
    "not an rpm",
    "not a requests",
    "not exceeded",
    "not reached",
    "not been reached",
    "not yet",
    "documentation:",
    "example:",
    "未达到",
    "尚未",
    "没有超",
    "未超",
];
pub(super) const PERMANENT_CODES: &[&str] = &[
    "DAILY_LIMIT_EXCEEDED",
    "WEEKLY_LIMIT_EXCEEDED",
    "MONTHLY_LIMIT_EXCEEDED",
    "USER_PLATFORM_DAILY_QUOTA_EXHAUSTED",
    "USER_PLATFORM_WEEKLY_QUOTA_EXHAUSTED",
    "USER_PLATFORM_MONTHLY_QUOTA_EXHAUSTED",
];

pub(super) const STATUS_CODES: &[Code] = &[
    Code {
        value: "RESOURCE_EXHAUSTED",
        category: UnknownRateLimit,
    },
    Code {
        value: "UNAUTHENTICATED",
        category: Authentication,
    },
    Code {
        value: "PERMISSION_DENIED",
        category: Permission,
    },
    Code {
        value: "INVALID_ARGUMENT",
        category: InvalidRequest,
    },
    Code {
        value: "FAILED_PRECONDITION",
        category: InvalidRequest,
    },
    Code {
        value: "INTERNAL",
        category: ServiceError,
    },
    Code {
        value: "UNAVAILABLE",
        category: ServiceError,
    },
    Code {
        value: "DEADLINE_EXCEEDED",
        category: ServiceError,
    },
];
pub(super) const LIMIT_HEADERS: &[(&str, ErrorBasis)] = &[
    ("anthropic-ratelimit-input-tokens-remaining", InputTokens),
    ("anthropic-ratelimit-output-tokens-remaining", OutputTokens),
    ("x-ratelimit-remaining-tokens", Tpm),
    ("anthropic-ratelimit-tokens-remaining", Tpm),
    ("x-ratelimit-remaining-project-tokens", Tpm),
    ("x-ratelimit-remaining-requests", Rpm),
    ("anthropic-ratelimit-requests-remaining", Rpm),
];
