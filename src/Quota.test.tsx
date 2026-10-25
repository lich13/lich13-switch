import { act, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ClientId, Provider, ProviderQuota } from "./types";
import { quotaFixture, quotaPlan } from "./quota-fixtures.v0212";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (s: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, fn: (s: unknown) => void) => {
    mock.listeners.set(name, fn);
    return () => mock.listeners.delete(name);
  }),
}));
import { QuotaInfo, useProviderQuota } from "./Quota";
import { gatewayDemo } from "./gateway-preview";
let provider: Provider;
function result(p = provider): ProviderQuota {
  return {
    providerId: p.id,
    version: p.quotaVersion,
    state: "ok",
    source: "sub2api",
    checkedAt: Date.now() / 1000,
    successAt: Date.now() / 1000,
    retryAt: null,
    stale: false,
    error: null,
    keyStatus: null,
    plans: [
      {
        name: "Key 配额",
        remaining: 8.5,
        used: 1.5,
        total: 10,
        unit: "USD",
        unlimited: false,
        resetAt: null,
      },
    ],
    expiresAt: null,
    expiresAtUnix: null,
    today: null,
    totalUsage: null,
  };
}
function Harness({
  p = provider,
  active = true,
  clientId = "codex",
  refreshSeconds = 60,
}: {
  p?: Provider;
  active?: boolean;
  clientId?: ClientId;
  refreshSeconds?: number;
}) {
  const q = useProviderQuota(
    [p],
    active,
    "app-visibility",
    clientId,
    refreshSeconds,
  );
  return (
    <>
      <QuotaInfo
        provider={p}
        quota={q.quotaFor(p)}
        refresh={() => void q.refresh(p.id)}
      />
    </>
  );
}
async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}
beforeEach(() => {
  localStorage.clear();
  provider = structuredClone(gatewayDemo.providers[0]);
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(async () => result());
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});
afterEach(() => {
  vi.useRealTimers();
  localStorage.clear();
});
describe("quota display and refresh", () => {
  it.each([false, true])("distinguishes an unavailable account balance from a key quota (unlimited=%s)", (unlimited) => {
    const q = quotaFixture(provider, {
      source: "newapi",
      plans: [quotaPlan({ name: "Key 额度", remaining: 8.5, unlimited })],
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);
    expect(screen.getByText("账户余额未开放")).toBeInTheDocument();
    expect(screen.getByText(unlimited ? "Key 配额无限制" : "Key 剩余 8.5 USD")).toBeInTheDocument();
  });
  it("shows account balance first and retains it with a separate Key query warning", () => {
    const q = quotaFixture(provider, {
      source: "newapi", error: "账户余额已更新，Key 配额查询失败",
      plans: [quotaPlan({ name: "账户余额", remaining: 1.5 }), quotaPlan({ name: "Key 额度", unlimited: true })],
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);
    expect(screen.getByText("剩余 1.5 USD")).toBeInTheDocument();
    expect(screen.getByText("账户余额")).toBeInTheDocument();
    expect(screen.getByText("Key 额度")).toBeInTheDocument();
    expect(screen.getByText("账户余额已更新，Key 配额查询失败")).toBeInTheDocument();
    expect(screen.queryByText("账户余额未开放")).not.toBeInTheDocument();
  });
  it("refreshes on entry and each minute, pauses while hidden or on another page, resumes with a cache-aware refresh", async () => {
    vi.useFakeTimers();
    const view = render(<Harness />);
    await flush();
    expect(mock.command).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(60_000));
    expect(mock.command).toHaveBeenCalledTimes(2);
    act(() => mock.listeners.get("app-visibility")!(false));
    await act(async () => vi.advanceTimersByTime(120_000));
    expect(mock.command).toHaveBeenCalledTimes(2);
    act(() => mock.listeners.get("app-visibility")!(true));
    await flush();
    expect(mock.command).toHaveBeenCalledTimes(3);
    view.rerender(<Harness active={false} />);
    await act(async () => vi.advanceTimersByTime(120_000));
    expect(mock.command).toHaveBeenCalledTimes(3);
    view.rerender(<Harness />);
    await flush();
    expect(mock.command).toHaveBeenLastCalledWith("query_provider_quota", {
      clientId: "codex",
      providerId: provider.id,
      force: false,
    });
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "hidden",
    });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    const count = mock.command.mock.calls.length;
    await act(async () => vi.advanceTimersByTime(120_000));
    expect(mock.command).toHaveBeenCalledTimes(count);
  });
  it("uses the configured refresh interval and preserves a zero interval as manual-only", async () => {
    vi.useFakeTimers();
    const view = render(<Harness refreshSeconds={10} />);
    await flush();
    expect(mock.command).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(9_000));
    expect(mock.command).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(1_000));
    expect(mock.command).toHaveBeenCalledTimes(2);

    view.rerender(<Harness refreshSeconds={0} />);
    await act(async () => vi.advanceTimersByTime(30_000));
    expect(mock.command).toHaveBeenCalledTimes(2);
    fireEvent.click(
      screen.getByRole("button", {
        name: "刷新 " + provider.name + " 额度",
      }),
    );
    await flush();
    expect(mock.command).toHaveBeenLastCalledWith("query_provider_quota", {
      clientId: "codex",
      providerId: provider.id,
      force: true,
    });
  });
  it("honors an upstream nextRefreshAt that is earlier than the configured interval", async () => {
    vi.useFakeTimers();
    mock.command.mockImplementationOnce(async () => ({
      ...result(),
      nextRefreshAt: Date.now() / 1000 + 10,
    }));
    const view = render(<Harness refreshSeconds={120} />);
    await flush();
    expect(mock.command).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(9_000));
    expect(mock.command).toHaveBeenCalledTimes(1);
    await act(async () => vi.advanceTimersByTime(1_000));
    expect(mock.command).toHaveBeenCalledTimes(2);
    view.unmount();
  });
  it("manual refresh requests fresh data and details retain the actual quota unit", async () => {
    render(<Harness />);
    await flush();
    expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
    fireEvent.click(
      screen.getByRole("button", { name: `刷新 ${provider.name} 额度` }),
    );
    await flush();
    expect(mock.command).toHaveBeenLastCalledWith("query_provider_quota", {
      clientId: "codex",
      providerId: provider.id,
      force: true,
    });
    expect(
      screen.getByRole("columnheader", { name: /额度.*USD/ }),
    ).toBeInTheDocument();
    expect(screen.getByRole("row", { name: /Key 配额/ })).toHaveTextContent(
      "1.5 / 10",
    );
  });
  it("shows a unique active subscription beside compact quota without relabeling existing plans", () => {
    const q = {
      ...result(),
      subscriptions: [
        {
          name: "Pro",
          status: "active",
          expiresAt: null,
          active: true,
          plans: [{ ...result().plans[0], name: "订阅窗口" }],
        },
      ],
    };
    render(
      <QuotaInfo
        provider={provider}
        quota={q}
        compact
        details={() => {}}
        refresh={() => {}}
      />,
    );
    const area = screen.getByLabelText(`${provider.name} 额度`);
    expect(within(area).getByText("Pro")).toHaveClass("quota-subscription");
    expect(within(area).queryByText("订阅窗口")).not.toBeInTheDocument();
    expect(within(area).queryByText("Key 配额")).not.toBeInTheDocument();
  });
  it("summarizes multiple subscriptions and keeps their plans in on-demand details", () => {
    const q = {
      ...result(),
      subscriptions: [
        {
          name: "Pro",
          status: "active",
          expiresAt: null,
          active: true,
          plans: [{ ...result().plans[0], name: "Pro 周期" }],
        },
        {
          name: null,
          status: null,
          expiresAt: null,
          active: null,
          plans: [{ ...result().plans[0], name: "未知周期" }],
        },
      ],
    };
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);
    expect(screen.getByRole("region", { name: "Pro" })).toHaveTextContent(
      "Pro 周期",
    );
    expect(
      screen.getByRole("region", { name: "未识别套餐" }),
    ).toHaveTextContent("未知周期");
    expect(screen.getByRole("region", { name: "额度窗口" })).toHaveTextContent(
      "Key 配额",
    );
  });
  it("uses a count for ambiguous compact subscription summaries", () => {
    const q = {
      ...result(),
      subscriptions: [
        {
          name: "Pro",
          status: "active",
          expiresAt: null,
          active: true,
          plans: [],
        },
        {
          name: "Team",
          status: "active",
          expiresAt: null,
          active: true,
          plans: [],
        },
      ],
    };
    render(
      <QuotaInfo
        provider={provider}
        quota={q}
        compact
        details={() => {}}
        refresh={() => {}}
      />,
    );
    expect(screen.getByText("2 个套餐")).toHaveClass("quota-subscription");
  });
  it("shows each Sub2API window once while keeping its summary and standalone key quota separate", () => {
    const daily = quotaPlan({ name: "每日", remaining: 8, used: 2, total: 10 });
    const weekly = quotaPlan({ name: "每周", remaining: 42, used: 8, total: 50 });
    const q = quotaFixture(provider, {
      source: "sub2api",
      plans: [
        quotaPlan({ name: "开发者套餐", remaining: 8, used: 2, total: 10 }),
        daily,
        weekly,
        quotaPlan({ name: "Key 配额", remaining: 3.5, used: 1.5, total: 5 }),
      ],
      subscriptions: [
        {
          name: "开发者套餐",
          status: "active",
          active: true,
          expiresAt: "2027-12-31T00:00:00Z",
          plans: [daily, weekly],
        },
      ],
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);

    expect(screen.getAllByText("开发者套餐", { exact: true })).toHaveLength(1);
    expect(screen.getAllByText("每日", { exact: true })).toHaveLength(1);
    expect(screen.getAllByText("每周", { exact: true })).toHaveLength(1);
    expect(screen.getByRole("region", { name: "额度窗口" })).toHaveTextContent(
      "Key 配额",
    );
  });
  it("keeps matching top-level plans when a Sub2API payload contains multiple subscriptions", () => {
    const pro = quotaPlan({ name: "Pro", remaining: 8, used: 2, total: 10 });
    const team = quotaPlan({ name: "Team", remaining: 5, used: 1, total: 6 });
    const q = quotaFixture(provider, {
      source: "sub2api",
      plans: [pro],
      subscriptions: [
        {
          name: "Pro",
          status: "active",
          active: true,
          expiresAt: null,
          plans: [pro],
        },
        {
          name: "Team",
          status: "active",
          active: true,
          expiresAt: null,
          plans: [team],
        },
      ],
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);

    expect(screen.getAllByText("Pro", { exact: true })).toHaveLength(3);
    expect(screen.getByRole("region", { name: "额度窗口" })).toHaveTextContent(
      "Pro",
    );
  });
  it("keeps zero balances and distinct values or currencies instead of deduplicating them", () => {
    const zero = quotaPlan({ name: "每日", remaining: 0, used: 0, total: 10 });
    const changed = quotaPlan({ name: "每日", remaining: 7, used: 3, total: 10 });
    const credits = quotaPlan({
      name: "每日",
      remaining: 0,
      used: 0,
      total: 10,
      unit: "积分",
    });
    const q = quotaFixture(provider, {
      source: "newapi",
      plans: [zero, changed, credits],
      subscriptions: [
        {
          name: "基础套餐",
          status: "active",
          active: true,
          expiresAt: null,
          plans: [zero],
        },
      ],
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);

    expect(screen.getByText("剩余 0 USD")).toBeInTheDocument();
    expect(screen.queryByText("未返回剩余额度")).not.toBeInTheDocument();
    expect(screen.getAllByText("每日", { exact: true })).toHaveLength(4);
    expect(screen.getByRole("region", { name: "额度窗口" })).toHaveTextContent(
      "7 USD",
    );
    expect(screen.getByRole("region", { name: "额度窗口" })).toHaveTextContent(
      "0 积分",
    );
  });
  it("keeps long expired plans readable and expands usage statistics with compact token values", () => {
    const longName = "企业协作套餐 · 跨区域研发与长周期用量管理";
    const q = quotaFixture(provider, {
      plans: [quotaPlan({ name: "Key 配额", remaining: 0, used: 0, total: 10 })],
      subscriptions: [
        {
          name: longName,
          status: "expired",
          active: false,
          expiresAt: "2025-12-31T00:00:00Z",
          plans: [quotaPlan({ name: "每月", remaining: 1, used: 9, total: 10 })],
        },
      ],
      expiresAtUnix: 1_800_000_000,
      today: { requests: 18, tokens: 1_234, cost: 0 },
      totalUsage: { requests: 2_400, tokens: 9_600_000, cost: null },
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);

    const subscription = screen.getByRole("region", { name: longName });
    expect(subscription).toHaveTextContent("已到期");
    const expiry = within(subscription).getByText(/^到期 /);
    expect(expiry).toHaveAttribute("title", expect.stringContaining("套餐到期"));
    expect(subscription).toHaveTextContent("每月");
    const usage = screen.getByText("用量统计").closest("details");
    expect(usage).not.toBeNull();
    expect(usage).not.toHaveAttribute("open");
    fireEvent.click(screen.getByText("用量统计"));
    expect(usage).toHaveAttribute("open", "");
    expect(within(usage as HTMLDetailsElement).getByText("1.23K")).toBeInTheDocument();
    expect(within(usage as HTMLDetailsElement).getByText("9.6M")).toBeInTheDocument();
  });
  it("labels a real expired key as 已到期", () => {
    const q = quotaFixture(provider, {
      keyStatus: "已过期",
      plans: [],
    });

    render(<QuotaInfo provider={provider} quota={q} refresh={() => {}} />);

    expect(screen.getByText("已到期")).toBeInTheDocument();
    expect(screen.queryByText("已过期")).not.toBeInTheDocument();
  });
  it("does not label an elapsed compact cache as a real quota expiry", () => {
    const q = {
      ...result(),
      stale: true,
      nextRefreshAt: Date.now() / 1000 - 60,
    };

    render(
      <QuotaInfo
        provider={provider}
        quota={q}
        compact
        details={() => {}}
        refresh={() => {}}
      />,
    );

    expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
    expect(screen.queryByText("已过期")).not.toBeInTheDocument();
    expect(screen.queryByText("已到期")).not.toBeInTheDocument();
  });
  it("disables manual refresh while an upstream retry backoff is active", () => {
    const refresh = vi.fn();
    const q = quotaFixture(provider, {
      state: "error",
      error: "请求太频繁",
      retryAt: Date.now() / 1000 + 120,
      stale: true,
    });
    render(<QuotaInfo provider={provider} quota={q} refresh={refresh} />);
    const button = screen.getByRole("button", {
      name: `刷新 ${provider.name} 额度`,
    });
    expect(button).toBeDisabled();
    fireEvent.click(button);
    expect(refresh).not.toHaveBeenCalled();
  });
  it("keeps previous successful values visible with a stale error, without inventing zero", async () => {
    render(<Harness />);
    await flush();
    const retryAt = Date.now() / 1000 + 120;
    act(() =>
      mock.listeners.get("provider-quota")!({
        clientId: "codex",
        quota: {
          ...result(),
          state: "error",
          stale: true,
          error: "上游连接超时",
          retryAt,
        },
      }),
    );
    expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
    expect(screen.queryByText("已过期")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("上游连接超时");
    expect(screen.getByRole("status")).toHaveTextContent("后重试");
  });
  it("retains the cached balance and retry time when a manual refresh fails after the retry window", async () => {
    const retryAt = Date.now() / 1000 - 1;
    provider.quota = { ...result(), retryAt };
    mock.command.mockRejectedValueOnce(new Error("真实上游错误"));

    render(<Harness refreshSeconds={0} />);
    fireEvent.click(screen.getByRole("button", { name: `刷新 ${provider.name} 额度` }));
    await flush();

    expect(mock.command).toHaveBeenCalledWith("query_provider_quota", {
      clientId: "codex", providerId: provider.id, force: true,
    });
    expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
    expect(screen.queryByText("已过期")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("真实上游错误");
    expect(screen.getByRole("status")).toHaveTextContent("后重试");
  });
  it("discards late responses and events from a replaced credential version", async () => {
    let resolveOld!: (q: ProviderQuota) => void;
    mock.command.mockImplementationOnce(
      () =>
        new Promise<ProviderQuota>((resolve) => {
          resolveOld = resolve;
        }),
    );
    const view = render(<Harness />);
    await flush();
    const newer = { ...provider, quotaVersion: "new-key" };
    mock.command.mockResolvedValue({
      ...result(newer),
      plans: [{ ...result().plans[0], remaining: 20 }],
    });
    view.rerender(<Harness p={newer} />);
    await flush();
    expect(screen.getByText("剩余 20 USD")).toBeInTheDocument();
    await act(async () => resolveOld(result()));
    act(() =>
      mock.listeners.get("provider-quota")!({
        clientId: "codex",
        quota: result(),
      }),
    );
    expect(screen.queryByText("剩余 8.5 USD")).not.toBeInTheDocument();
    expect(screen.getByText("剩余 20 USD")).toBeInTheDocument();
  });
  it.each(["codex", "claude"] as const)(
    "isolates %s quota events even when another client has the same provider id and version",
    async (clientId) => {
      render(<Harness clientId={clientId} />);
      await flush();
      expect(mock.command).toHaveBeenCalledWith("query_provider_quota", {
        clientId,
        providerId: provider.id,
        force: false,
      });
      act(() =>
        mock.listeners.get("provider-quota")!({
          clientId: clientId === "codex" ? "claude" : "codex",
          quota: {
            ...result(),
            plans: [{ ...result().plans[0], remaining: 999 }],
            state: "error",
            stale: true,
            error: "另一个客户端失败",
          },
        }),
      );
      expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
      expect(screen.queryByText("剩余 999 USD")).not.toBeInTheDocument();
      expect(screen.queryByText("另一个客户端失败")).not.toBeInTheDocument();
      act(() =>
        mock.listeners.get("provider-quota")!({
          clientId,
          quota: {
            ...result(),
            plans: [{ ...result().plans[0], remaining: 23 }],
          },
        }),
      );
      expect(screen.getByText("剩余 23 USD")).toBeInTheDocument();
      expect(screen.queryByText("剩余 8.5 USD")).not.toBeInTheDocument();
    },
  );
  it("distinguishes unsupported, unlimited, empty balance and retry cooldown", () => {
    const q = {
      ...result(),
      state: "unsupported" as const,
      plans: [],
      error: "不可查询",
    };
    const view = render(
      <QuotaInfo provider={provider} quota={q} refresh={() => {}} />,
    );
    expect(screen.getAllByText("不可查询").length).toBeGreaterThan(0);
    expect(screen.queryByText(/剩余 0/)).not.toBeInTheDocument();
    view.rerender(
      <QuotaInfo
        provider={provider}
        quota={{
          ...result(),
          plans: [{ ...result().plans[0], remaining: null, unlimited: true }],
        }}
        refresh={() => {}}
      />,
    );
    expect(screen.getAllByText("无限制").length).toBeGreaterThan(0);
    view.rerender(
      <QuotaInfo
        provider={provider}
        quota={{
          ...result(),
          plans: [
            {
              ...result().plans[0],
              remaining: null,
              used: null,
              total: null,
            },
          ],
        }}
        refresh={() => {}}
      />,
    );
    expect(screen.getByText("剩余 未返回剩余额度")).toBeInTheDocument();
    expect(screen.queryByText("剩余 0 USD")).not.toBeInTheDocument();
    expect(
      within(screen.getByRole("region", { name: "额度窗口" })).getByRole(
        "row",
        { name: /Key 配额/ },
      ),
    ).toHaveTextContent("—");
    view.rerender(
      <QuotaInfo
        provider={provider}
        quota={{
          ...result(),
          retryAt: Date.now() / 1000 + 120,
          plans: [{ ...result().plans[0], remaining: 0 }],
        }}
        refresh={() => {}}
      />,
    );
    expect(screen.getByText("剩余 0 USD")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: `刷新 ${provider.name} 额度` }),
    ).toBeDisabled();
  });
});

it("waits the configured interval after failed automatic refreshes and preserves cached data", async () => {
  vi.useFakeTimers();
  vi.setSystemTime(new Date("2026-10-25T00:00:00Z"));
  provider.quota = { ...result(), nextRefreshAt: Date.now() / 1000 - 1, retryAt: Date.now() / 1000 - 2 };
  mock.command.mockRejectedValue(new Error("fixture quota failure"));
  render(<Harness />);
  await flush();
  expect(mock.command).toHaveBeenCalledTimes(1);
  expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
  await act(async () => { await vi.advanceTimersByTimeAsync(59_999); });
  expect(mock.command).toHaveBeenCalledTimes(1);
  await act(async () => { await vi.advanceTimersByTimeAsync(1); });
  expect(mock.command).toHaveBeenCalledTimes(2);
});

it("enables manual refresh exactly when backoff expires with automatic refresh off", async () => {
  vi.useFakeTimers();
  provider.quota = { ...result(), state: "error", retryAt: Date.now() / 1000 + 120 };
  render(<Harness refreshSeconds={0} />);
  const button = screen.getByRole("button", { name: `刷新 ${provider.name} 额度` });
  expect(button).toBeDisabled();
  await act(async () => { await vi.advanceTimersByTimeAsync(119_999); });
  expect(button).toBeDisabled();
  await act(async () => { await vi.advanceTimersByTimeAsync(1); });
  expect(button).toBeEnabled();
  expect(mock.command).not.toHaveBeenCalled();
  fireEvent.click(button);
  await flush();
  expect(mock.command).toHaveBeenCalledTimes(1);
});
