import { expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import {
  providerHandoffStatus,
  providerRuntimeStatus,
  providerStatus,
} from "./provider-status";
import type { GatewayState } from "./types";

// Each case works on its own copy of the shared preview object.
function provider() {
  return structuredClone(gatewayDemo.providers[0]);
}

function state(overrides: Partial<GatewayState> = {}): GatewayState {
  return { ...structuredClone(gatewayDemo), ...overrides };
}

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

  expect(providerStatus(p)).toBe("熔断 · 首字超时 · 等待7秒");
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

it("keeps handoff waiting separate from clickable runtime status", () => {
  const p = provider();

  const current = state({
    handoffWaiting: [{ providerId: p.id, state: "awaiting_compaction" }],
  });
  expect(providerRuntimeStatus(p, current)).toBe("");
  expect(providerHandoffStatus(p, current)).toBe("待压缩后接管");
});

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
    state({
      capacityRetries: [
        { providerId: p.id, retryIn: 9, existingOwner: false },
      ],
    }),
  );
  expect(newThread).not.toContain("原供应商等待");
  expect(newThread).toBe("");
});
