import { beforeEach, expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import { providerRuntimeStatus, providerStatus } from "./provider-status";
import type { GatewayState } from "./types";

function provider() {
  return structuredClone(gatewayDemo.providers[0]);
}

function state(overrides: Partial<GatewayState> = {}): GatewayState {
  return { ...structuredClone(gatewayDemo), ...overrides };
}

beforeEach(() => {
  // Keep each case independent from the shared preview object.
});

it("turns a concrete transport cause into a short circuit status", () => {
  const p = provider();
  p.health = {
    ...p.health,
    state: "open",
    retryIn: 7,
    cause: {
      eventId: "fixture-event-timeout",
      code: "FIRST_BYTE_TIMEOUT",
      reason: "network",
      status: null,
      wsCloseCode: null,
    },
  };

  expect(providerStatus(p)).toBe("熔断 · 首字超时 · 7秒");
});

it("distinguishes a 429 rate limit from other capacity waits", () => {
  const p = provider();
  p.health = {
    ...p.health,
    cooldownReason: "capacity_retry",
    retryIn: 12,
    cause: {
      eventId: "fixture-event-rate-limit",
      code: "RATE_LIMITED",
      reason: "rate_limit",
      status: 429,
      wsCloseCode: null,
    },
  };

  expect(providerStatus(p)).toBe("限流 429 · 等待12秒");

  p.health = {
    ...p.health,
    cause: {
      eventId: "fixture-event-capacity",
      code: "CAPACITY_LIMITED",
      reason: "capacity",
      status: 503,
      wsCloseCode: null,
    },
  };
  expect(providerStatus(p)).toBe("容量不足 · 等待12秒");
});

it("uses network and WebSocket causes for protected cooldowns", () => {
  const p = provider();
  p.health = {
    ...p.health,
    cooldownReason: "single_provider_protected",
    retryIn: 4,
    cause: {
      eventId: "fixture-event-ws",
      code: "STREAM_INTERRUPTED",
      reason: "network",
      status: null,
      wsCloseCode: 1006,
    },
  };

  expect(providerStatus(p)).toBe("连接中断 · 冷却4秒");
});

it("retains useful non-cause statuses and clamps negative retry values", () => {
  const p = provider();
  p.health = { ...p.health, cooldownReason: "retry_after", retryIn: -3 };
  expect(providerStatus(p)).toBe("单供应商保护 · 冷却0秒");

  p.health = {
    ...p.health,
    cooldownReason: null,
    retryIn: 0,
    cause: null,
  };
  p.rpmLimited = true;
  p.rpmRetryIn = 0;
  expect(providerStatus(p)).toBe("RPM 已满 · 等待1秒");
});

it("shows a transient upstream retry on the original provider", () => {
  const p = provider();
  p.health = {
    ...p.health,
    cause: {
      eventId: "fixture-event-transient",
      code: "UPSTREAM_RESPONSE_ERROR",
      reason: "upstream_service",
      status: 502,
      wsCloseCode: null,
    },
  };

  expect(
    providerRuntimeStatus(p, state({ transientRetries: [{ providerId: p.id, retryIn: 9 }] })),
  ).toBe("上游 502 · 原线路重试9秒");
});

it("prioritizes a WebSocket reconnect over other runtime states", () => {
  const p = provider();

  expect(
    providerRuntimeStatus(
      p,
      state({
        websocketRetries: [{ providerId: p.id, retryIn: 4 }],
        transientRetries: [{ providerId: p.id, retryIn: 9 }],
      }),
    ),
  ).toBe("WS 断开 · 重连4秒");
});

it("indicates a provider waiting for compaction handoff", () => {
  const p = provider();

  expect(providerRuntimeStatus(p, state({ compactionPending: [p.id] }))).toBe(
    "等待压缩后接管",
  );
});
