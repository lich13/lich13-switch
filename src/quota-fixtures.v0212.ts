import type { Provider, ProviderQuota, QuotaPlan } from "./types";

export function quotaPlan(overrides: Partial<QuotaPlan> = {}): QuotaPlan {
  return {
    name: "每日",
    remaining: 8,
    used: 2,
    total: 10,
    unit: "USD",
    unlimited: false,
    resetAt: null,
    ...overrides,
  };
}

export function quotaFixture(
  provider: Provider,
  overrides: Partial<ProviderQuota> = {},
): ProviderQuota {
  return {
    providerId: provider.id,
    version: provider.quotaVersion,
    state: "ok",
    source: "sub2api",
    checkedAt: 1_800_000_000,
    successAt: 1_800_000_000,
    retryAt: null,
    stale: false,
    error: null,
    keyStatus: null,
    plans: [quotaPlan()],
    subscriptions: [],
    expiresAt: null,
    expiresAtUnix: null,
    today: null,
    totalUsage: null,
    ...overrides,
  };
}
