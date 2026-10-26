import { expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import { providerRuntimeStatus, providerStatus } from "./provider-status";
import type { GatewayState } from "./types";

function provider() {
  return structuredClone(gatewayDemo.providers[0]);
}

function state(overrides: Partial<GatewayState> = {}): GatewayState {
  return { ...structuredClone(gatewayDemo), ...overrides };
}

it("prioritizes the daily quota reason over a generic 429 status", () => {
  const p = provider();
  p.health = {
    ...p.health,
    cooldownReason: "rate_limit",
    retryIn: 60,
    cause: {
      eventId: "fixture-daily-limit",
      code: "RATE_LIMITED",
      upstreamReason: "DAILY_LIMIT_EXCEEDED",
      reason: "rate_limit",
      status: 429,
      wsCloseCode: null,
    },
  };

  expect(providerStatus(p)).toBe("日额度已用尽 · 等待60秒");
});

it.each([
  ["billing", "余额或消费额度不足"],
  ["authentication", "认证失败"],
  ["tpm", "上游 Token 限流"],
  ["concurrency", "上游并发限制"],
  ["unknown_rate_limit", "未分类限流"],
])(
  "shows %s cooldown and on-demand recovery consistently",
  (errorBasis, label) => {
    const p = provider();
    p.health = {
      ...p.health,
      available: false,
      cooldownReason: "hard_rejection",
      retryIn: 300,
      cause: {
        eventId: "fixture-policy",
        code: "RATE_LIMITED",
        reason: "rate_limit",
        errorBasis,
      status: 429,
      wsCloseCode: null,
      },
    };
    expect(providerStatus(p)).toBe(`${label} · 等待300秒`);
    p.health.retryIn = 0;
    expect(providerStatus(p)).toBe(`${label} · 待请求探测`);
    p.health.probeInFlight = true;
    expect(providerStatus(p)).toBe(`恢复探测中 · ${label}`);
  },
);

it("shows original-provider capacity waiting only for an explicit owner retry", () => {
  const p = provider();

  expect(
    providerRuntimeStatus(
      p,
      state({
        capacityRetries: [
          { providerId: p.id, retryIn: 9, existingOwner: true },
        ],
      }),
    ),
  ).toBe("临时限流 · 原供应商等待9秒");

  const newThread = providerRuntimeStatus(
    p,
    state({ capacityRetries: [{ providerId: p.id, retryIn: 9 }] }),
  );
  expect(newThread).not.toContain("原供应商等待");
  expect(newThread).toBe("");
});
