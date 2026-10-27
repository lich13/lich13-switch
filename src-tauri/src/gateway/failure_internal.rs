//! Read-only descriptions of existing local gateway handling; never routing rules.
use super::failure_match::Parameter;
use crate::errors::code;
use serde::Serialize;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub origin: &'static str,
    pub category: &'static str,
    pub policy_id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub action: &'static str,
    pub codes: Vec<Code>,
    pub conditions: Vec<&'static str>,
    pub parameters: Vec<Parameter>,
}
#[derive(Serialize)]
pub struct Code {
    pub value: &'static str,
    pub platform: &'static str,
}
pub fn catalog() -> Vec<Policy> {
    use Parameter::*;
    [
        (
            "connection",
            "连接失败与连接超时",
            "安全边界内原商重试，耗尽后按现有规则换商",
            vec![
                code::CONNECTION_FAILED,
                code::CONNECT_TIMEOUT,
                code::NETWORK_ERROR,
                code::NETWORK,
            ],
            vec![
                "上游连接失败",
                "上游连接超时",
                "connection refused",
                "connection timed out",
            ],
            vec![ConnectSeconds, MaxRetries, RetryBudgetSeconds],
        ),
        (
            "tls",
            "TLS 与网关回环",
            "返回错误，不按临时连接故障重试",
            vec![code::TLS_HANDSHAKE_FAILED, code::TLS, code::GATEWAY_LOOP],
            vec!["TLS 验证失败", "上游指向本网关"],
            vec![],
        ),
        (
            "first_byte",
            "首字节与等待预算超时",
            "超时后终止；已派发请求不重放",
            vec![code::FIRST_BYTE_TIMEOUT],
            vec!["首字节超时"],
            vec![FirstByteSeconds, RetryBudgetSeconds],
        ),
        (
            "stream",
            "流静默与交付中断",
            "已交付内容不重放；未交付时遵守现有安全重试边界",
            vec![
                code::STREAM_TIMEOUT,
                code::STREAM_INTERRUPTED,
                code::REQUEST_INTERRUPTED,
                code::CLIENT_DISCONNECTED,
            ],
            vec!["流静默超时", "上游流中断", "客户端已断开"],
            vec![IdleSeconds],
        ),
        (
            "websocket",
            "WebSocket 断开等待",
            "未交付且上下文安全时等待原商重连；已交付不重放",
            vec![
                code::WEBSOCKET,
                code::WS_UPGRADE_FAILED,
                code::WS_HANDSHAKE_TIMEOUT,
                code::WS_UNSUPPORTED,
                code::WS_EXTENSION_UNSUPPORTED,
            ],
            vec!["WebSocket 连接失败", "WebSocket 断开"],
            vec![WebsocketRetrySeconds, ConnectSeconds, RetryBudgetSeconds],
        ),
        (
            "admission",
            "本地准入与等待",
            "遵守并发、RPM、队列与总等待预算",
            vec![
                code::CONCURRENCY,
                code::CAPACITY,
                code::RPM_LIMIT,
                code::QUEUE,
                code::PROVIDERS_COOLING_DOWN,
                code::CIRCUIT_OPEN,
                code::RPM_LEDGER,
            ],
            vec![
                "供应商并发已满",
                "等待队列已满",
                "排队超时",
                "供应商已达到 RPM 上限",
            ],
            vec![QueueSeconds, MaxWaiting, RetryBudgetSeconds],
        ),
        (
            "routing",
            "路由与重放保护",
            "无合格供应商或上下文不安全时返回原因",
            vec![
                code::NO_PROVIDER,
                code::NO_ELIGIBLE_PROVIDER,
                code::FAILOVER_EXHAUSTED,
                code::CONVERSATION_OWNERSHIP,
                code::COMPACTION_CONTEXT_UNSUPPORTED,
            ],
            vec!["没有可用供应商", "没有合格供应商"],
            vec![],
        ),
        (
            "lifecycle",
            "停止与配置冲突",
            "终止当前操作或重新读取配置后重试",
            vec![
                code::STOPPED,
                code::GATEWAY_STOPPED,
                code::CANCELLED,
                code::CONFIG_CONFLICT,
                code::LOCAL_AUTH,
                code::CONFIG,
                code::CLIENT,
                code::STALE,
            ],
            vec!["网关已停止", "配置发生冲突", "客户端网关凭据不匹配"],
            vec![],
        ),
    ]
    .into_iter()
    .map(
        |(id, label, action, codes, conditions, parameters)| Policy {
            origin: "gateway",
            category: id,
            policy_id: id,
            label,
            group: "网关内部",
            action,
            codes: codes
                .into_iter()
                .map(|value| Code {
                    value,
                    platform: "网关",
                })
                .collect(),
            conditions,
            parameters,
        },
    )
    .collect()
}
