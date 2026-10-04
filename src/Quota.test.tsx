import { act, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ClientId, Provider, ProviderQuota } from "./types";
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
      <button onClick={() => q.refreshAll()}>全部刷新</button>
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
    fireEvent.click(screen.getByText("额度详情"));
    expect(screen.getByText("已用 1.5 USD / 10 USD")).toBeInTheDocument();
  });
  it("keeps previous successful values visible with a stale error, without inventing zero", async () => {
    render(<Harness />);
    await flush();
    act(() =>
      mock.listeners.get("provider-quota")!({
        clientId: "codex",
        quota: {
          ...result(),
          state: "error",
          stale: true,
          error: "上游连接超时",
        },
      }),
    );
    expect(screen.getByText("剩余 8.5 USD")).toBeInTheDocument();
    expect(screen.getByText("已过期")).toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("上游连接超时");
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
