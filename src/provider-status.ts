import type { Provider, GatewayState } from "./types";
export function providerRuntimeStatus(p: Provider, state: GatewayState): string {
  const reconnect = state.websocketRetries?.find((w) => w.providerId === p.id);
  if (reconnect) return `WS 断开 · 重连${reconnect.retryIn}秒`;
  const retry = state.transientRetries?.find((w) => w.providerId === p.id);
  if (retry) return `${causeText(p) || "上游波动"} · 原线路重试${retry.retryIn}秒`;
  return providerStatus(p) || (state.compactionPending?.includes(p.id) ? "等待压缩后接管" : "");
}
function causeText(p: Provider): string {
  const c = p.health.cause;
  if (!c) return "";
  const codes: Record<string, string> = { FIRST_BYTE_TIMEOUT: "首字超时", CONNECT_TIMEOUT: "连接超时", TLS_HANDSHAKE_FAILED: "TLS 失败", TLS: "TLS 失败", STREAM_TIMEOUT: "流超时", STREAM_INTERRUPTED: "连接中断", CONNECTION_FAILED: "连接失败", NETWORK: "连接失败" };
  if (codes[c.code]) return codes[c.code];
  if (c.reason === "authentication") return "认证失败";
  if (c.status != null && c.status >= 400) return `上游 ${c.status}`;
  if (c.wsCloseCode != null) return `WS ${c.wsCloseCode}`;
  return ({ capacity: "容量不足", network: "连接失败", upstream_service: "上游异常" } as Record<string, string>)[c.reason] ?? "";
}
export function providerStatus(p: Provider): string {
  const h = p.health;
  const reason = causeText(p);
  const seconds = Math.max(0, h.retryIn);
  if (h.probeInFlight) return ["恢复探测", reason].filter(Boolean).join(" · ");
  if (h.state === "open") return ["熔断", reason, seconds ? `${seconds}秒` : ""].filter(Boolean).join(" · ");
  if (h.cooldownReason === "capacity_retry" || h.cooldownReason === "rate_limit")
    return `${h.cause?.status === 429 || h.cooldownReason === "rate_limit" ? "限流 429" : "容量不足"} · 等待${seconds}秒`;
  if (h.cooldownReason === "single_provider_protected" || h.cooldownReason === "retry_after")
    return `${reason || "单供应商保护"} · 冷却${seconds}秒`;
  if (h.state === "half_open") return "恢复探测";
  if (p.rpmLedgerError) return "RPM 记录不可用";
  if (p.rpmLimited) return `RPM 已满 · 等待${Math.max(1, p.rpmRetryIn)}秒`;
  if (p.maxConcurrency > 0 && p.activeRequests >= p.maxConcurrency)
    return `并发已满 · ${p.activeRequests}/${p.maxConcurrency}`;
  return "";
}
