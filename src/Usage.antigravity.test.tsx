import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  AntigravityUsageStats,
  Dashboard,
  UsageFilter,
  UsageRecord,
  UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
}));

vi.mock("./bridge", () => ({
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));

vi.mock("./confirmation", () => ({ confirmAction: vi.fn(async () => true) }));

import Usage from "./Usage";

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
    tracks: [
      {
        id: "gemini",
        label: "Gemini",
        burst5h: {
          requests: 6,
          tokens: 110_000,
          reasoningTokens: 7_200,
          costUsd: "0.4200",
          unpricedRequests: 0,
          tokensPerHour: 22_000,
        },
        weekly7d: {
          requests: 10,
          tokens: 175_000,
          reasoningTokens: 10_400,
          costUsd: "0.6850",
          unpricedRequests: 0,
          tokensPerHour: 1_041.7,
        },
        liveQuota: {
          label: "Gemini Pro",
          remainingPercent: 72.5,
          usedPercent: 27.5,
          resetAt: Date.UTC(2026, 9, 30, 12, 0, 0),
        },
      },
      {
        id: "claude_gpt",
        label: "Claude / GPT",
        burst5h: {
          requests: 3,
          tokens: 52_000,
          reasoningTokens: 3_600,
          costUsd: "0.3500",
          unpricedRequests: 0,
          tokensPerHour: 10_400,
        },
        weekly7d: {
          requests: 5,
          tokens: 85_000,
          reasoningTokens: 5_800,
          costUsd: "0.5600",
          unpricedRequests: 0,
          tokensPerHour: 506.0,
        },
        liveQuota: {
          label: "Claude / GPT",
          remainingPercent: 18.0,
          usedPercent: 82.0,
          resetAt: Date.UTC(2026, 9, 30, 18, 0, 0),
        },
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
        shortHash: "a1b2c3d4",
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
    liveQuotas: [],
    liveConnected: true,
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
        if (name === "get_antigravity_usage_stats") return statsPayload;
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

  it("lazily fetches Antigravity stats on tab selection, displays dual-track windows & token mix, and filters logs by redacted session", async () => {
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

    const tracks = screen.getByLabelText("双轨 5 小时与 7 天滚动窗口");
    expect(within(tracks).getByText("剩余 72.5%")).toBeInTheDocument();
    expect(within(tracks).getByText("剩余 18%")).toBeInTheDocument();
    expect(within(tracks).getAllByText("5h 突发窗口")).toHaveLength(2);
    expect(within(tracks).getAllByText("7d 周滚动")).toHaveLength(2);

    const sessions = screen.getByLabelText("脱敏会话排行");
    expect(within(sessions).getByText("#a1b2c3d4")).toBeInTheDocument();
    expect(within(sessions).getByText("gemini-3.1-pro")).toBeInTheDocument();

    await user.click(
      screen.getByRole("button", { name: "查看会话 #a1b2c3d4 请求" }),
    );

    await waitFor(() => {
      expect(
        screen.getByRole("tab", { name: "请求日志", selected: true }),
      ).toBeInTheDocument();
      expect(lastLogFilters.at(-1)).toMatchObject({
        client: "antigravity",
        sessionId: SESSION_ALPHA,
      });
    });

    const chip = screen.getByRole("button", {
      name: "清除会话筛选 #a1b2c3d4",
    });
    expect(chip).toBeInTheDocument();
    await user.click(chip);

    await waitFor(() => {
      expect(
        screen.queryByRole("button", { name: "清除会话筛选 #a1b2c3d4" }),
      ).not.toBeInTheDocument();
      expect(lastLogFilters.at(-1)?.sessionId).toBeUndefined();
    });
  });

  it("degrades silently without error banner or quota badges when Antigravity app is offline", async () => {
    statsPayload = buildStats({
      liveConnected: false,
      tracks: buildStats().tracks.map((t) => ({ ...t, liveQuota: null })),
      liveQuotas: [],
    });
    const user = userEvent.setup();
    render(<Usage />);

    await user.click(screen.getByRole("tab", { name: "Antigravity" }));

    const tracks = await screen.findByLabelText("双轨 5 小时与 7 天滚动窗口");
    expect(within(tracks).getAllByText("5h 突发窗口")).toHaveLength(2);
    expect(within(tracks).queryByText(/剩余/)).not.toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  });
});
