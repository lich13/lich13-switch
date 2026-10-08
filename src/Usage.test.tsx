import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type {
  Attempt,
  Dashboard,
  PricingConfig,
  PricingView,
  Tokens,
  UsageFilter,
  UsageRecord,
  UsageSettings,
  UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
  confirm: vi.fn(),
}));
vi.mock("./bridge", () => ({
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));
vi.mock("./confirmation", () => ({ confirmAction: mock.confirm }));

import Usage from "./Usage";
import Pricing from "./UsagePricing";
import App from "./App";

const timestamp = Date.UTC(2026, 9, 8, 2);
const absentTokens: Tokens = {
  input: null,
  output: null,
  cacheRead: null,
  cacheWrite: null,
};
const zeroTokens: Tokens = {
  input: 0,
  output: 0,
  cacheRead: 0,
  cacheWrite: 0,
};
const automaticPrices: PricingView["models"] = {
  "gpt-alpha": {
    input_cost_per_token: "0.000001",
    output_cost_per_token: "0.000002",
    cache_read_input_token_cost: "0",
    output_cost_per_image: "0.01",
  },
  "gpt-beta": {
    input_cost_per_token: "0.000003",
    output_cost_per_token: "0.000004",
  },
  "claude-gamma": {
    input_cost_per_token: "0.000005",
    output_cost_per_token: "0.000006",
  },
};

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-final-attempt",
    provider: "provider-final",
    requestedModel: "fixture-request-model",
    responseModel: "fixture-response-model",
    pricingModel: "fixture-response-model",
    responseId: null,
    status: 200,
    outcome: "success",
    startedAt: timestamp,
    durationMs: 5_000,
    firstTokenMs: 1_000,
    stream: true,
    transport: "http",
    tokens: { input: 0, output: 400, cacheRead: 20, cacheWrite: null },
    serviceTier: null,
    price: {
      version: "fixture-price-version",
      source: "fixture",
      model: "fixture-response-model",
      multiplier: "1.5",
      rates: { output_cost_per_token: "0.000002" },
      cost: "0.0045678",
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-request",
    client: "codex",
    source: "proxy",
    startedAt: timestamp,
    sessionId: null,
    attempts: [attempt()],
    completed: true,
    estimatedSpeed: false,
    duplicateOf: null,
    deduplication: "独立记录",
    ...overrides,
  };
}

let records: UsageRecord[];
let totalRows: number;
let dashboard: Dashboard;
let state: UsageState;
let pricing: PricingView;
let pricingRevision: number;

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(true);
  records = [record()];
  totalRows = 1;
  dashboard = {
    totals: {
      requests: 1,
      success: 1,
      statusKnown: 1,
      sessions: 0,
      tokens: { input: 0, output: 400, cacheRead: 20, cacheWrite: null },
      cost: "0.0045678",
      unpriced: 0,
      durationMs: 5_000,
      measuredOutputs: 400,
      generationMs: 4_000,
    },
    trend: [],
    heatmap: [],
    providers: [],
    models: [],
    precision: "millisecond",
    detailSince: 0,
    sources: { proxy: 1 },
  };
  state = {
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
  pricingRevision = 1;
  pricing = {
    config: {
      autoUpdate: true,
      selected: null,
      excluded: [],
      fixed: {},
      aliases: { "fixture-alias": "gpt-alpha" },
    },
    models: structuredClone(automaticPrices),
    version: "fixture-price-version",
    source: "Fixture prices",
    checkedAt: null,
    updatedAt: null,
    error: null,
    syncing: false,
    revision: "fixture-revision-1",
  };
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(async (name: string, args: Record<string, unknown> = {}) => {
    switch (name) {
      case "get_state":
        return {
          accounts: [],
          authRevision: "fixture-auth-revision",
          configRevision: "fixture-config-revision",
          currentState: "missing",
          preferences: { codexHome: "/fixture/codex", cliPath: "", theme: "dark" },
          authSource: {
            provider: "openai",
            credentialStore: "file",
            inlineToken: false,
            envKey: false,
            commandAuth: false,
            requiresOpenaiAuth: true,
            warning: null,
          },
          error: null,
        };
      case "get_login":
        return { phase: "idle", mode: "", url: null, code: null, message: "", callbackReady: false };
      case "get_provider_imports":
        return [];
      case "frontend_ready":
        return undefined;
      case "get_app_events":
        return { items: [], total: 0, page: 1, error: null };
      case "get_gateway":
        return {
          clientId: args.clientId,
          providers: args.clientId === "codex" ? [
            { id: "provider-first", name: "First Provider" },
            { id: "provider-final", name: "Fixture Provider" },
          ] : [],
        };
      case "get_usage_dashboard":
        return structuredClone(dashboard);
      case "get_usage_heatmap":
        return structuredClone(dashboard.heatmap);
      case "get_usage_logs": {
        const filter = args.filter as UsageFilter;
        return { rows: structuredClone(records), total: totalRows, page: filter.page ?? 1, detailSince: 0 };
      }
      case "get_usage_detail":
        return structuredClone(records.find((row) => row.id === args.id));
      case "get_usage_state":
      case "sync_usage":
        return structuredClone(state);
      case "set_usage_settings":
        state = { ...state, settings: structuredClone(args.settings as UsageSettings) };
        return structuredClone(state);
      case "get_pricing":
        return structuredClone(pricing);
      case "configure_pricing": {
        if (args.expectedRevision !== pricing.revision) {
          throw { code: "CONFLICT", message: "价格配置已被外部修改，请重新保存" };
        }
        const config = structuredClone(args.config as PricingConfig);
        pricing = {
          ...pricing,
          config,
          models: { ...structuredClone(automaticPrices), ...config.fixed },
          revision: `fixture-revision-${++pricingRevision}`,
        };
        return structuredClone(pricing);
      }
      default:
        throw new Error(`Unexpected fixture command: ${name}`);
    }
  });
});

afterEach(() => vi.useRealTimers());

function calls(name: string) {
  return mock.command.mock.calls.filter(([command]) => command === name);
}

function lastFilter(): UsageFilter {
  return calls("get_usage_logs").at(-1)![1].filter;
}

async function flush() {
  await act(async () => {
    await Promise.resolve();
    await Promise.resolve();
  });
}

function requestMetric() {
  return screen.getByText("请求", { selector: ".usage-metrics .usage-metric span" }).parentElement!;
}

async function renderUsage() {
  const result = render(<Usage />);
  await waitFor(() => {
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(records.length + 1);
    expect(screen.getByRole("button", { name: "刷新用量" })).toBeEnabled();
  });
  return result;
}

function cancelDialog(dialog: HTMLElement) {
  fireEvent(dialog, new Event("cancel", { bubbles: true, cancelable: true }));
}

describe("usage records", () => {
  it("loads the overview while the request log is still pending", async () => {
    let resolveLogs!: (value: { rows: UsageRecord[]; total: number; page: number; detailSince: number }) => void;
    const logsResponse = new Promise<{ rows: UsageRecord[]; total: number; page: number; detailSince: number }>((resolve) => {
      resolveLogs = resolve;
    });
    mock.command
      .mockImplementationOnce(async () => ({
        ...structuredClone(dashboard),
        totals: { ...dashboard.totals, requests: 7 },
      }))
      .mockImplementationOnce(() => logsResponse);

    render(<Usage />);

    await waitFor(() => expect(requestMetric()).toHaveTextContent("7"));
    expect(calls("get_usage_dashboard")).toHaveLength(1);
    expect(calls("get_usage_logs")).toHaveLength(1);
    expect(screen.getByRole("table").querySelector("tbody")).toBeEmptyDOMElement();

    await act(async () => resolveLogs({ rows: structuredClone(records), total: 1, page: 1, detailSince: 0 }));
    expect(await within(screen.getByRole("table")).findByText("Fixture Provider")).toBeInTheDocument();
  });

  it("loads the annual heatmap only when its disclosure is opened", async () => {
    dashboard.heatmap = [{
      time: new Date(new Date().getFullYear(), 0, 2).getTime(),
      totals: structuredClone(dashboard.totals),
    }];
    await renderUsage();

    expect(calls("get_usage_heatmap")).toHaveLength(0);
    fireEvent.click(screen.getByText(`${new Date().getFullYear()} 年度用量`));
    await waitFor(() => expect(calls("get_usage_heatmap")).toHaveLength(1));
    const args = calls("get_usage_heatmap")[0][1];
    expect(args.filter).toMatchObject({
      start: new Date(new Date().getFullYear(), 0, 1).getTime(),
    });
  });

  it("keeps the previous overview and rows visible until a refresh resolves", async () => {
    await renderUsage();
    let resolveDashboard!: (value: Dashboard) => void;
    const dashboardResponse = new Promise<Dashboard>((resolve) => {
      resolveDashboard = resolve;
    });
    mock.command.mockImplementationOnce(() => dashboardResponse);

    fireEvent.click(screen.getByRole("button", { name: "刷新用量" }));

    expect(requestMetric()).toHaveTextContent("1");
    expect(within(screen.getByRole("table")).getByText("Fixture Provider")).toBeInTheDocument();
    await act(async () => resolveDashboard({
      ...structuredClone(dashboard),
      totals: { ...dashboard.totals, requests: 9 },
    }));
    await waitFor(() => expect(requestMetric()).toHaveTextContent("9"));
    expect(within(screen.getByRole("table")).getByText("Fixture Provider")).toBeInTheDocument();
  });

  it("discards an older overview response after the selected range changes", async () => {
    let resolveStale!: (value: Dashboard) => void;
    const staleResponse = new Promise<Dashboard>((resolve) => {
      resolveStale = resolve;
    });
    mock.command.mockImplementationOnce(() => staleResponse);
    render(<Usage />);

    await waitFor(() => expect(screen.getByRole("table")).toBeInTheDocument());
    dashboard = { ...dashboard, totals: { ...dashboard.totals, requests: 7 } };
    await userEvent.selectOptions(screen.getByRole("combobox", { name: "用量时间范围" }), "7");
    await waitFor(() => expect(requestMetric()).toHaveTextContent("7"));

    await act(async () => resolveStale({
      ...structuredClone(dashboard),
      totals: { ...dashboard.totals, requests: 99 },
    }));
    expect(requestMetric()).toHaveTextContent("7");
  });

  it("keeps unavailable counts and prices distinct from measured zero", async () => {
    records = [
      record({ id: "fixture-missing", attempts: [attempt({ tokens: absentTokens, price: null })] }),
      record({ id: "fixture-zero", attempts: [attempt({ tokens: zeroTokens, price: { ...attempt().price!, cost: "0", multiplier: "1" } })] }),
    ];
    totalRows = 2;
    dashboard.totals.tokens = absentTokens;
    await renderUsage();
    const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
    const unavailable = within(rows[0]).getAllByRole("cell");
    const zero = within(rows[1]).getAllByRole("cell");
    for (const index of [4, 5, 6]) {
      expect(unavailable[index]).toHaveTextContent(/^未提供$/);
      expect(zero[index]).toHaveTextContent(/^0$/);
    }
    expect(unavailable[7]).toHaveTextContent(/^未定价$/);
    expect(zero[7]).toHaveTextContent(/^\$0\.0000$/);
    expect(unavailable[8]).toHaveTextContent(/^—$/);
    const tokenMetric = screen.getByText("实际 Token").parentElement!;
    expect(within(tokenMetric).getByText("未提供")).toBeInTheDocument();

    dashboard.totals.tokens = zeroTokens;
    fireEvent.click(screen.getByRole("button", { name: "刷新用量" }));
    await waitFor(() => expect(within(tokenMetric).getByText("0")).toBeInTheDocument());
  });

  it("shows nine columns from the final attempt and preserves all attempts in detail", async () => {
    const first = attempt({
      id: "fixture-first-attempt",
      provider: "provider-first",
      requestedModel: "fixture-first-model",
      responseModel: null,
      status: 503,
      outcome: "failure",
      tokens: { ...zeroTokens, input: 9_999 },
      price: { ...attempt().price!, cost: "0.0014322", multiplier: "1" },
    });
    records = [record({ attempts: [first, attempt()] })];
    await renderUsage();
    const table = screen.getByRole("table");
    expect(within(table).getAllByRole("columnheader").map((cell) => cell.textContent)).toEqual([
      "时间", "客户端", "供应商", "模型", "输入", "输出", "缓存", "费用", "速度",
    ]);
    const row = within(table).getAllByRole("row")[1];
    const cells = within(row).getAllByRole("cell");
    expect(cells).toHaveLength(9);
    expect(cells[2]).toHaveTextContent(/^Fixture Provider$/);
    expect(cells[3]).toHaveTextContent("fixture-request-model→ fixture-response-model");
    expect(cells[4]).toHaveTextContent(/^0$/);
    expect(cells[5]).toHaveTextContent(/^400$/);
    expect(cells[6]).toHaveTextContent(/^20$/);
    expect(cells[7]).toHaveTextContent("$0.0046×1.5");
    expect(cells[7]).toHaveAttribute("title", "0.0045678");
    expect(cells[8]).toHaveTextContent(/^100\.0$/);
    expect(within(row).queryByText("First Provider")).not.toBeInTheDocument();
    expect(within(row).queryByTitle("HTTP 503")).not.toBeInTheDocument();

    fireEvent.keyDown(row, { key: "Enter" });
    const dialog = await screen.findByRole("dialog");
    expect(mock.command).toHaveBeenCalledWith("get_usage_detail", { id: "fixture-request" });
    expect(within(dialog).getByText("HTTP 200")).toBeInTheDocument();
    expect(within(dialog).getByText("已完成")).toBeInTheDocument();
    expect(within(dialog).getByText("尝试 · 2")).toBeInTheDocument();
    expect(within(dialog).getByText("全部尝试费用").nextElementSibling).toHaveTextContent("$0.0060");
    const firstSummary = within(dialog).getByText("1 · First Provider · 503");
    fireEvent.click(firstSummary);
    expect(within(firstSummary.parentElement!).getByText("fixture-first-model")).toBeVisible();
    expect(within(firstSummary.parentElement!).getByText("9,999")).toBeVisible();
    expect(within(dialog).getByText("2 · Fixture Provider · 200")).toBeInTheDocument();
  });

  it("returns focus to request and source entry points after dialog cancellation", async () => {
    await renderUsage();
    const requestRow = within(screen.getByRole("table")).getAllByRole("row")[1];
    fireEvent.click(requestRow);
    const detail = await screen.findByRole("dialog");
    cancelDialog(detail);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(requestRow).toHaveFocus());

    const sourcesButton = screen.getByRole("button", { name: "数据来源与设置" });
    fireEvent.click(sourcesButton);
    const sources = await screen.findByRole("dialog");
    cancelDialog(sources);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(sourcesButton).toHaveFocus());
  });

  it("labels per-million rates and keeps zero, media, cache, and tier values", async () => {
    const rates = {
      input_cost_per_token: "0",
      output_cost_per_token: "0.000002",
      cache_read_input_token_cost: "0.000003",
      cache_creation_input_token_cost: "0.000004",
      cache_creation_input_token_cost_above_1hr: "0.000005",
      input_cost_per_image_token: "0.000006",
      output_cost_per_image_token: "0.000007",
      input_cost_per_audio_token: "0.000008",
      output_cost_per_audio_token: "0.000009",
      input_cost_per_token_above_100k_tokens_priority: "0.00001",
      output_cost_per_token_above_200k_tokens_flex: "0.000011",
    };
    records = [record({ attempts: [attempt({ price: { ...attempt().price!, rates } })] })];
    await renderUsage();
    fireEvent.click(within(screen.getByRole("table")).getAllByRole("row")[1]);
    const dialog = await screen.findByRole("dialog");
    const summary = within(dialog).getAllByText("单价 / 百万 Token")[0];
    fireEvent.click(summary);
    const rateList = summary.closest("details")!;
    const pairs = [...rateList.querySelector(".usage-detail-grid")!.children];

    expect(pairs).toHaveLength(11);
    expect(pairs.map((pair) => pair.querySelector("dt")?.textContent)).toEqual([
      "输入",
      "输出",
      "缓存读取",
      "缓存写入",
      "缓存写入（1 小时）",
      "图片输入",
      "图片输出",
      "音频输入",
      "音频输出",
      "输入 · 上下文 > 100K · 优先",
      "输出 · 上下文 > 200K · 弹性",
    ]);
    expect(pairs.every((pair) => pair.classList.contains("usage-detail-pair"))).toBe(true);
    expect(pairs.map((pair) => pair.querySelector("dd")?.textContent)).toEqual([
      "$0", "$2", "$3", "$4", "$5", "$6", "$7", "$8", "$9", "$10", "$11",
    ]);
  });

  it("distinguishes measured speed from session estimates and missing first-token timing", async () => {
    records = [
      record({ id: "fixture-measured" }),
      record({ id: "fixture-no-timing", attempts: [attempt({ firstTokenMs: null })] }),
      record({
        id: "fixture-session",
        source: "codex",
        estimatedSpeed: true,
        attempts: [attempt({ provider: null, firstTokenMs: null, status: null, outcome: "completed" })],
      }),
    ];
    totalRows = records.length;
    await renderUsage();
    const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
    const measured = within(rows[0]).getAllByRole("cell")[8];
    const unavailable = within(rows[1]).getAllByRole("cell")[8];
    const estimated = within(rows[2]).getAllByRole("cell")[8];
    expect(measured).toHaveTextContent(/^100\.0$/);
    expect(measured).toHaveAttribute("title", "Token/s");
    expect(unavailable).toHaveTextContent(/^—$/);
    expect(estimated).toHaveTextContent(/^80\.0（估算）$/);
    expect(estimated).toHaveAttribute("title", "估算 Token/s");

    fireEvent.click(rows[2]);
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("会话完成")).toBeInTheDocument();
    expect(within(dialog).getAllByText("80.0（估算） Token/s")).toHaveLength(2);
  });

  it("jumps only to valid pages and resets the page when filters change", async () => {
    const user = userEvent.setup();
    totalRows = 61;
    await renderUsage();
    const initial = calls("get_usage_logs").length;
    fireEvent.change(screen.getByRole("spinbutton", { name: "跳转页码" }), { target: { value: "5" } });
    fireEvent.keyDown(screen.getByRole("spinbutton", { name: "跳转页码" }), { key: "Enter" });
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(initial);
    fireEvent.change(screen.getByRole("spinbutton", { name: "跳转页码" }), { target: { value: "4" } });
    fireEvent.keyDown(screen.getByRole("spinbutton", { name: "跳转页码" }), { key: "Enter" });
    await waitFor(() => expect(lastFilter().page).toBe(4));
    expect(screen.getByRole("button", { name: "4" })).toHaveAttribute("aria-current", "page");
    expect(screen.getByRole("button", { name: "下一页" })).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "上一页" }));
    await waitFor(() => expect(lastFilter().page).toBe(3));

    await user.selectOptions(screen.getByRole("combobox", { name: "用量客户端" }), "codex");
    await waitFor(() => expect(lastFilter()).toMatchObject({ page: 1, client: "codex" }));
    await user.selectOptions(screen.getByRole("combobox", { name: "用量供应商" }), "provider-final");
    fireEvent.change(screen.getByLabelText("计价模型筛选"), { target: { value: "fixture-response-model" } });
    await waitFor(() => expect(lastFilter().model).toBe("fixture-response-model"));
    await user.selectOptions(screen.getByRole("combobox", { name: "请求状态" }), "5xx");
    await waitFor(() => expect(lastFilter()).toMatchObject({
      page: 1,
      client: "codex",
      provider: "provider-final",
      model: "fixture-response-model",
      status: "5xx",
    }));
    await user.selectOptions(screen.getByRole("combobox", { name: "用量客户端" }), "claude");
    await waitFor(() => {
      expect(lastFilter().client).toBe("claude");
      expect(lastFilter().provider).toBeUndefined();
    });
  });

  it.each([
    { label: "daily detail", stepMs: 86_400_000, precision: "millisecond", reverse: false },
    { label: "compacted daily history", stepMs: 86_400_000, precision: "day", reverse: true },
    { label: "hourly detail", stepMs: 3_600_000, precision: "millisecond", reverse: false },
  ])("includes the entire final $label bucket when dragging the trend", async ({ stepMs, precision, reverse }) => {
    const first = Date.UTC(2026, 9, 5);
    dashboard.trendStepMs = stepMs;
    dashboard.precision = precision;
    dashboard.trend = [0, 1, 2].map((index) => ({
      time: first + index * stepMs,
      totals: structuredClone(dashboard.totals),
    }));
    await renderUsage();
    const chart = screen.getByRole("img", { name: "用量趋势，拖动选择时间范围" });
    vi.spyOn(chart, "getBoundingClientRect").mockReturnValue({
      x: 100, y: 0, left: 100, top: 0, right: 1100, bottom: 175,
      width: 1000, height: 175, toJSON: () => ({}),
    });

    fireEvent(chart, new MouseEvent("pointerdown", {
      bubbles: true, clientX: reverse ? 1080 : 120,
    }));
    fireEvent(chart, new MouseEvent("pointerup", {
      bubbles: true, clientX: reverse ? 120 : 1080,
    }));

    await waitFor(() => expect(lastFilter()).toMatchObject({
      start: first,
      end: first + 3 * stepMs,
      page: 1,
    }));
    expect(mock.command).toHaveBeenCalledWith("get_usage_dashboard", {
      filter: expect.objectContaining({ start: first, end: first + 3 * stepMs }),
    });
    expect(screen.getByRole("combobox", { name: "用量时间范围" })).toHaveValue("custom");
    expect(screen.getByRole("checkbox", { name: "跟随当前" })).not.toBeChecked();
    expect(screen.getByLabelText("结束时间")).toBeEnabled();
  });

  it("pauses periodic reloads for document and native hiding and respects manual refresh", async () => {
    vi.useFakeTimers();
    state.settings.refreshSeconds = 5;
    const view = render(<Usage />);
    await flush();
    const initial = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(5_000));
    expect(calls("get_usage_logs")).toHaveLength(initial + 1);

    Object.defineProperty(document, "visibilityState", { configurable: true, value: "hidden" });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    const documentHidden = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(15_000));
    expect(calls("get_usage_logs")).toHaveLength(documentHidden);
    Object.defineProperty(document, "visibilityState", { configurable: true, value: "visible" });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(documentHidden + 1);

    expect(mock.listeners.has("app-visibility")).toBe(true);
    act(() => mock.listeners.get("app-visibility")!(false));
    const nativeHidden = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(15_000));
    expect(calls("get_usage_logs")).toHaveLength(nativeHidden);
    act(() => mock.listeners.get("app-visibility")!(true));
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(nativeHidden + 1);

    fireEvent.change(screen.getByRole("combobox", { name: "用量刷新频率" }), { target: { value: "0" } });
    await flush();
    expect(state.settings.refreshSeconds).toBe(0);
    const manual = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(30_000));
    expect(calls("get_usage_logs")).toHaveLength(manual);
    fireEvent.click(screen.getByRole("button", { name: "刷新用量" }));
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(manual + 1);
    view.unmount();
  });

  it("saves recording and automatic session sync as independent switches", async () => {
    const user = userEvent.setup();
    await renderUsage();
    await user.click(screen.getByRole("button", { name: "数据来源与设置" }));
    const dialog = screen.getByRole("dialog");
    const recording = within(dialog).getByRole("checkbox", { name: "记录网关用量" });
    const autoSync = within(dialog).getByRole("checkbox", { name: "自动同步会话" });
    expect(recording).toBeChecked();
    expect(autoSync).toBeChecked();
    await user.click(recording);
    expect(autoSync).toBeChecked();
    expect(calls("set_usage_settings")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(state.settings).toMatchObject({ recording: false, autoSync: true }));

    await user.click(autoSync);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(state.settings).toMatchObject({ recording: false, autoSync: false }));
    await user.click(recording);
    expect(autoSync).not.toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(state.settings).toMatchObject({ recording: true, autoSync: false }));
    expect(calls("sync_usage")).toHaveLength(0);
    expect(calls("configure_pricing")).toHaveLength(0);
  });

  it("keeps source settings drafts while a background refresh updates sync status", async () => {
    vi.useFakeTimers();
    state.settings.refreshSeconds = 5;
    const onDirtyChange = vi.fn();
    render(<Usage onDirtyChange={onDirtyChange} />);
    await flush();
    fireEvent.click(screen.getByRole("button", { name: "数据来源与设置" }));
    const dialog = screen.getByRole("dialog");
    const recording = within(dialog).getByRole("checkbox", { name: "记录网关用量" });
    const autoSync = within(dialog).getByRole("checkbox", { name: "自动同步会话" });
    const multiplier = within(dialog).getByRole("spinbutton", { name: "成本倍率" });
    const model = within(dialog).getByRole("combobox", { name: "计价模型" });
    fireEvent.click(recording);
    fireEvent.click(autoSync);
    fireEvent.change(multiplier, { target: { value: "2.5" } });
    fireEvent.change(model, { target: { value: "request" } });
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    state = {
      ...state,
      settings: { ...state.settings, multiplier: "9" },
      reports: { codex: { files: 4, imported: 2, skipped: 0, errors: 2, completedAt: timestamp } },
    };
    await act(async () => vi.advanceTimersByTime(5_000));
    expect(within(dialog).getByText("2 个文件未同步")).toBeInTheDocument();
    expect(recording).not.toBeChecked();
    expect(autoSync).not.toBeChecked();
    expect(multiplier).toHaveValue(2.5);
    expect(model).toHaveValue("request");
    expect(calls("set_usage_settings")).toHaveLength(0);

    fireEvent.click(within(dialog).getByRole("button", { name: "保存" }));
    await flush();
    expect(calls("set_usage_settings")).toHaveLength(1);
    expect(state.settings).toEqual({
      recording: false,
      autoSync: false,
      refreshSeconds: 5,
      multiplier: "2.5",
      pricingModel: "request",
    });
    fireEvent.click(within(dialog).getByRole("button", { name: "关闭" }));
    expect(onDirtyChange).toHaveBeenLastCalledWith(false);
  });

  it("keeps a price draft when tab navigation is cancelled and releases it after confirmation", async () => {
    const user = userEvent.setup();
    const onDirtyChange = vi.fn();
    render(<Usage onDirtyChange={onDirtyChange} />);
    await screen.findByRole("button", { name: "刷新用量" });
    await user.click(screen.getByRole("tab", { name: "定价" }));
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const dialog = screen.getByRole("dialog");
    const input = within(dialog).getByLabelText("输入 · USD / 百万 Token");
    fireEvent.change(input, { target: { value: "2.5" } });
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    mock.confirm.mockResolvedValueOnce(false);
    await user.click(screen.getByRole("tab", { name: "供应商" }));
    expect(mock.confirm).toHaveBeenCalledWith("离开定价会丢弃未保存的修改。");
    expect(screen.getByRole("tab", { name: "定价" })).toHaveAttribute("aria-selected", "true");
    expect(input).toHaveValue("2.5");
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    await user.click(screen.getByRole("tab", { name: "供应商" }));
    await waitFor(() => expect(screen.getByRole("tab", { name: "供应商" })).toHaveAttribute("aria-selected", "true"));
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(onDirtyChange).toHaveBeenLastCalledWith(false);
    expect(calls("configure_pricing")).toHaveLength(0);
  });
});

describe("usage pricing", () => {
  it("filters model prices and preserves zero and missing rate displays", async () => {
    render(<Pricing />);
    await screen.findByRole("button", { name: "gpt-alpha" });
    fireEvent.change(screen.getByRole("textbox", { name: "搜索价格模型" }), { target: { value: "GPT-ALPHA" } });
    const rows = within(screen.getByRole("table")).getAllByRole("row");
    expect(rows).toHaveLength(2);
    const cells = within(rows[1]).getAllByRole("cell");
    expect(cells[1]).toHaveTextContent(/^1$/);
    expect(cells[2]).toHaveTextContent(/^2$/);
    expect(cells[3]).toHaveTextContent(/^0$/);
    expect(cells[4]).toHaveTextContent(/^—$/);
    expect(screen.queryByRole("button", { name: "gpt-beta" })).not.toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "claude-gamma" })).not.toBeInTheDocument();
    expect(calls("configure_pricing")).toHaveLength(0);
  });

  it("fixes a price including zero and restores automatic rates with the latest revision", async () => {
    const user = userEvent.setup();
    pricing.config.excluded = ["claude-gamma"];
    render(<Pricing />);
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const editor = screen.getByRole("dialog");
    fireEvent.change(within(editor).getByLabelText("输入 · USD / 百万 Token"), { target: { value: "0" } });
    fireEvent.change(within(editor).getByLabelText("输出 · USD / 百万 Token"), { target: { value: "2.75" } });
    await user.click(within(editor).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const firstSave = calls("configure_pricing")[0][1];
    expect(firstSave.expectedRevision).toBe("fixture-revision-1");
    expect(firstSave.config.fixed["gpt-alpha"]).toMatchObject({
      input_cost_per_token: "0.000000",
      output_cost_per_token: "0.00000275",
      cache_read_input_token_cost: "0.000000",
      output_cost_per_image: "0.01",
    });
    expect(firstSave.config.fixed["gpt-alpha"]).not.toHaveProperty("cache_creation_input_token_cost");
    expect(firstSave.config.excluded).toEqual(["claude-gamma"]);
    const fixedRow = screen.getByRole("button", { name: "gpt-alpha" }).closest("tr")!;
    expect(within(fixedRow).getByText("固定")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "gpt-alpha" }));
    await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "恢复自动价格" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    const restored = calls("configure_pricing")[1][1];
    expect(restored.expectedRevision).toBe("fixture-revision-2");
    expect(restored.config.fixed).not.toHaveProperty("gpt-alpha");
    expect(restored.config.excluded).toEqual(["claude-gamma"]);
    expect(restored.config.aliases).toEqual({ "fixture-alias": "gpt-alpha" });
    const restoredCells = within(screen.getByRole("button", { name: "gpt-alpha" }).closest("tr")!).getAllByRole("cell");
    expect(restoredCells[1]).toHaveTextContent(/^1$/);
    expect(restoredCells[2]).toHaveTextContent(/^2$/);
    expect(restoredCells[5]).toHaveTextContent(/^Sub2API$/);
  });

  it("returns focus to the price editor and sync entry points after cancellation", async () => {
    render(<Pricing />);
    const modelButton = await screen.findByRole("button", { name: "gpt-alpha" });
    fireEvent.click(modelButton);
    const editor = await screen.findByRole("dialog");
    cancelDialog(editor);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(modelButton).toHaveFocus());

    const syncButton = screen.getByRole("button", { name: "自动同步" });
    fireEvent.click(syncButton);
    const sync = await screen.findByRole("dialog");
    cancelDialog(sync);
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    await waitFor(() => expect(syncButton).toHaveFocus());
  });

  it("changes only filtered sync selections and saves price sync independently", async () => {
    const user = userEvent.setup();
    render(<Pricing />);
    await screen.findByRole("button", { name: "gpt-alpha" });
    await user.click(screen.getByRole("button", { name: "自动同步" }));
    const dialog = screen.getByRole("dialog");
    fireEvent.change(within(dialog).getByRole("textbox", { name: "搜索同步模型" }), { target: { value: "GPT-" } });
    expect(within(dialog).queryByRole("checkbox", { name: /claude-gamma/ })).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "清空结果" }));
    expect(within(dialog).getByRole("checkbox", { name: /gpt-alpha/ })).not.toBeChecked();
    expect(within(dialog).getByRole("checkbox", { name: /gpt-beta/ })).not.toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "全选结果" }));
    await user.click(within(dialog).getByRole("checkbox", { name: /gpt-beta/ }));
    await user.click(within(dialog).getByRole("checkbox", { name: "自动更新" }));
    expect(calls("configure_pricing")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(pricing.config.selected?.slice().sort()).toEqual(["claude-gamma", "gpt-alpha"]);
    expect(pricing.config.autoUpdate).toBe(false);
    expect(pricing.config.excluded).toEqual([]);
    expect(pricing.config.aliases).toEqual({ "fixture-alias": "gpt-alpha" });
    expect(calls("set_usage_settings")).toHaveLength(0);
    expect(calls("sync_usage")).toHaveLength(0);
  });

  it("keeps edited rates through price events and saves alongside the refreshed configuration", async () => {
    const user = userEvent.setup();
    render(<Pricing />);
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const dialog = screen.getByRole("dialog");
    const input = within(dialog).getByLabelText("输入 · USD / 百万 Token");
    fireEvent.change(input, { target: { value: "2.5" } });
    pricing = {
      ...pricing,
      config: { ...pricing.config, excluded: ["claude-gamma"], aliases: { "external-alias": "gpt-beta" } },
      models: { ...pricing.models, "gpt-alpha": { ...pricing.models["gpt-alpha"], input_cost_per_token: "0.000099" } },
      revision: `fixture-revision-${++pricingRevision}`,
    };
    act(() => mock.listeners.get("pricing-state")?.(pricing));
    const row = screen.getByRole("button", { name: "gpt-alpha" }).closest("tr")!;
    await waitFor(() => expect(within(row).getAllByRole("cell")[1]).toHaveTextContent(/^99$/));
    expect(input).toHaveValue("2.5");
    expect(calls("configure_pricing")).toHaveLength(0);

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(calls("configure_pricing")[0][1]).toMatchObject({
      expectedRevision: "fixture-revision-2",
      config: {
        fixed: { "gpt-alpha": { input_cost_per_token: "0.0000025", output_cost_per_image: "0.01" } },
        excluded: ["claude-gamma"],
        aliases: { "external-alias": "gpt-beta" },
      },
    });
  });

  it("retains a failed price draft and retries with the reloaded revision and external prices", async () => {
    const user = userEvent.setup();
    const onDirtyChange = vi.fn();
    render(<Pricing onDirtyChange={onDirtyChange} />);
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const dialog = screen.getByRole("dialog");
    const output = within(dialog).getByLabelText("输出 · USD / 百万 Token");
    fireEvent.change(output, { target: { value: "2.75" } });
    const externalPrice = { input_cost_per_token: "0.000009" };
    pricing = {
      ...pricing,
      config: {
        ...pricing.config,
        fixed: { "gpt-beta": externalPrice },
        aliases: { "external-alias": "gpt-beta" },
      },
      models: { ...pricing.models, "gpt-beta": externalPrice },
      revision: `fixture-revision-${++pricingRevision}`,
    };
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("价格配置已被外部修改，请重新保存");
    expect(calls("get_pricing")).toHaveLength(2);
    expect(calls("configure_pricing")[0][1].expectedRevision).toBe("fixture-revision-1");
    expect(output).toHaveValue("2.75");
    expect(pricing.config.fixed).not.toHaveProperty("gpt-alpha");
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(calls("configure_pricing")).toHaveLength(2);
    expect(calls("configure_pricing")[1][1]).toMatchObject({
      expectedRevision: "fixture-revision-2",
      config: {
        fixed: { "gpt-alpha": { output_cost_per_token: "0.00000275" }, "gpt-beta": externalPrice },
        aliases: { "external-alias": "gpt-beta" },
      },
    });
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(onDirtyChange).toHaveBeenLastCalledWith(false);
  });

  it("retains sync selections through background updates and conflict retry", async () => {
    const user = userEvent.setup();
    render(<Pricing />);
    await screen.findByRole("button", { name: "gpt-alpha" });
    await user.click(screen.getByRole("button", { name: "自动同步" }));
    const dialog = screen.getByRole("dialog");
    const automatic = within(dialog).getByRole("checkbox", { name: "自动更新" });
    const beta = within(dialog).getByRole("checkbox", { name: /gpt-beta/ });
    await user.click(automatic);
    await user.click(beta);
    pricing = {
      ...pricing,
      config: { ...pricing.config, selected: ["gpt-beta"] },
      revision: `fixture-revision-${++pricingRevision}`,
    };
    act(() => mock.listeners.get("pricing-state")?.(pricing));
    await flush();
    expect(calls("get_pricing")).toHaveLength(2);
    expect(automatic).not.toBeChecked();
    expect(beta).not.toBeChecked();
    expect(within(dialog).getByRole("checkbox", { name: /gpt-alpha/ })).toBeChecked();
    expect(calls("configure_pricing")).toHaveLength(0);

    pricing = { ...pricing, revision: `fixture-revision-${++pricingRevision}` };
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent("价格配置已被外部修改，请重新保存");
    expect(calls("configure_pricing")[0][1].expectedRevision).toBe("fixture-revision-2");
    expect(calls("get_pricing")).toHaveLength(3);
    expect(automatic).not.toBeChecked();
    expect(beta).not.toBeChecked();

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
    expect(calls("configure_pricing")[1][1].expectedRevision).toBe("fixture-revision-3");
    expect(pricing.config.autoUpdate).toBe(false);
    expect(pricing.config.selected?.slice().sort()).toEqual(["claude-gamma", "gpt-alpha"]);
  });
});

describe("usage navigation", () => {
  it("opens usage and logs from the sidebar and protects price drafts from page and native navigation", async () => {
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() => expect(mock.command).toHaveBeenCalledWith("frontend_ready"));
    const navigation = screen.getByRole("navigation", { name: "主导航" });
    await user.click(within(navigation).getByRole("button", { name: "用量" }));
    await screen.findByRole("tab", { name: "请求日志" });
    await user.click(screen.getByRole("tab", { name: "定价" }));
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const input = within(screen.getByRole("dialog")).getByLabelText("输入 · USD / 百万 Token");
    fireEvent.change(input, { target: { value: "2.5" } });
    mock.confirm.mockResolvedValue(false);

    await user.click(within(navigation).getByRole("button", { name: "日志" }));
    expect(mock.confirm).toHaveBeenLastCalledWith("离开当前页面会丢弃未保存的表单。");
    await act(async () => mock.listeners.get("navigate")?.("settings"));
    expect(mock.confirm).toHaveBeenLastCalledWith("打开设置会丢弃未保存的草稿。");
    await act(async () => mock.listeners.get("provider-settings")?.({ id: "provider-final", clientId: "codex" }));
    expect(mock.confirm).toHaveBeenLastCalledWith("打开供应商设置会丢弃未保存的草稿。");
    await act(async () => mock.listeners.get("navigate")?.("accounts"));
    expect(mock.confirm).toHaveBeenLastCalledWith("离开当前页面会丢弃未保存的表单。");
    expect(input).toHaveValue("2.5");
    expect(screen.getByRole("tab", { name: "定价" })).toHaveAttribute("aria-selected", "true");
    expect(calls("configure_pricing")).toHaveLength(0);

    mock.confirm.mockResolvedValue(true);
    await user.click(within(navigation).getByRole("button", { name: "日志" }));
    await screen.findByRole("button", { name: "清空日志" });
    expect(calls("get_app_events").length).toBeGreaterThan(0);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const confirmations = mock.confirm.mock.calls.length;
    await user.click(within(navigation).getByRole("button", { name: "用量" }));
    expect(await screen.findByRole("tab", { name: "请求日志" })).toHaveAttribute("aria-selected", "true");
    expect(mock.confirm).toHaveBeenCalledTimes(confirmations);
  });
});
