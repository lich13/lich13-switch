import { expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import { providerStatus } from "./provider-status";

function provider() {
  return structuredClone(gatewayDemo.providers[0]);
}

it("shows a model-scoped cooldown when the provider itself is healthy", () => {
  const p = provider();
  p.modelHealth = [
    {
      model: "gpt-5-mini",
      health: {
        ...p.health,
        state: "open",
        available: false,
        retryIn: 7,
        cause: {
          eventId: "fixture-model-cooldown",
          code: "RATE_LIMITED",
          reason: "rate_limit",
          status: 429,
          wsCloseCode: null,
        },
      },
    },
  ];

  expect(providerStatus(p)).toBe("mini · 熔断 · 上游 429 · 等待7秒");
});

it("keeps provider cooldown ahead of a model-scoped cooldown", () => {
  const p = provider();
  p.health = {
    ...p.health,
    cooldownReason: "capacity_retry",
    retryIn: 12,
    cause: {
      eventId: "fixture-provider-cooldown",
      code: "CAPACITY_LIMITED",
      reason: "capacity",
      status: 503,
      wsCloseCode: null,
    },
  };
  p.modelHealth = [
    {
      model: "gpt-5-mini",
      health: {
        ...p.health,
        cooldownReason: "retry_after",
        retryIn: 2,
      },
    },
  ];

  expect(providerStatus(p)).toBe("容量不足 · 等待12秒");
});
