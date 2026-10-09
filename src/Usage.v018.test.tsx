import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type {
  Attempt,
  Dashboard,
  Group,
  PricingConfig,
  PricingView,
  Totals,
  UsageFilter,
  UsageRecord,
  UsageSettings,
  UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  confirm: vi.fn(),
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
vi.mock("./confirmation", () => ({ confirmAction: mock.confirm }));

import Usage from "./Usage";

const timestamp = Date.UTC(2026, 9, 8, 2);
const fixtureProviders = Array.from({ length: 45 }, (_, index) => ({
  id: `fixture-provider-${index + 1}`,
  name: `Fixture Provider ${index + 1}`,
}));

function totals(overrides: Partial<Totals> = {}): Totals {
  return {
    requests: 12,
    attempts: 14,
    success: 9,
    statusKnown: 10,
    sessions: 2,
    tokens: { input: 1000, output: 200, cacheRead: 300, cacheWrite: 50 },
    cost: "12.34567",
    unpriced: 0,
    durationMs: 90000,
    measuredOutputs: 200,
    generationMs: 12000,
    firstTokenSumMs: 8000,
    firstTokenSamples: 5,
    ...overrides,
  };
}

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-attempt",
    provider: "fixture-provider-1",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: null,
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt: timestamp,
    durationMs: 5000,
    firstTokenMs: 250,
    stream: true,
    transport: "http",
    tokens: { input: 1000, output: 200, cacheRead: 300, cacheWrite: 50 },
    serviceTier: null,
    price: {
      version: "fixture-price-version",
      source: "fixture",
      model: "fixture-priced",
      multiplier: "1",
      rates: { input_cost_per_token: "0.000001" },
      cost: "0.001",
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
    deduplication: "",
    ...overrides,
  };
}

function groups(prefix: "provider" | "model"): Group[] {
  return Array.from({ length: 45 }, (_, index) => ({
    id: `fixture-${prefix}-${index + 1}`,
    totals: totals({
      requests: 100 - index,
      tokens: {
        input: (index + 1) * 1000,
        output: 0,
        cacheRead: 0,
        cacheWrite: 0,
      },
      cost: String(index + 1),
      firstTokenSumMs: (index + 1) * 250,
      firstTokenSamples: 1,
    }),
  }));
}

let dashboard: Dashboard;
let records: UsageRecord[];
let state: UsageState;
let pricing: PricingView;

beforeEach(() => {
  mock.command.mockReset();
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(true);
  mock.listeners.clear();
  dashboard = {
    totals: totals(),
    trend: [],
    heatmap: [],
    providers: [],
    models: [],
    precision: "millisecond",
    detailSince: 0,
    sources: { proxy: 1, codex: 2 },
  };
  records = [record()];
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
  pricing = {
    config: {
      autoUpdate: true,
      selected: null,
      excluded: [],
      fixed: {},
      aliases: {},
      providerMappings: [],
    },
    models: {
      "fixture-priced": {
        input_cost_per_token: "0.000001",
        output_cost_per_token: "0.000002",
      },
    },
    version: "fixture-price-version",
    source: "Fixture prices",
    checkedAt: null,
    updatedAt: null,
    error: null,
    syncing: false,
    revision: "fixture-pricing-1",
  };
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_gateway":
          return {
            clientId: args.clientId,
            providers: args.clientId === "codex" ? fixtureProviders : [],
          };
        case "get_usage_dashboard":
          return structuredClone(dashboard);
        case "get_usage_heatmap":
          return structuredClone(dashboard.heatmap);
        case "get_usage_logs": {
          const page = (args.filter as UsageFilter).page ?? 1;
          return {
            rows: structuredClone(records.slice((page - 1) * 20, page * 20)),
            total: records.length,
            page,
            detailSince: 0,
          };
        }
        case "get_usage_detail":
          return structuredClone(records.find((row) => row.id === args.id));
        case "get_usage_state":
        case "sync_usage":
          return structuredClone(state);
        case "set_usage_settings":
          state = {
            ...state,
            settings: structuredClone(args.settings as UsageSettings),
          };
          return structuredClone(state);
        case "get_pricing":
          return structuredClone(pricing);
        case "configure_pricing": {
          if (args.expectedRevision !== pricing.revision)
            throw new Error("Fixture pricing revision conflict");
          const config = structuredClone(args.config as PricingConfig);
          pricing = {
            ...pricing,
            config,
            models: { ...pricing.models, ...config.fixed },
            revision: "fixture-pricing-2",
          };
          return structuredClone(pricing);
        }
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
});

function calls(name: string) {
  return mock.command.mock.calls.filter(([command]) => command === name);
}

function metric(label: string) {
  return screen
    .getByText(label, { selector: ".usage-metrics .usage-metric > span" })
    .closest<HTMLElement>(".usage-metric")!;
}

function bodyRows() {
  return within(screen.getByRole("table")).getAllByRole("row").slice(1);
}

function rowNames() {
  return bodyRows().map(
    (row) => within(row).getAllByRole("cell")[0].textContent,
  );
}

function cancelDialog(dialog: HTMLElement) {
  fireEvent(dialog, new Event("cancel", { bubbles: true, cancelable: true }));
}

async function renderUsage() {
  const view = render(<Usage />);
  await waitFor(() => {
    expect(bodyRows()).toHaveLength(Math.min(20, records.length));
    expect(screen.getByRole("button", { name: "刷新用量" })).toBeEnabled();
  });
  return view;
}

async function chooseRange(value: string) {
  await userEvent.click(screen.getByRole("button", { name: "用量时间范围" }));
  const dialog = screen.getByRole("dialog", { name: "选择时间范围" });
  await userEvent.selectOptions(
    within(dialog).getByRole("combobox", { name: "用量时间范围" }),
    value,
  );
  await userEvent.click(within(dialog).getByRole("button", { name: "确定" }));
}

async function openBilling() {
  await userEvent.click(screen.getByRole("tab", { name: "定价" }));
  await screen.findByRole("button", { name: "fixture-priced" });
  const button = screen.getByRole("button", { name: "计费设置" });
  await userEvent.click(button);
  const dialog = screen.getByRole("dialog", { name: "计费设置" });
  await within(dialog).findByRole("spinbutton", { name: "成本倍率" });
  return { button, dialog };
}

describe("v0.18 usage overview", () => {
  it("shows four primary metrics and computes first-token time from measured samples", async () => {
    dashboard.providers = [
      {
        id: "fixture-provider-1",
        totals: totals({ firstTokenSumMs: 0, firstTokenSamples: 1 }),
      },
      {
        id: "fixture-provider-2",
        totals: totals({ firstTokenSumMs: 8000, firstTokenSamples: 4 }),
      },
    ];
    await renderUsage();

    expect(
      [...document.querySelectorAll(".usage-metrics .usage-metric > span")].map(
        (node) => node.textContent,
      ),
    ).toEqual(["估算费用", "请求", "实际 Token", "平均首字"]);
    expect(
      within(metric("估算费用")).getByText("$12.3457"),
    ).toBeInTheDocument();
    expect(within(metric("请求")).getByText("12")).toBeInTheDocument();
    expect(within(metric("实际 Token")).getByText("1.55K")).toBeInTheDocument();
    const average = within(metric("平均首字")).getByText("1.60s");
    expect(average).toHaveAttribute("title", "1.600s · 5 个有效样本");
    act(() => average.focus());
    expect(within(metric("平均首字")).getByRole("tooltip")).toHaveTextContent(
      "1.600s · 5 个有效样本",
    );
  });

  it.each([
    {
      label: "measured zero",
      sum: 0,
      samples: 3,
      expected: "0.00s",
      title: "0.000s · 3 个有效样本",
    },
    {
      label: "no samples",
      sum: 9000,
      samples: 0,
      expected: "—",
      title: "无首字样本",
    },
    {
      label: "legacy totals without measurements",
      sum: undefined,
      samples: undefined,
      expected: "—",
      title: "无首字样本",
    },
  ])(
    "preserves $label without inventing first-token measurements",
    async ({ sum, samples, expected, title }) => {
      dashboard.totals = totals({
        firstTokenSumMs: sum,
        firstTokenSamples: samples,
      });
      await renderUsage();
      expect(within(metric("平均首字")).getByText(expected)).toHaveAttribute(
        "title",
        title,
      );
    },
  );

  it("opens cache and success statistics from a separate more-metrics button", async () => {
    const user = userEvent.setup();
    await renderUsage();
    const more = screen.getByRole("button", { name: "更多指标" });
    expect(more).toHaveAttribute("aria-expanded", "false");
    expect(screen.queryByText("缓存命中率")).not.toBeInTheDocument();
    await user.click(more);
    expect(more).toHaveAttribute("aria-expanded", "true");
    expect(screen.getByText("缓存命中率")).toHaveTextContent("缓存命中率22.2%");
    expect(screen.getByText("HTTP 成功率")).toHaveTextContent(
      "HTTP 成功率90.0%",
    );
    expect(screen.getByText("缓存读取")).toBeInTheDocument();
    expect(screen.getByText("缓存写入")).toBeInTheDocument();
    await user.click(more);
    expect(screen.queryByText("缓存命中率")).not.toBeInTheDocument();
    expect(within(metric("平均首字")).getByText("1.60s")).toBeInTheDocument();
  });

  it("keeps review and historical-source information in sources and request details", async () => {
    const user = userEvent.setup();
    dashboard.reviewCount = 3;
    dashboard.sourceHistoryIncomplete = true;
    state.reports = {
      codex: {
        files: 1,
        imported: 2,
        merged: 1,
        pending: 3,
        skipped: 0,
        errors: 0,
        completedAt: timestamp,
      },
    };
    records = [record({ deduplication: "ambiguous" })];
    await renderUsage();
    expect(screen.queryByText(/待核对/)).not.toBeInTheDocument();
    expect(
      screen.queryByText("部分历史汇总无法按来源拆分"),
    ).not.toBeInTheDocument();

    const sourcesButton = screen.getByRole("button", { name: "数据来源" });
    await user.click(sourcesButton);
    const sources = screen.getByRole("dialog", { name: "数据来源" });
    expect(within(sources).getByText(/待核对 3/)).toBeInTheDocument();
    await user.click(
      within(sources).getByText("历史数据", { selector: "summary" }),
    );
    expect(
      within(sources).getByText("部分旧日汇总仅支持全部来源查询。"),
    ).toBeVisible();
    cancelDialog(sources);
    await waitFor(() => expect(sourcesButton).toHaveFocus());

    const requestRow = bodyRows()[0];
    await user.click(requestRow);
    const detail = await screen.findByRole("dialog", { name: "请求详情" });
    expect(
      within(detail).getByText("关联未确认，未计入合计"),
    ).toBeInTheDocument();
    cancelDialog(detail);
    await waitFor(() => expect(requestRow).toHaveFocus());
  });
});

describe("v0.18 time ranges", () => {
  it("keeps all seven range choices in the date dialog and applies custom values only on confirmation", async () => {
    const user = userEvent.setup();
    records = Array.from({ length: 21 }, (_, index) =>
      record({ id: `fixture-request-${index}` }),
    );
    render(<Usage />);
    await waitFor(() => expect(bodyRows()).toHaveLength(20));
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => expect(bodyRows()).toHaveLength(1));
    expect(
      screen.queryByRole("combobox", { name: "用量时间范围" }),
    ).not.toBeInTheDocument();
    const dateButton = screen.getByRole("button", { name: "用量时间范围" });
    expect(dateButton).toHaveTextContent("今日");
    await user.click(dateButton);
    const dialog = screen.getByRole("dialog", { name: "选择时间范围" });
    const selector = within(dialog).getByRole("combobox", {
      name: "用量时间范围",
    });
    expect(
      within(selector)
        .getAllByRole("option")
        .map((option) => option.textContent),
    ).toEqual([
      "今日",
      "最近 24 小时",
      "7 天",
      "14 天",
      "30 天",
      "全部",
      "自定义",
    ]);
    const overviewCalls = calls("get_usage_dashboard").length;
    const logCalls = calls("get_usage_logs").length;
    await user.selectOptions(selector, "custom");
    expect(within(dialog).getByLabelText("结束时间")).toBeDisabled();
    await user.click(
      within(dialog).getByRole("checkbox", { name: "跟随当前" }),
    );
    fireEvent.change(within(dialog).getByLabelText("开始时间"), {
      target: { value: "2026-10-01T00:00" },
    });
    fireEvent.change(within(dialog).getByLabelText("结束时间"), {
      target: { value: "2026-10-02T00:00" },
    });
    expect(calls("get_usage_dashboard")).toHaveLength(overviewCalls);
    expect(calls("get_usage_logs")).toHaveLength(logCalls);
    expect(dateButton).toHaveTextContent("今日");

    await user.click(within(dialog).getByRole("button", { name: "确定" }));
    const filter = {
      start: new Date("2026-10-01T00:00").getTime(),
      end: new Date("2026-10-02T00:00").getTime(),
    };
    await waitFor(() =>
      expect(calls("get_usage_dashboard").at(-1)![1].filter).toMatchObject(
        filter,
      ),
    );
    expect(calls("get_usage_logs").at(-1)![1].filter).toMatchObject({
      ...filter,
      page: 1,
    });
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(dateButton).toHaveTextContent("自定义");
    expect(dateButton).toHaveFocus();
  });

  it("discards a date draft on cancellation and restores focus to the date button", async () => {
    const user = userEvent.setup();
    await renderUsage();
    const dateButton = screen.getByRole("button", { name: "用量时间范围" });
    const overviewCalls = calls("get_usage_dashboard").length;
    await user.click(dateButton);
    const dialog = screen.getByRole("dialog", { name: "选择时间范围" });
    await user.selectOptions(
      within(dialog).getByRole("combobox", { name: "用量时间范围" }),
      "all",
    );
    cancelDialog(dialog);
    expect(dateButton).toHaveFocus();
    expect(dateButton).toHaveTextContent("今日");
    expect(calls("get_usage_dashboard")).toHaveLength(overviewCalls);
    expect(calls("get_usage_heatmap")).toHaveLength(0);
  });

  it("uses a yearly heatmap for all time and bounds year navigation at the current year", async () => {
    const user = userEvent.setup();
    const year = new Date().getFullYear();
    dashboard.trend = [
      { time: new Date(year - 2, 0, 1).getTime(), totals: totals() },
    ];
    dashboard.heatmap = [
      { time: new Date(year, 0, 2).getTime(), totals: totals() },
    ];
    await renderUsage();
    expect(
      screen.getByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).toBeInTheDocument();
    expect(calls("get_usage_heatmap")).toHaveLength(0);
    await chooseRange("all");
    await waitFor(() => expect(calls("get_usage_heatmap")).toHaveLength(1));
    expect(
      screen.queryByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).not.toBeInTheDocument();
    const heatmap = screen.getByRole("img", { name: `${year} 年度用量热力图` });
    expect(heatmap.querySelectorAll("span")).toHaveLength(
      year % 4 === 0 && (year % 100 !== 0 || year % 400 === 0) ? 366 : 365,
    );
    expect(calls("get_usage_heatmap").at(-1)![1].filter).toMatchObject({
      start: new Date(year, 0, 1).getTime(),
    });
    expect(screen.getByRole("button", { name: "下一年" })).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "上一年" }));
    await waitFor(() =>
      expect(calls("get_usage_heatmap").at(-1)![1].filter).toMatchObject({
        start: new Date(year - 1, 0, 1).getTime(),
        end: new Date(year, 0, 1).getTime() - 1,
      }),
    );
    expect(
      screen.getByRole("img", { name: `${year - 1} 年度用量热力图` }),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "下一年" }));
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "下一年" })).toBeDisabled(),
    );
    await chooseRange("7");
    expect(
      screen.getByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("img", { name: /年度用量热力图/ }),
    ).not.toBeInTheDocument();
  });
});

describe("v0.18 rankings", () => {
  it("keeps request columns and separate twenty-row ranking pages when switching tabs", async () => {
    const user = userEvent.setup();
    dashboard.providers = groups("provider");
    dashboard.models = groups("model");
    await renderUsage();
    expect(
      screen.getAllByRole("columnheader").map((header) => header.textContent),
    ).toEqual([
      "时间",
      "客户端",
      "供应商",
      "模型",
      "输入",
      "输出",
      "缓存",
      "费用",
    ]);
    await user.click(screen.getByRole("tab", { name: "供应商" }));
    expect(
      screen.getAllByRole("columnheader").map((header) => header.textContent),
    ).toEqual(["供应商", "请求", "Token", "费用", "成功率", "平均首字"]);
    expect(bodyRows()).toHaveLength(20);
    await user.click(screen.getByRole("button", { name: "下一页" }));
    expect(rowNames()[0]).toBe("Fixture Provider 21");
    await user.click(screen.getByRole("tab", { name: "模型" }));
    expect(bodyRows()).toHaveLength(20);
    expect(rowNames()[0]).toBe("fixture-model-1");
    await user.click(screen.getByRole("button", { name: "3" }));
    expect(bodyRows()).toHaveLength(5);
    expect(rowNames()[0]).toBe("fixture-model-41");

    await user.click(screen.getByRole("tab", { name: "供应商" }));
    expect(screen.getByRole("button", { name: "2" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(rowNames()[0]).toBe("Fixture Provider 21");
    await user.click(screen.getByRole("tab", { name: "请求日志" }));
    await user.click(screen.getByRole("tab", { name: "模型" }));
    expect(screen.getByRole("button", { name: "3" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(bodyRows()).toHaveLength(5);
  });

  it("sorts the full ranking before paging and keeps its sort when returning from another tab", async () => {
    const user = userEvent.setup();
    dashboard.providers = groups("provider");
    await renderUsage();
    await user.click(screen.getByRole("tab", { name: "供应商" }));
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await user.click(
      within(screen.getByRole("table")).getByRole("button", { name: "费用" }),
    );
    expect(screen.getByRole("button", { name: "1" })).toHaveAttribute(
      "aria-current",
      "page",
    );
    expect(rowNames()[0]).toBe("Fixture Provider 45");
    await user.click(screen.getByRole("button", { name: "下一页" }));
    expect(rowNames()[0]).toBe("Fixture Provider 25");
    await user.click(screen.getByRole("tab", { name: "模型" }));
    await user.click(screen.getByRole("tab", { name: "供应商" }));
    expect(rowNames()[0]).toBe("Fixture Provider 25");
    await user.click(
      within(screen.getByRole("table")).getByRole("button", { name: "费用" }),
    );
    expect(rowNames()[0]).toBe("Fixture Provider 1");
  });

  it("orders measured first-token averages numerically and keeps missing samples last in both directions", async () => {
    const user = userEvent.setup();
    dashboard.models = [
      {
        id: "fixture-weighted",
        totals: totals({ firstTokenSumMs: 8000, firstTokenSamples: 5 }),
      },
      {
        id: "fixture-no-samples",
        totals: totals({ firstTokenSumMs: 0, firstTokenSamples: 0 }),
      },
      {
        id: "fixture-zero",
        totals: totals({ firstTokenSumMs: 0, firstTokenSamples: 2 }),
      },
      {
        id: "fixture-slow",
        totals: totals({ firstTokenSumMs: 6000, firstTokenSamples: 2 }),
      },
    ];
    await renderUsage();
    await user.click(screen.getByRole("tab", { name: "模型" }));
    const sort = within(screen.getByRole("table")).getByRole("button", {
      name: "平均首字",
    });
    await user.click(sort);
    expect(rowNames()).toEqual([
      "fixture-zero",
      "fixture-weighted",
      "fixture-slow",
      "fixture-no-samples",
    ]);
    const cells = bodyRows().map((row) => within(row).getAllByRole("cell")[5]);
    expect(cells.map((cell) => cell.textContent)).toEqual([
      "0.00s",
      "1.60s",
      "3.00s",
      "—",
    ]);
    expect(within(cells[1]).getByText("1.60s")).toHaveAttribute(
      "title",
      "1.600s · 5 个有效样本",
    );
    await user.click(sort);
    expect(rowNames()).toEqual([
      "fixture-slow",
      "fixture-weighted",
      "fixture-zero",
      "fixture-no-samples",
    ]);
  });
});

describe("v0.18 request details and sources", () => {
  it.each([
    {
      label: "measured streaming zero",
      source: "proxy",
      stream: true,
      operation: "model" as const,
      firstTokenMs: 0,
      expected: "0.000s",
    },
    {
      label: "measured streaming latency",
      source: "proxy",
      stream: true,
      operation: "model" as const,
      firstTokenMs: 250,
      expected: "0.250s",
    },
    {
      label: "missing streaming measurement",
      source: "proxy",
      stream: true,
      operation: "model" as const,
      firstTokenMs: null,
      expected: "未提供",
    },
    {
      label: "session duration",
      source: "codex",
      stream: true,
      operation: "model" as const,
      firstTokenMs: 1250,
      expected: "未提供",
    },
    {
      label: "non-streaming duration",
      source: "proxy",
      stream: false,
      operation: "model" as const,
      firstTokenMs: 1250,
      expected: "未提供",
    },
    {
      label: "web search duration",
      source: "proxy",
      stream: true,
      operation: "web_search" as const,
      firstTokenMs: 1250,
      expected: "未提供",
    },
  ])(
    "groups detail fields and treats $label correctly",
    async ({ source, stream, operation, firstTokenMs, expected }) => {
      records = [
        record({
          source,
          estimatedSpeed: source !== "proxy",
          attempts: [attempt({ stream, operation, firstTokenMs })],
        }),
      ];
      await renderUsage();
      await userEvent.click(bodyRows()[0]);
      const detail = await screen.findByRole("dialog", { name: "请求详情" });
      expect(
        [...detail.querySelectorAll(".usage-drawer-body > h3")].map(
          (heading) => heading.textContent,
        ),
      ).toEqual(["基本信息", "Token", "费用", "性能", "尝试记录"]);
      const firstToken = within(detail).getAllByText("首字", {
        selector: "dt",
      })[0];
      expect(firstToken.nextElementSibling).toHaveTextContent(expected);
      expect(
        within(detail).queryByText(/Token\/s|估算首字/),
      ).not.toBeInTheDocument();
      expect(
        within(detail).getByText("来源", { selector: "dt" }).nextElementSibling,
      ).toHaveTextContent(source === "proxy" ? "网关" : "Codex 会话");
    },
  );

  it("keeps recording, session synchronization and rebuild actions in data sources", async () => {
    const user = userEvent.setup();
    await renderUsage();
    await user.click(screen.getByRole("button", { name: "数据来源" }));
    const sources = screen.getByRole("dialog", { name: "数据来源" });
    expect(within(sources).getAllByRole("checkbox")).toHaveLength(2);
    expect(
      within(sources).getByRole("checkbox", { name: "记录网关用量" }),
    ).toBeChecked();
    expect(
      within(sources).getByRole("checkbox", { name: "自动同步会话" }),
    ).toBeChecked();
    expect(
      within(sources).queryByRole("spinbutton", { name: "成本倍率" }),
    ).not.toBeInTheDocument();
    expect(
      within(sources).queryByRole("combobox", { name: "计价模型" }),
    ).not.toBeInTheDocument();
    expect(
      within(sources).queryByRole("button", { name: "供应商映射" }),
    ).not.toBeInTheDocument();

    await user.click(within(sources).getByRole("button", { name: "同步会话" }));
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("sync_usage", {}),
    );
    await user.click(
      within(sources).getByRole("button", { name: "重建 Codex 用量" }),
    );
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("sync_usage", {
        rebuild: "codex",
      }),
    );
    await user.click(
      within(sources).getByRole("button", { name: "重建 Claude 用量" }),
    );
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("sync_usage", {
        rebuild: "claude",
      }),
    );
    expect(calls("set_usage_settings")).toHaveLength(0);
    expect(calls("configure_pricing")).toHaveLength(0);
  });
});

describe("v0.18 billing settings", () => {
  it("keeps billing drafts while opening provider mappings and returns focus through both drawers", async () => {
    const user = userEvent.setup();
    await renderUsage();
    const { button, dialog } = await openBilling();
    const multiplier = within(dialog).getByRole("spinbutton", {
      name: "成本倍率",
    });
    const model = within(dialog).getByRole("combobox", { name: "计价模型" });
    fireEvent.change(multiplier, { target: { value: "2.5" } });
    await user.selectOptions(model, "request");
    const mappingButton = within(dialog).getByRole("button", {
      name: "供应商映射",
    });
    await user.click(mappingButton);
    const mappings = screen.getByRole("dialog", { name: "供应商计价映射" });
    expect(
      within(mappings).getByRole("button", { name: "添加映射" }),
    ).toBeInTheDocument();
    cancelDialog(mappings);
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "供应商计价映射" }),
      ).not.toBeInTheDocument(),
    );
    expect(screen.getByRole("dialog", { name: "计费设置" })).toBe(dialog);
    expect(multiplier).toHaveValue(2.5);
    expect(model).toHaveValue("request");
    await waitFor(() => expect(mappingButton).toHaveFocus());
    cancelDialog(dialog);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(mock.confirm).toHaveBeenCalledWith("放弃未保存的计费设置？");
    await waitFor(() => expect(button).toHaveFocus());
    expect(
      screen.queryByRole("button", { name: "供应商映射" }),
    ).not.toBeInTheDocument();
    expect(calls("set_usage_settings")).toHaveLength(0);
    expect(calls("configure_pricing")).toHaveLength(0);
  });

  it("saves only billing fields against the latest recording, sync and refresh settings", async () => {
    const user = userEvent.setup();
    await renderUsage();
    const { button, dialog } = await openBilling();
    const multiplier = within(dialog).getByRole("spinbutton", {
      name: "成本倍率",
    });
    const model = within(dialog).getByRole("combobox", { name: "计价模型" });
    fireEvent.change(multiplier, { target: { value: "0" } });
    await user.selectOptions(model, "request");
    state.settings = {
      ...state.settings,
      recording: false,
      autoSync: false,
      refreshSeconds: 60,
      multiplier: "9",
    };
    await act(async () => mock.listeners.get("usage-state")?.(state));
    expect(multiplier).toHaveValue(0);
    expect(model).toHaveValue("request");
    expect(calls("set_usage_settings")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(state.settings).toEqual({
      recording: false,
      autoSync: false,
      refreshSeconds: 60,
      multiplier: "0",
      pricingModel: "request",
    });
    expect(calls("set_usage_settings")).toHaveLength(1);
    expect(calls("configure_pricing")).toHaveLength(0);
    expect(calls("sync_usage")).toHaveLength(0);
    await waitFor(() => expect(button).toHaveFocus());
  });

  it("retains unsaved billing settings when cancellation is declined", async () => {
    const user = userEvent.setup();
    await renderUsage();
    const { button, dialog } = await openBilling();
    const multiplier = within(dialog).getByRole("spinbutton", {
      name: "成本倍率",
    });
    fireEvent.change(multiplier, { target: { value: "2.5" } });
    mock.confirm.mockResolvedValueOnce(false);
    await user.click(within(dialog).getByRole("button", { name: "取消" }));
    expect(mock.confirm).toHaveBeenCalledWith("放弃未保存的计费设置？");
    expect(dialog).toBeInTheDocument();
    expect(multiplier).toHaveValue(2.5);
    await user.click(within(dialog).getByRole("button", { name: "取消" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    await waitFor(() => expect(button).toHaveFocus());
    expect(calls("set_usage_settings")).toHaveLength(0);
  });

  it("preserves actual price editing alongside the billing settings entry", async () => {
    const user = userEvent.setup();
    await renderUsage();
    await user.click(screen.getByRole("tab", { name: "定价" }));
    const priceButton = await screen.findByRole("button", {
      name: "fixture-priced",
    });
    expect(
      screen.getByRole("button", { name: "计费设置" }),
    ).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "供应商映射" }),
    ).not.toBeInTheDocument();
    await user.click(priceButton);
    const editor = screen.getByRole("dialog", { name: "编辑价格" });
    fireEvent.change(within(editor).getByLabelText("输入 · USD / 百万 Token"), {
      target: { value: "4.25" },
    });
    fireEvent.change(within(editor).getByLabelText("输出 · USD / 百万 Token"), {
      target: { value: "0" },
    });
    await user.click(within(editor).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(calls("configure_pricing")[0][1]).toMatchObject({
      expectedRevision: "fixture-pricing-1",
      config: {
        fixed: {
          "fixture-priced": {
            input_cost_per_token: "0.00000425",
            output_cost_per_token: "0.000000",
          },
        },
      },
    });
    const priceRow = screen
      .getByRole("button", { name: "fixture-priced" })
      .closest("tr")!;
    expect(within(priceRow).getAllByRole("cell")[1]).toHaveTextContent(
      /^4\.25$/,
    );
    expect(within(priceRow).getAllByRole("cell")[2]).toHaveTextContent(/^0$/);
    expect(within(priceRow).getByText("固定")).toBeInTheDocument();
    expect(calls("set_usage_settings")).toHaveLength(0);
  });
});
