import { expect, it } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import { providerRuntimeStatus, providerStatus } from "./provider-status";
import { limitLabel, quotaCheckLabel } from "./limit-labels";

it.each([
  ["pacing", "RPM 均匀发送"],
  ["send_window", "RPM 发送窗口已满"],
  ["completion_window", "RPM 完成窗口保护"],
  ["upstream_backoff", "上游 RPM 退避"],
])("shows %s separately from the sent request counter", (reason, label) => {
  const state = structuredClone(gatewayDemo);
  const p = state.providers[0];
  p.health = {
    ...p.health,
    state: "closed",
    available: true,
    probeInFlight: false,
    cooldownReason: null,
    cause: null,
  };
  p.modelHealth = [];
  p.rpmLimited = true;
  p.rpmRetryIn = 6;
  p.rpmUsed = 3;
  p.rpmWaitReason = reason;
  expect(providerStatus(p)).toBe(`${label} · 等待6秒`);
  expect(providerRuntimeStatus(p, state)).toBe(providerStatus(p));
  expect(p.rpmUsed).toBe(3);
});

it("shows a live quota check during owner waiting without claiming a known balance", () => {
  const state = structuredClone(gatewayDemo);
  const p = state.providers[0];
  p.quotaChecking = true;
  state.capacityRetries = [
    { providerId: p.id, existingOwner: true, retryIn: 45 },
  ];
  expect(providerRuntimeStatus(p, state)).toBe(
    "临时限流 · 原供应商等待45秒 · 正在核验额度",
  );
  expect(
    quotaCheckLabel({
      verdict: "available",
      source: "sub2_api",
      basis: "wallet",
      checkedAt: 1,
    }),
  ).toContain("未证实额度耗尽");
  expect(limitLabel({ upstreamCode: "DAILY_LIMIT_EXCEEDED" })).toBe(
    "日额度已用尽",
  );
  expect(limitLabel({ upstreamReason: "WEEKLY_LIMIT_EXCEEDED" })).toBe(
    "周额度已用尽",
  );
  expect(limitLabel({ upstreamReason: "MONTHLY_LIMIT_EXCEEDED" })).toBe(
    "月额度已用尽",
  );
  expect(limitLabel({ errorBasis: "future_reason" })).toBe("");
  expect(limitLabel()).toBe("");
});
