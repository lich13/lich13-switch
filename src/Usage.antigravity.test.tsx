import UsageAntigravity from "./UsageAntigravity";
import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  AntigravityUsageStats,
  Dashboard,
  UsageFilter,
  UsageRecord,
  UsageSettings,
  UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, Set<(value: unknown) => void>>(),
}));

vi.mock("./bridge", () => ({
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    const listeners = mock.listeners.get(name) ?? new Set();
    listeners.add(listener);
    mock.listeners.set(name, listeners);
    return () => listeners.delete(listener);
  }),
}));

vi.mock("./confirmation", () => ({ confirmAction: vi.fn(async () => true) }));

import Usage from "./Usage";
import UsageLogTable from "./UsageLogTable";
import Detail from "./UsageDetail";
import Sources from "./UsageSources";

const settings: UsageSettings = {
  recording: true,
  autoSync: false,
  refreshSeconds: 0,
  multiplier: "1",
  pricingModel: "response",
};

function record(): UsageRecord {
  return {
    id: "fixture-request",
    client: "antigravity",
    source: "antigravity",
    startedAt: 1767225600000,
    finishedAt: 1767225602000,
    completed: true,
    estimatedSpeed: false,
    sessionId: null,
    duplicateOf: null,
    deduplication: "",
    attempts: [
      {
        id: "fixture-attempt",
        provider: null,
        requestedModel: null,
        responseModel: "fixture-response",
        modelDisplayName: "Fixture Model",
        reasoningEffort: "high",
        apiProvider: "API_PROVIDER_FIXTURE",
        pricingModel: "fixture-response",
        responseId: null,
        status: null,
        outcome: "success",
        startedAt: 1767225600000,
        durationMs: 2000,
        firstTokenMs: 250,
        generationMs: 1750,
        stream: true,
        transport: "local_api",
        tokens: {
          input: 100,
          output: 20,
          cacheRead: 40,
          cacheWrite: null,
          reasoning: 12,
        },
        price: null,
        serviceTier: null,
        availability: { duration: "reported", first_token: "reported" },
      },
    ],
  };
}

it("shows the display model and measured app performance without a fabricated HTTP status", () => {
  render(
    <UsageLogTable
      rows={[record()]}
      settings={settings}
      name={() => "本机会话"}
      open={vi.fn()}
      busy={false}
    />,
  );
  const table = screen.getByRole("table", { name: "请求记录" });
  expect(within(table).getByText("Fixture Model")).toBeInTheDocument();
  expect(within(table).getByText("思考 High")).toBeInTheDocument();
  expect(within(table).getByText("0.25s")).toBeInTheDocument();
  expect(within(table).getByText("2.00s")).toBeInTheDocument();
  expect(within(table).getByText("10.0")).toBeInTheDocument();
  expect(within(table).getByText("未定价")).toBeInTheDocument();
  expect(within(table).queryByText("HTTP 200")).not.toBeInTheDocument();
});

it("leaves missing time and speed empty even in legacy app rows", () => {
  const r = record();
  const a = r.attempts[0];
  a.durationMs = 0;
  a.firstTokenMs = null;
  a.generationMs = null;
  a.availability = {};
  a.parseReasons = ["attempt_time_unreported"];
  render(
    <UsageLogTable
      rows={[r]}
      settings={settings}
      name={() => "本机会话"}
      open={vi.fn()}
      busy={false}
    />,
  );
  expect(screen.queryByText("0.00s")).not.toBeInTheDocument();
  expect(screen.queryByText("0.0")).not.toBeInTheDocument();
});

it("shows real response identity, service, request outcome and separate generation speed in details", () => {
  const r = record();
  r.attempts[0].outcome = "failed";
  r.completed = false;
  render(<Detail record={r} name={() => "本机会话"} onClose={vi.fn()} />);
  expect(screen.getByText("请求失败")).toBeInTheDocument();
  expect(screen.getByText("API_PROVIDER_FIXTURE")).toBeInTheDocument();
  expect(screen.getByText("0.250s")).toBeInTheDocument();
  expect(screen.getByText("2.000s")).toBeInTheDocument();
  expect(screen.getByText("1.750s")).toBeInTheDocument();
  expect(screen.getByText("10.0 tok/s")).toBeInTheDocument();
  expect(screen.getByText("11.4 tok/s")).toBeInTheDocument();
});

it("runs explicit app history repair through the existing synchronization command", async () => {
  const state = {
    settings,
    syncing: false,
    reports: {},
    error: null,
  } as UsageState;
  mock.command.mockResolvedValue(state);
  const changed = vi.fn();
  render(
    <Sources
      state={state}
      counts={{}}
      historyIncomplete={false}
      onClose={vi.fn()}
      onChange={changed}
    />,
  );
  await userEvent.click(
    screen.getByRole("button", { name: "补全 Antigravity 历史" }),
  );
  expect(mock.command).toHaveBeenCalledWith("sync_usage", {
    rebuild: "antigravity",
  });
  expect(changed).toHaveBeenCalledWith(state);
});

const SESSION_ALPHA =
  "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";

function buildStats(
  overrides: Partial<AntigravityUsageStats> = {},
): AntigravityUsageStats {
  return {
    requests: 15,
    sessions: 2,
    costUsd: "1.2450",
    unpricedRequests: 0,
    tokenMix: {
      input: 45_000,
      cacheRead: 180_000,
      cacheWrite: 5_000,
      output: 30_000,
      visibleOutput: 13_800,
      reasoning: 16_200,
      total: 260_000,
      thinkingSharePercent: 54.0,
      cacheHitPercent: 78.3,
      cacheReadEligible: 180_000,
      cacheInputEligible: 230_000,
      cacheSavingsUsd: "0.4860",
    },
    quotaError: null,
    accounts: [
      {
        id: "ag-oauth-1",
        name: "Fixture OAuth",
        kind: "oauth",
        total: {
          period: null,
          requests: 15,
          tokens: 260000,
          reasoningTokens: 16200,
          costUsd: "1.2450",
          unpricedRequests: 0,
          tokensPerHour: 0,
        },
        tracks: [
          {
            id: "gemini",
            label: "Gemini",
            burst5h: {
              period: {
                start: 1_760_000_000_000 - 5 * 3_600_000,
                end: 1_760_000_000_000,
              },
              requests: 6,
              tokens: 110_000,
              reasoningTokens: 7_200,
              costUsd: "0.4200",
              unpricedRequests: 0,
              tokensPerHour: 22_000,
            },
            weekly7d: {
              period: {
                start: 1_760_000_000_000 - 168 * 3_600_000,
                end: 1_760_000_000_000,
              },
              requests: 10,
              tokens: 175_000,
              reasoningTokens: 10_400,
              costUsd: "0.6850",
              unpricedRequests: 0,
              tokensPerHour: 1_041.7,
            },
          },
          {
            id: "claude_gpt",
            label: "Claude / GPT",
            burst5h: {
              period: {
                start: 1_760_000_000_000 - 5 * 3_600_000,
                end: 1_760_000_000_000,
              },
              requests: 3,
              tokens: 52_000,
              reasoningTokens: 3_600,
              costUsd: "0.3500",
              unpricedRequests: 0,
              tokensPerHour: 10_400,
            },
            weekly7d: {
              period: {
                start: 1_760_000_000_000 - 168 * 3_600_000,
                end: 1_760_000_000_000,
              },
              requests: 5,
              tokens: 85_000,
              reasoningTokens: 5_800,
              costUsd: "0.5600",
              unpricedRequests: 0,
              tokensPerHour: 506.0,
            },
          },
        ],
      },
    ],
    providers: [
      {
        id: "ag-oauth-1",
        requests: 10,
        tokens: 175_000,
        reasoningTokens: 10_400,
        costUsd: "0.6850",
        unpricedRequests: 0,
        sharePercent: 67.3,
      },
      {
        id: "ag-adc-1",
        requests: 5,
        tokens: 85_000,
        reasoningTokens: 5_800,
        costUsd: "0.5600",
        unpricedRequests: 0,
        sharePercent: 32.7,
      },
    ],
    families: [
      {
        family: "gemini",
        label: "Gemini",
        requests: 10,
        tokens: 175_000,
        reasoningTokens: 10_400,
        costUsd: "0.6850",
        sharePercent: 67.3,
      },
      {
        family: "claude",
        label: "Claude",
        requests: 5,
        tokens: 85_000,
        reasoningTokens: 5_800,
        costUsd: "0.5600",
        sharePercent: 32.7,
      },
    ],
    topSessions: [
      {
        sessionId: SESSION_ALPHA,
        title: "示例会话",
        requests: 9,
        primaryModel: "gemini-3.1-pro",
        family: "gemini",
        totalTokens: 158_000,
        reasoningTokens: 9_600,
        cacheHitPercent: 80.5,
        durationMs: 38_400,
        costUsd: "0.6120",
        unpricedRequests: 0,
        startedAt: Date.UTC(2026, 9, 30, 7, 0, 0),
        lastActiveAt: Date.UTC(2026, 9, 30, 8, 30, 0),
      },
    ],
    ...overrides,
  };
}

describe("Usage Antigravity statistics tab", () => {
  let statsPayload: AntigravityUsageStats;
  let lastLogFilters: UsageFilter[];

  beforeEach(() => {
    mock.command.mockReset();
    mock.listeners.clear();
    statsPayload = buildStats();
    lastLogFilters = [];
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "visible",
    });

    const dashboard: Dashboard = {
      totals: {
        requests: 15,
        success: 15,
        statusKnown: 0,
        sessions: 2,
        tokens: {
          input: 45_000,
          output: 30_000,
          cacheRead: 180_000,
          cacheWrite: 5_000,
        },
        cost: "1.2450",
        unpriced: 0,
        durationMs: 40_000,
        measuredOutputs: 30_000,
        generationMs: 35_000,
      },
      providers: [],
      models: [],
      precision: "millisecond",
      detailSince: 0,
      sources: { antigravity: 15 },
    };

    const usageState: UsageState = {
      settings: {
        recording: true,
        autoSync: true,
        refreshSeconds: 0,
        multiplier: "1",
        pricingModel: "response",
      },
      syncing: false,
      reports: {},
      error: null,
    };

    const sampleRow: UsageRecord = {
      id: "ag-req-1",
      client: "antigravity",
      source: "antigravity",
      startedAt: Date.UTC(2026, 9, 30, 8, 29, 0),
      finishedAt: Date.UTC(2026, 9, 30, 8, 29, 4),
      sessionId: SESSION_ALPHA,
      completed: true,
      estimatedSpeed: false,
      duplicateOf: null,
      deduplication: "独立记录",
      attempts: [
        {
          id: "ag-req-1-final",
          provider: null,
          requestedModel: "gemini-3.1-pro",
          responseModel: "gemini-3.1-pro",
          pricingModel: "gemini-3.1-pro",
          modelDisplayName: "Gemini 3.1 Pro",
          reasoningEffort: "high",
          responseId: null,
          status: null,
          outcome: "success",
          startedAt: Date.UTC(2026, 9, 30, 8, 29, 0),
          durationMs: 4_000,
          firstTokenMs: 320,
          stream: true,
          transport: "local_api",
          tokens: {
            input: 4_000,
            output: 1_200,
            reasoning: 600,
            cacheRead: 16_000,
            cacheWrite: 0,
          },
          serviceTier: null,
          price: {
            version: "seed",
            source: "sub2api",
            model: "gemini-3.1-pro",
            multiplier: "1",
            rates: {},
            cost: "0.0252",
          },
        },
      ],
    };

    mock.command.mockImplementation(
      async (name: string, args?: Record<string, unknown>) => {
        if (name === "get_usage_dashboard") return dashboard;
        if (name === "get_usage_state") return usageState;
        if (name === "get_gateway") return { providers: [] };
        if (name === "get_antigravity_auth_state")
          return {
            mode: "oauth",
            enableAdc: false,
            accounts: [
              {
                id: "ag-oauth-1",
                name: "oauth@example.invalid",
                kind: "oauth",
                current: true,
              },
              { id: "ag-adc-1", name: "ADC 凭据 · p1", kind: "adc" },
            ],
          };
        if (
          name === "get_antigravity_usage_stats" ||
          name === "get_claude_usage_stats"
        )
          return statsPayload;
        if (name === "get_claude_usage_quota")
          return {
            account: "f***@example.invalid",
            plan: "Max",
            windows: [
              {
                id: "five_hour",
                label: "当前 5h",
                usedPercent: 22.5,
                resetsAt: null,
              },
            ],
            fetchedAt: Date.now(),
            retryAt: null,
            error: null,
          };
        if (name === "get_usage_logs") {
          const filter = (args?.filter ?? {}) as UsageFilter;
          lastLogFilters.push(filter);
          const rows =
            !filter.sessionId || filter.sessionId === SESSION_ALPHA
              ? [sampleRow]
              : [];
          return {
            rows,
            total: rows.length,
            page: 1,
            detailSince: 0,
          };
        }
        throw new Error(`unexpected command: ${name}`);
      },
    );
  });

  it("locks each independent page to its client and clears incompatible filters", async () => {
    render(<Usage />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("tab", { name: "Antigravity" }));
    expect(screen.getByLabelText("用量客户端")).toBeDisabled();
    expect(screen.getByLabelText("用量客户端")).toHaveValue("antigravity");
    await user.selectOptions(screen.getByLabelText("用量供应商"), "ag-oauth-1");
    await user.selectOptions(screen.getByLabelText("请求类型"), "compaction");
    await user.click(screen.getByRole("tab", { name: "Claude Code" }));
    await screen.findByRole("region", { name: "Claude 官方额度" });
    expect(screen.getByLabelText("用量客户端")).toHaveValue("claude");
    expect(screen.getByLabelText("用量供应商")).toHaveValue("");
    expect(mock.command).toHaveBeenCalledWith("get_claude_usage_stats", {
      filter: expect.objectContaining({
        client: "claude",
        operation: "compaction",
        provider: undefined,
      }),
    });
    await user.click(
      screen.getByRole("button", { name: "查看会话 示例会话 请求" }),
    );
    await waitFor(() =>
      expect(lastLogFilters.at(-1)).toMatchObject({
        client: "claude",
        sessionId: SESSION_ALPHA,
        operation: "compaction",
      }),
    );
    await user.click(screen.getByRole("tab", { name: "Antigravity" }));
    expect(
      screen.queryByRole("button", { name: /清除会话筛选/ }),
    ).not.toBeInTheDocument();
    expect(mock.command).toHaveBeenCalledWith("get_antigravity_usage_stats", {
      filter: expect.objectContaining({
        client: "antigravity",
        operation: "compaction",
        sessionId: undefined,
      }),
    });
  });

  it("keeps local statistics usable on quota errors and stops hidden refreshes", async () => {
    const original = mock.command.getMockImplementation()!;
    mock.command.mockImplementation(
      (name: string, args?: Record<string, unknown>) =>
        name === "get_claude_usage_quota"
          ? Promise.reject(new Error("Claude 登录已失效"))
          : original(name, args),
    );
    render(<Usage />);
    const user = userEvent.setup();
    await user.click(screen.getByRole("tab", { name: "Claude Code" }));
    expect(await screen.findByText("Claude 登录已失效")).toBeInTheDocument();
    expect(
      screen.getByRole("region", { name: "Token 构成与缓存节省" }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "刷新用量" }));
    expect(mock.command).toHaveBeenCalledWith("get_claude_usage_quota", {
      refresh: true,
    });
    const reads = () =>
      mock.command.mock.calls.filter(
        ([name]) => name === "get_claude_usage_quota",
      ).length;
    await act(async () =>
      mock.listeners
        .get("app-visibility")
        ?.forEach((listener) => listener(false)),
    );
    const hiddenReads = reads();
    await act(async () =>
      mock.listeners
        .get("claude-login-state")
        ?.forEach((listener) => listener({ phase: "complete" })),
    );
    expect(reads()).toBe(hiddenReads);
    await act(async () =>
      mock.listeners
        .get("app-visibility")
        ?.forEach((listener) => listener(true)),
    );
    expect(reads()).toBe(hiddenReads + 1);
  });

  it("lazily fetches Antigravity stats on tab selection, displays dual-track windows & token mix, and filters logs by session title", async () => {
    const user = userEvent.setup();
    render(<Usage />);

    await waitFor(() => {
      expect(mock.command).toHaveBeenCalledWith(
        "get_usage_logs",
        expect.anything(),
      );
    });
    expect(
      mock.command.mock.calls.some(
        ([name]) => name === "get_antigravity_usage_stats",
      ),
    ).toBe(false);

    await user.click(screen.getByRole("tab", { name: "Antigravity" }));

    await waitFor(() => {
      expect(mock.command).toHaveBeenCalledWith(
        "get_antigravity_usage_stats",
        expect.anything(),
      );
    });

    const summary = await screen.findByLabelText("Token 构成与缓存节省");
    expect(within(summary).getByText("54.0%")).toBeInTheDocument();
    expect(within(summary).getByText("78.3%")).toBeInTheDocument();
    expect(within(summary).getByText("$0.4860")).toBeInTheDocument();

    const tracks = screen.getByLabelText("账号周期用量");
    expect(within(tracks).getAllByText("当前 5h")).toHaveLength(2);
    expect(within(tracks).getAllByText("当前 7d")).toHaveLength(2);

    const creds = screen.getByLabelText("账号与凭据分布");
    expect(
      within(creds).getByText("oauth@example.invalid (OAuth)"),
    ).toBeInTheDocument();
    expect(within(creds).getByText("ADC 凭据 · p1 (ADC)")).toBeInTheDocument();
    await user.click(
      within(creds).getByRole("button", {
        name: "按 oauth@example.invalid (OAuth) 筛选",
      }),
    );

    const sessions = screen.getByLabelText("会话排行");
    expect(within(sessions).getByText("示例会话")).toBeInTheDocument();
    expect(within(sessions).getByText("gemini-3.1-pro")).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "查看会话 示例会话 请求" }),
    );

    await waitFor(() => {
      expect(
        screen.getByRole("tab", { name: "请求日志", selected: true }),
      ).toBeInTheDocument();
      expect(lastLogFilters.at(-1)).toMatchObject({
        client: "antigravity",
        provider: "ag-oauth-1",
        sessionId: SESSION_ALPHA,
      });
    });

    const chip = screen.getByRole("button", {
      name: "清除会话筛选 示例会话",
    });
    expect(chip).toBeInTheDocument();
    await user.click(chip);

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "清除会话筛选 示例会话" }),
      ).not.toBeInTheDocument();
      expect(lastLogFilters.at(-1)?.sessionId).toBeUndefined();
    });
  });

  it("formats unpriced requests as 未定价 across summary, track windows, and session rows", async () => {
    statsPayload = buildStats({
      costUsd: "0",
      unpricedRequests: 15,
      accounts: buildStats().accounts.map((a) => ({
        ...a,
        tracks: a.tracks.map((t) => ({
          ...t,
          burst5h: {
            ...t.burst5h,
            costUsd: "0",
            unpricedRequests: t.burst5h.requests,
          },
          weekly7d: {
            ...t.weekly7d,
            costUsd: "0",
            unpricedRequests: t.weekly7d.requests,
          },
        })),
      })),
      topSessions: buildStats().topSessions.map((s) => ({
        ...s,
        costUsd: "0",
        unpricedRequests: s.requests,
      })),
    });
    const user = userEvent.setup();
    render(<Usage />);

    await user.click(screen.getByRole("tab", { name: "Antigravity" }));

    const summary = await screen.findByLabelText("Token 构成与缓存节省");
    expect(within(summary).getByText("未定价")).toBeInTheDocument();
    const tracks = screen.getByLabelText("账号周期用量");
    expect(within(tracks).getAllByText("未定价")).toHaveLength(4);
    const sessions = screen.getByLabelText("会话排行");
    expect(within(sessions).getByText("未定价")).toBeInTheDocument();
  });

  it("follows the active authentication mode without changing the global usage filter", async () => {
    const extra = (["vertex_key", "adc"] as const).map((kind) => ({
      ...statsPayload.accounts[0],
      id: kind,
      name: kind,
      kind,
      tracks: [],
    }));
    statsPayload = {
      ...statsPayload,
      accounts: [...statsPayload.accounts, ...extra],
    };
    const user = userEvent.setup();
    render(<Usage />);
    await user.click(screen.getByRole("tab", { name: "Antigravity" }));
    const select = await screen.findByRole("combobox", {
      name: "Antigravity 用量账号",
    });
    await waitFor(() => expect(select).toHaveValue("ag-oauth-1"));
    const accounts = statsPayload.accounts.map((a) => ({
      ...a,
      current: true,
    }));
    for (const mode of ["vertex_key", "adc"] as const) {
      const count = mock.command.mock.calls.filter(
        ([name]) => name === "get_antigravity_usage_stats",
      ).length;
      act(() =>
        mock.listeners
          .get("antigravity-auth-state")
          ?.forEach((listener) =>
            listener({ accounts, mode, enableAdc: mode === "adc" }),
          ),
      );
      await waitFor(() => expect(select).toHaveValue(mode));
      await waitFor(() =>
        expect(
          mock.command.mock.calls.filter(
            ([name]) => name === "get_antigravity_usage_stats",
          ).length,
        ).toBeGreaterThan(count),
      );
    }
    const count = mock.command.mock.calls.length;
    await user.selectOptions(select, "ag-oauth-1");
    expect(mock.command.mock.calls).toHaveLength(count);
    expect(screen.getByRole("combobox", { name: "用量供应商" })).toHaveValue(
      "",
    );
  });
});

it("selects one account, retains manual selection on refresh, follows the current account and handles deletion", async () => {
  const user = userEvent.setup();
  const base = buildStats();
  const unavailable = {
    ...base.accounts[0],
    id: "b",
    name: "Fixture B",
    tracks: base.accounts[0].tracks.map((t) => ({
      ...t,
      burst5h: { ...t.burst5h, period: null },
      weekly7d: { ...t.weekly7d, period: null },
    })),
  };
  const accounts = [
    ...base.accounts,
    unavailable,
    ...(["vertex_key", "adc"] as const).map((kind) => ({
      ...base.accounts[0],
      id: kind,
      name: kind,
      kind,
      tracks: [],
    })),
  ];
  const props = {
    stats: { ...base, accounts },
    busy: false,
    currentAccountId: "ag-oauth-1",
    onSelectSession: vi.fn(),
  };
  const view = render(<UsageAntigravity {...props} />);
  const select = screen.getByRole("combobox", { name: "Antigravity 用量账号" });
  expect(select).toHaveValue("ag-oauth-1");
  expect(screen.queryByLabelText("Fixture B")).not.toBeInTheDocument();
  const known = screen.getByLabelText("Fixture OAuth");
  expect(within(known).getAllByText("当前 5h")).toHaveLength(2);
  expect(within(known).getAllByText("当前 7d")).toHaveLength(2);
  expect(known.querySelectorAll(".usage-ag-period")).toHaveLength(4);
  await user.selectOptions(select, "b");
  expect(screen.queryByLabelText("Fixture OAuth")).not.toBeInTheDocument();
  view.rerender(<UsageAntigravity {...props} stats={{ ...props.stats }} />);
  expect(select).toHaveValue("b");
  expect(
    within(screen.getByLabelText("Fixture B")).getAllByText("未获取当前周期"),
  ).toHaveLength(4);
  for (const kind of ["vertex_key", "adc"]) {
    await user.selectOptions(select, kind);
    const section = screen.getByLabelText(kind);
    expect(within(section).getByText(/所选日期/)).toBeInTheDocument();
    expect(within(section).queryByText("当前 5h")).not.toBeInTheDocument();
    expect(within(section).getByText("请求")).toBeInTheDocument();
  }
  expect(screen.getByLabelText("Token 构成与缓存节省")).toHaveTextContent(
    "$1.2450",
  );
  view.rerender(<UsageAntigravity {...props} currentAccountId="b" />);
  expect(select).toHaveValue("b");
  await user.selectOptions(select, "adc");
  view.rerender(
    <UsageAntigravity
      {...props}
      currentAccountId="b"
      stats={{ ...base, accounts: accounts.filter((a) => a.id !== "adc") }}
    />,
  );
  expect(select).toHaveValue("b");
  view.rerender(<UsageAntigravity {...props} currentAccountId={null} />);
  expect(select).toHaveValue("");
  expect(screen.queryByLabelText("Fixture B")).not.toBeInTheDocument();
  view.rerender(
    <UsageAntigravity
      {...props}
      currentAccountId={null}
      stats={{ ...base, accounts: [] }}
    />,
  );
  expect(select).toBeDisabled();
  expect(screen.getByRole("option", { name: "暂无账号" })).toBeInTheDocument();
});

it("shows missing Claude reasoning and snapshot savings as unknown", () => {
  const stats = buildStats();
  stats.tokenMix.reasoningReported = false;
  stats.tokenMix.thinkingSharePercent = null;
  stats.tokenMix.cacheSavingsUsd = null;
  stats.topSessions[0].reasoningReported = false;
  render(
    <UsageAntigravity
      client="claude"
      stats={stats}
      busy={false}
      currentAccountId={null}
      onSelectSession={vi.fn()}
    />,
  );
  const mix = screen.getByRole("region", { name: "Token 构成与缓存节省" });
  expect(within(mix).queryByText("可见输出")).not.toBeInTheDocument();
  expect(within(mix).getByText("输出")).toBeInTheDocument();
  expect(within(mix).getAllByText("—")).toHaveLength(2);
  expect(
    screen.queryByLabelText("Antigravity 用量账号"),
  ).not.toBeInTheDocument();
});
