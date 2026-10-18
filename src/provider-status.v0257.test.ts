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

  expect(providerStatus(p)).toBe("日额度已用尽");
});

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
