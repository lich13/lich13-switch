/** Shared labels for provider rows, quick controls and diagnostic details. */
export type LimitEvidence = {
  errorBasis?: string | null;
  upstreamReason?: string | null;
  upstreamCode?: string | null;
  code?: string | null;
};
export function limitLabel(e?: LimitEvidence | null): string {
  const reason = e?.upstreamReason ?? e?.upstreamCode ?? e?.code ?? "";
  if (
    ["DAILY_LIMIT_EXCEEDED", "USER_PLATFORM_DAILY_QUOTA_EXHAUSTED"].includes(
      reason,
    )
  )
    return "日额度已用尽";
  if (
    ["WEEKLY_LIMIT_EXCEEDED", "USER_PLATFORM_WEEKLY_QUOTA_EXHAUSTED"].includes(
      reason,
    )
  )
    return "周额度已用尽";
  if (
    [
      "MONTHLY_LIMIT_EXCEEDED",
      "USER_PLATFORM_MONTHLY_QUOTA_EXHAUSTED",
    ].includes(reason)
  )
    return "月额度已用尽";
  return (
    (
      {
        quota_exhausted: "额度已耗尽",
        rpm: "上游 RPM 限制",
        capacity: "临时容量限制",
        usage_limit: "用量限制，额度状态待核验",
      } as Record<string, string>
    )[e?.errorBasis ?? ""] ?? ""
  );
}
export function rpmWaitLabel(reason?: string | null): string {
  return (
    (
      {
        pacing: "RPM 均匀发送",
        send_window: "RPM 分钟额度已满",
        completion_window: "RPM 完成窗口保护",
        upstream_backoff: "上游 RPM 退避",
      } as Record<string, string>
    )[reason ?? ""] ?? "RPM 已满"
  );
}
export type QuotaCheck = {
  verdict: string;
  source: string;
  basis: string;
  checkedAt: number;
};
export function quotaCheckLabel(check: QuotaCheck): string {
  return (
    (
      {
        exhausted: "本次查询确认额度耗尽",
        available: "本次查询未证实额度耗尽，继续等待",
        unknown: "本次查询证据不足，继续等待",
      } as Record<string, string>
    )[check.verdict] ?? "额度查询结果未知"
  );
}
export function quotaBasisLabel(basis: string): string {
  return (
    (
      {
        wallet: "钱包",
        subscription: "适用订阅",
        key_quota: "当前 Key 配额",
        spending_window: "消费额度窗口",
        unlimited: "无限额度",
        incomplete: "信息不完整",
        query_failed: "查询未完成",
      } as Record<string, string>
    )[basis] ?? "未识别依据"
  );
}
