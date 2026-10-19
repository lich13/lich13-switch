import type { Provider, GatewayState } from "./types";
function pendingHealth(h: Provider["health"]) {
  return h.available === false || h.state !== "closed" || h.probeInFlight;
}
export function providerFault(p: Provider) {
  return (
    p.health.cause ??
    p.modelHealth?.find((m) => pendingHealth(m.health))?.health.cause
  );
}
export function providerRuntimeStatus(
  p: Provider,
  state: GatewayState,
): string {
  const fault = providerFault(p);
  if (
    fault?.upstreamReason === "DAILY_LIMIT_EXCEEDED" ||
    fault?.code === "DAILY_LIMIT_EXCEEDED"
  )
    return "日额度已用尽";
  const capacity = state.capacityRetries?.find(
    (w) => w.providerId === p.id && w.existingOwner,
  );
  if (capacity) return `临时限流 · 原供应商等待${capacity.retryIn}秒`;
  if (
    p.health.state !== "closed" ||
    p.modelHealth?.some((m) => m.health.state !== "closed")
  )
    return providerStatus(p);
  const reconnect = state.websocketRetries?.find((w) => w.providerId === p.id);
  if (reconnect) return `WS 断开 · 重连${reconnect.retryIn}秒`;
  const retry = state.transientRetries?.find((w) => w.providerId === p.id);
  if (retry)
    return `${causeText(p) || "上游波动"} · 原线路重试${retry.retryIn}秒`;
  return providerStatus(p);
}
export function providerHandoffStatus(
  p: Provider,
  state: GatewayState,
): string {
  const waiting = state.handoffWaiting?.find((w) => w.providerId === p.id);
  return waiting
    ? {
        awaiting_compaction: "待压缩后接管",
        awaiting_request: "待下一请求接管",
        capacity_full: "接管名额已满",
        claim_in_flight: "正在尝试接管",
      }[waiting.state]
    : "";
}
function causeText(p: Provider): string {
  const c = p.health.cause;
  if (!c) return "";
  if (
    c.upstreamReason === "DAILY_LIMIT_EXCEEDED" ||
    c.code === "DAILY_LIMIT_EXCEEDED"
  )
    return "日额度已用尽";
  const codes: Record<string, string> = {
    FIRST_BYTE_TIMEOUT: "首字超时",
    CONNECT_TIMEOUT: "连接超时",
    WS_HANDSHAKE_TIMEOUT: "WS 握手超时",
    TLS_HANDSHAKE_FAILED: "TLS 失败",
    TLS: "TLS 失败",
    STREAM_TIMEOUT: "流超时",
    STREAM_INTERRUPTED: "连接中断",
    CONNECTION_FAILED: "连接失败",
    NETWORK: "连接失败",
  };
  if (codes[c.code]) return codes[c.code];
  if (c.reason === "authentication") return "认证失败";
  if (c.status != null && c.status >= 400) return `上游 ${c.status}`;
  if (c.wsCloseCode != null) return `WS ${c.wsCloseCode}`;
  return (
    (
      {
        capacity: "容量不足",
        network: "连接失败",
        upstream_service: "上游异常",
      } as Record<string, string>
    )[c.reason] ?? ""
  );
}
export function providerStatus(p: Provider): string {
  const h = p.health;
  const reason = causeText(p);
  if (h.available !== false && h.state === "closed" && !h.cooldownReason) {
    const model = p.modelHealth?.find((m) => pendingHealth(m.health));
    if (model) {
      const label = model.model.replace(/^gpt-[\d.]+-/, "");
      const scoped = { ...p, health: model.health, modelHealth: [] };
      return `${label} · ${providerStatus(scoped)}`;
    }
  }
  const seconds = Math.max(0, h.retryIn);
  if (reason === "日额度已用尽") return reason;
  if (h.probeInFlight)
    return ["恢复探测中", reason].filter(Boolean).join(" · ");
  if (h.state === "open")
    return ["熔断", reason, seconds ? `等待${seconds}秒` : "待请求探测"]
      .filter(Boolean)
      .join(" · ");
  if (
    h.cooldownReason === "capacity_retry" ||
    h.cooldownReason === "rate_limit"
  )
    return `${h.cause?.status === 429 || h.cooldownReason === "rate_limit" ? "限流 429" : "容量不足"} · 等待${seconds}秒`;
  if (
    h.cooldownReason === "single_provider_protected" ||
    h.cooldownReason === "retry_after"
  )
    return `${reason || "单供应商保护"} · 冷却${seconds}秒`;
  if (h.state === "half_open")
    return ["待请求探测", reason].filter(Boolean).join(" · ");
  if (p.rpmLedgerError) return "RPM 记录不可用";
  if (p.rpmLimited) return `RPM 已满 · 等待${Math.max(1, p.rpmRetryIn)}秒`;
  const occupied = p.activeRequests + (p.reservedThreads ?? 0);
  if (p.maxConcurrency > 0 && occupied >= p.maxConcurrency)
    return `并发已满 · ${occupied}/${p.maxConcurrency}`;
  return "";
}
