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
            || crate::events::safe_message(&self.value).as_deref() != Some(self.value.as_str())
            || matches!(self.category, Unknown)
        {
            return Err(AppError::new(
                code::SETTINGS,
                "规则需要有效类别及不含敏感信息的错误码或关键词",
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
    pub platform: &'static str,
}
pub(super) const CODES: &[Code] = &[
    Code {
        value: "credit_balance_exhausted",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "organization_spend_limit_exceeded",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "project_spend_limit_exceeded",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "organization_usage_limit_exceeded",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "insufficient_quota",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "insufficient_balance",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "insufficient_account_balance",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "account_balance_insufficient",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "insufficient_user_quota",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "balance_exhausted",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "balance_not_enough",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "credits_exhausted",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "credit_limit_exceeded",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "credit_balance_too_low",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "billing_hard_limit_reached",
        category: Billing,
        platform: "OpenAI",
    },
    Code {
        value: "billing_error",
        category: Billing,
        platform: "通用兼容",
    },
    Code {
        value: "enforced_spend_limit_reached",
        category: QuotaExhausted,
        platform: "Claude",
    },
    Code {
        value: "quota_exceeded",
        category: QuotaExhausted,
        platform: "通用兼容",
    },
    Code {
        value: "api_key_quota_exhausted",
        category: QuotaExhausted,
        platform: "通用兼容",
    },
    Code {
        value: "invalid_api_key",
        category: Authentication,
        platform: "OpenAI",
    },
    Code {
        value: "api_key_expired",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "api_key_revoked",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "api_key_disabled",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "authentication_error",
        category: Authentication,
        platform: "Claude",
    },
    Code {
        value: "account_disabled",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "account_deactivated",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "account_suspended",
        category: Authentication,
        platform: "通用兼容",
    },
    Code {
        value: "permission_denied",
        category: Permission,
        platform: "通用兼容",
    },
    Code {
        value: "permission_error",
        category: Permission,
        platform: "Claude",
    },
    Code {
        value: "ip_not_authorized",
        category: Permission,
        platform: "通用兼容",
    },
    Code {
        value: "unsupported_country_region_territory",
        category: Permission,
        platform: "通用兼容",
    },
    Code {
        value: "tokens_per_minute_limit_exceeded",
        category: Tpm,
        platform: "通用兼容",
    },
    Code {
        value: "tpm_limit_exceeded",
        category: Tpm,
        platform: "通用兼容",
    },
    Code {
        value: "input_token_rate_limit_exceeded",
        category: InputTokens,
        platform: "通用兼容",
    },
    Code {
        value: "itpm_limit_exceeded",
        category: InputTokens,
        platform: "通用兼容",
    },
    Code {
        value: "output_token_rate_limit_exceeded",
        category: OutputTokens,
        platform: "通用兼容",
    },
    Code {
        value: "otpm_limit_exceeded",
        category: OutputTokens,
        platform: "通用兼容",
    },
    Code {
        value: "requests_per_second_limit_exceeded",
        category: Rps,
        platform: "通用兼容",
    },
    Code {
        value: "rps_limit_exceeded",
        category: Rps,
        platform: "通用兼容",
    },
    Code {
        value: "rpm_limit_exceeded",
        category: Rpm,
        platform: "通用兼容",
    },
    Code {
        value: "group_rpm_exceeded",
        category: Rpm,
        platform: "通用兼容",
    },
    Code {
        value: "user_rpm_exceeded",
        category: Rpm,
        platform: "通用兼容",
    },
    Code {
        value: "requests_per_minute_limit_exceeded",
        category: Rpm,
        platform: "通用兼容",
    },
    Code {
        value: "concurrency_limit_exceeded",
        category: Concurrency,
        platform: "通用兼容",
    },
    Code {
        value: "too_many_concurrent_requests",
        category: Concurrency,
        platform: "通用兼容",
    },
    Code {
        value: "connection_limit_exceeded",
        category: Concurrency,
        platform: "通用兼容",
    },
    Code {
        value: "slow_down",
        category: Acceleration,
        platform: "通用兼容",
    },
    Code {
        value: "acceleration_limit_exceeded",
        category: Acceleration,
        platform: "通用兼容",
    },
    Code {
        value: "spend_rate_limit_exceeded",
        category: SpendingRate,
        platform: "通用兼容",
    },
    Code {
        value: "spending_rate_limit_exceeded",
        category: SpendingRate,
        platform: "通用兼容",
    },
    Code {
        value: "usage_limit_exceeded",
        category: UsageLimit,
        platform: "通用兼容",
    },
    Code {
        value: "usage_limit_reached",
        category: UsageLimit,
        platform: "通用兼容",
    },
    Code {
        value: "overloaded_error",
        category: Capacity,
        platform: "Claude",
    },
    Code {
        value: "server_is_overloaded",
        category: Capacity,
        platform: "通用兼容",
    },
    Code {
        value: "model_capacity_exceeded",
        category: Capacity,
        platform: "通用兼容",
    },
    Code {
        value: "context_length_exceeded",
        category: RequestTooLarge,
        platform: "OpenAI",
    },
    Code {
        value: "request_too_large",
        category: RequestTooLarge,
        platform: "Claude",
    },
    Code {
        value: "max_tokens_exceeded",
        category: RequestTooLarge,
        platform: "通用兼容",
    },
    Code {
        value: "invalid_request",
        category: InvalidRequest,
        platform: "通用兼容",
    },
    Code {
        value: "invalid_request_error",
        category: InvalidRequest,
        platform: "通用兼容",
    },
    Code {
        value: "unsupported_endpoint",
        category: InvalidRequest,
        platform: "通用兼容",
    },
    Code {
        value: "unsupported_protocol",
        category: InvalidRequest,
        platform: "通用兼容",
    },
    Code {
        value: "conflict",
        category: InvalidRequest,
        platform: "通用兼容",
    },
    Code {
        value: "api_error",
        category: ServiceError,
        platform: "Claude",
    },
    Code {
        value: "server_error",
        category: ServiceError,
        platform: "通用兼容",
    },
    Code {
        value: "internal_error",
        category: ServiceError,
        platform: "通用兼容",
    },
    Code {
        value: "internal_server_error",
        category: ServiceError,
        platform: "通用兼容",
    },
    Code {
        value: "upstream_stream_read_error",
        category: ServiceError,
        platform: "通用兼容",
    },
    Code {
        value: "upstream_http2_stream_error",
        category: ServiceError,
        platform: "通用兼容",
    },
    Code {
        value: "content_filter",
        category: SafetyRejection,
        platform: "通用兼容",
    },
    Code {
        value: "content_policy_violation",
        category: SafetyRejection,
        platform: "OpenAI",
    },
    Code {
        value: "safety_error",
        category: SafetyRejection,
        platform: "通用兼容",
    },
    Code {
        value: "cyber_policy",
        category: SafetyRejection,
        platform: "通用兼容",
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
pub(super) const MSG_2: &[&str] = &["request too large", "single request exceeds"];
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
    "authentication_error",
    "account_disabled",
    "account_deactivated",
    "account_suspended",
    "account has been disabled",
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
    "account_disabled",
    "account_deactivated",
    "insufficient credits",
    "insufficient balance",
    "credit balance is too low",
    "account has been disabled",
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
    "账号已禁用",
    "账户已禁用",
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KeywordCondition {
    pub all_of: Vec<Vec<&'static str>>,
    pub exact: bool,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Category {
    pub category: ErrorBasis,
    pub label: &'static str,
    pub group: &'static str,
    pub action: &'static str,
    pub codes: Vec<&'static Code>,
    pub keywords: Vec<KeywordCondition>,
    pub protocols: [&'static str; 3],
    pub excluded_keywords: Vec<&'static str>,
    pub conditions: Vec<String>,
    pub statuses: Vec<&'static str>,
}
pub(super) const STATUS_CODES: &[Code] = &[
    Code {
        value: "RESOURCE_EXHAUSTED",
        category: UnknownRateLimit,
        platform: "Gemini",
    },
    Code {
        value: "UNAUTHENTICATED",
        category: Authentication,
        platform: "Gemini",
    },
    Code {
        value: "PERMISSION_DENIED",
        category: Permission,
        platform: "Gemini",
    },
    Code {
        value: "INVALID_ARGUMENT",
        category: InvalidRequest,
        platform: "Gemini",
    },
    Code {
        value: "FAILED_PRECONDITION",
        category: InvalidRequest,
        platform: "Gemini",
    },
    Code {
        value: "INTERNAL",
        category: ServiceError,
        platform: "Gemini",
    },
    Code {
        value: "UNAVAILABLE",
        category: ServiceError,
        platform: "Gemini",
    },
    Code {
        value: "DEADLINE_EXCEEDED",
        category: ServiceError,
        platform: "Gemini",
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
pub fn catalog() -> Vec<Category> {
    let mut result = Vec::new();
    for (category, label, group, action) in [
        (Rpm, "请求频率 RPM", "限流与容量", "按请求窗口等待"),
        (Rps, "请求频率 RPS", "限流与容量", "按请求窗口等待"),
        (Tpm, "Token 速率", "限流与容量", "按 Token 窗口等待"),
        (
            InputTokens,
            "输入 Token 速率",
            "限流与容量",
            "按输入窗口等待",
        ),
        (
            OutputTokens,
            "输出 Token 速率",
            "限流与容量",
            "按输出窗口等待",
        ),
        (Concurrency, "并发与连接数", "限流与容量", "退避后重新准入"),
        (Acceleration, "突发与增长过快", "限流与容量", "渐进退避"),
        (SpendingRate, "短时消费速率", "限流与容量", "按短时窗口等待"),
        (Capacity, "容量不足", "限流与容量", "已有会话优先原供应商"),
        (
            UnknownRateLimit,
            "未分类限流",
            "限流与容量",
            "在总预算内退避",
        ),
        (UsageLimit, "待核验用量限制", "限流与容量", "核验额度并等待"),
        (
            QuotaExhausted,
            "周期额度耗尽",
            "额度与认证",
            "跳过候选，到期按需探测",
        ),
        (
            Billing,
            "余额与消费硬上限",
            "额度与认证",
            "跳过候选，冷却后探测",
        ),
        (
            Authentication,
            "认证失败",
            "额度与认证",
            "跳过供应商，检查凭据",
        ),
        (Permission, "权限与访问限制", "额度与认证", "隔离受影响范围"),
        (
            ModelUnavailable,
            "模型不可用",
            "额度与认证",
            "仅隔离对应模型",
        ),
        (ServiceError, "服务异常", "服务与网络故障", "有限重试与熔断"),
        (
            InvalidRequest,
            "请求或协议错误",
            "服务与网络故障",
            "返回原因",
        ),
        (
            RequestTooLarge,
            "单次请求超限",
            "服务与网络故障",
            "返回原因，不缩减请求",
        ),
        (
            SafetyRejection,
            "安全拒绝",
            "服务与网络故障",
            "返回原因，不换商绕过",
        ),
    ] {
        let any = |words: &[&'static str]| KeywordCondition {
            all_of: vec![words.to_vec()],
            exact: false,
        };
        let mut keywords = match category {
            Rpm => vec![any(RPM_WORDS)],
            Rps => vec![any(MSG_12)],
            Tpm => vec![any(MSG_10)],
            InputTokens => vec![any(MSG_8)],
            OutputTokens => vec![any(MSG_9)],
            Concurrency => vec![any(MSG_11)],
            Acceleration => vec![any(MSG_13)],
            SpendingRate => vec![any(MSG_4)],
            Capacity => vec![
                any(MSG_14),
                KeywordCondition {
                    all_of: vec![vec!["capacity"]],
                    exact: true,
                },
            ],
            UnknownRateLimit => vec![any(MSG_16)],
            UsageLimit => vec![any(MSG_15)],
            QuotaExhausted => vec![any(QUOTA_WORDS), any(MSG_5)],
            Billing => vec![any(BILLING_WORDS)],
            Authentication => vec![any(MSG_6)],
            Permission => vec![any(MSG_7)],
            ModelUnavailable => vec![
                KeywordCondition {
                    all_of: vec![vec!["model"], MODEL_EN.to_vec()],
                    exact: false,
                },
                KeywordCondition {
                    all_of: vec![vec!["模型"], MODEL_ZH.to_vec()],
                    exact: false,
                },
            ],
            RequestTooLarge => vec![
                any(MSG_0),
                KeywordCondition {
                    all_of: vec![MSG_1.to_vec(), MSG_2.to_vec()],
                    exact: false,
                },
            ],
            SafetyRejection => vec![any(MSG_3)],
            _ => vec![],
        };
        if category == QuotaExhausted {
            keywords.push(KeywordCondition {
                all_of: vec![PERMANENT_CODES.to_vec()],
                exact: true,
            });
        }
        if category == ModelUnavailable {
            keywords.push(KeywordCondition {
                all_of: vec![MODEL_CODES.to_vec()],
                exact: true,
            });
        }
        let mut conditions: Vec<_> = LIMIT_HEADERS
            .iter()
            .filter(|(_, basis)| *basis == category)
            .map(|(name, _)| format!("{name} = 0"))
            .collect();
        conditions.extend(
            (match category {
                Rpm => vec![
                    "Gemini QuotaFailure：quotaId / quotaMetric 含 request 与 perminute",
                    "兼容网关：完整的分钟请求限制消息，次数与分钟数均为正整数",
                ],
                Rps => vec!["Gemini QuotaFailure：quotaId / quotaMetric 含 persecond"],
                InputTokens => vec!["Gemini QuotaFailure：指标同时含 input 与 token"],
                OutputTokens => vec!["Gemini QuotaFailure：指标同时含 output 与 token"],
                Tpm => vec!["Gemini QuotaFailure：指标含 token，且未命中输入、输出或周期额度条件"],
                QuotaExhausted => vec![
                    "Claude：details.error_code = enforced_spend_limit_reached",
                    "Gemini QuotaFailure：quotaValue = 0，或指标含 perday / perweek / permonth",
                ],
                SpendingRate => vec!["Gemini QuotaFailure：quotaId / quotaMetric 含 spend"],
                ModelUnavailable => vec![
                    "Claude：type = not_found_error 且 message 以 model: 开头",
                    "排除明确的容量或账号故障",
                ],
                UnknownRateLimit => vec![
                    "Claude：rate_limit_error 需要进一步判断具体原因",
                    "Gemini：RESOURCE_EXHAUSTED 需要进一步判断具体原因",
                ],
                Permission => vec!["param = model 或明确模型证据时仅隔离模型"],
                _ => vec![],
            })
            .into_iter()
            .map(String::from),
        );
        let statuses = match category {
            UnknownRateLimit => vec!["429"],
            Authentication => vec!["401"],
            Billing => vec!["402"],
            Permission => vec!["403"],
            RequestTooLarge => vec!["413", "414"],
            InvalidRequest => vec!["400", "405", "406", "409", "415", "422", "426", "501"],
            ServiceError => vec!["500", "502–599"],
            _ => vec![],
        };
        result.push(Category {
            category,
            label,
            group,
            action,
            codes: CODES
                .iter()
                .chain(STATUS_CODES)
                .filter(|c| c.category == category)
                .collect(),
            keywords,
            protocols: [
                "HTTP 错误响应",
                "SSE 错误事件",
                "Responses WebSocket 错误帧",
            ],
            excluded_keywords: QUOTA_NEGATIVES
                .iter()
                .chain(if category == Rpm { RPM_NEGATIVES } else { &[] })
                .copied()
                .collect(),
            conditions,
            statuses,
        });
    }
    result
}
