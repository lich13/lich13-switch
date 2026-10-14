import { expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import {
  providerRuntimeStatus,
  providerStatus,
} from "./provider-status";
import type { GatewayState } from "./types";

function provider() {
  return structuredClone(gatewayDemo.providers[0]);
}

function state(overrides: Partial<GatewayState> = {}): GatewayState {
  return { ...structuredClone(gatewayDemo), ...overrides };
}

it("marks an open circuit with no retry delay as waiting for a request probe", () => {
  const p = provider();
  p.health = {
    ...p.health,
    state: "open",
    retryIn: 0,
    probeInFlight: false,
    cause: {
      eventId: "fixture-open-zero-retry",
      code: "FIRST_BYTE_TIMEOUT",
      reason: "network",
      status: null,
      wsCloseCode: null,
    },
  };

  expect(providerStatus(p)).toBe("熔断 · 首字超时 · 待请求探测");
});

it("distinguishes an in-flight recovery probe from a pending probe", () => {
  const p = provider();
  p.health = {
    ...p.health,
    state: "open",
    retryIn: 4,
    probeInFlight: true,
    cause: {
      eventId: "fixture-recovery-probe",
      code: "FIRST_BYTE_TIMEOUT",
      reason: "network",
      status: null,
      wsCloseCode: null,
    },
  };

  expect(providerStatus(p)).toBe("恢复探测中 · 首字超时");
});

it("shows a half-open provider waiting for the next request when no probe is in flight", () => {
  const p = provider();
  p.health = {
    ...p.health,
    state: "half_open",
    retryIn: 0,
    probeInFlight: false,
    cause: null,
  };

  expect(providerStatus(p)).toBe("待请求探测");
});

it("keeps a model circuit visible when availability has recovered but retryIn is zero", () => {
  const p = provider();
  p.modelHealth = [
    {
      model: "gpt-5-mini",
      health: {
        ...p.health,
        state: "open",
        available: true,
        retryIn: 0,
        probeInFlight: false,
        cause: {
          eventId: "fixture-model-open-zero-retry",
          code: "FIRST_BYTE_TIMEOUT",
          reason: "network",
          status: null,
          wsCloseCode: null,
        },
      },
    },
  ];

  expect(providerStatus(p)).toBe(
    "mini · 熔断 · 首字超时 · 待请求探测",
  );
});

it("prioritizes open or half-open circuit status over same-provider retries", () => {
  const p = provider();
  const queuedRetry = state({
    transientRetries: [{ providerId: p.id, retryIn: 9 }],
  });

  p.health = {
    ...p.health,
    state: "open",
    retryIn: 0,
    cause: {
      eventId: "fixture-provider-open",
      code: "UPSTREAM_RESPONSE_ERROR",
      reason: "upstream_service",
      status: 502,
      wsCloseCode: null,
    },
  };
  expect(providerRuntimeStatus(p, queuedRetry)).toBe(
    "熔断 · 上游 502 · 待请求探测",
  );

  p.health = {
    ...p.health,
    state: "half_open",
    retryIn: 0,
    cooldownReason: null,
    probeInFlight: false,
    cause: null,
  };
  expect(providerRuntimeStatus(p, queuedRetry)).toBe("待请求探测");
});
