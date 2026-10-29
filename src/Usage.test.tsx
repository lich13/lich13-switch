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

import Usage, { syncStatus } from "./Usage";
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
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_state":
          return {
            accounts: [],
            authRevision: "fixture-auth-revision",
            configRevision: "fixture-config-revision",
            currentState: "missing",
            preferences: {
              codexHome: "/fixture/codex",
              cliPath: "",
              theme: "dark",
            },
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
          return {
            phase: "idle",
            mode: "",
            url: null,
            code: null,
            message: "",
            callbackReady: false,
          };
        case "get_provider_imports":
          return [];
        case "frontend_ready":
          return undefined;
        case "get_app_events":
          return { items: [], total: 0, page: 1, error: null };
        case "get_gateway":
          return {
            clientId: args.clientId,
            providers:
              args.clientId === "codex"
                ? [
                    { id: "provider-first", name: "First Provider" },
                    { id: "provider-final", name: "Fixture Provider" },
                  ]
                : [],
          };
        case "get_usage_dashboard":
          return structuredClone(dashboard);
        case "get_usage_interruptions":
          return {
            start: timestamp - 1000,
            end: timestamp + 1000,
            detailSince: 0,
            historyMissing: false,
            requests: 3,
            reported: 1,
            partial: 1,
            unknown: 1,
            groups: [
              {
                client: "claude",
                provider: "provider-final",
                reported: 1,
                partial: 1,
                unknown: 1,
              },
            ],
          };
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          return {
            rows: structuredClone(records),
            total: totalRows,
            page: filter.page ?? 1,
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
        case "search_pricing": {
          const rows = Object.entries(pricing.models)
            .filter(([model]) =>
              model.toLowerCase().includes(String(args.query).toLowerCase()),
            )
            .sort(([a], [b]) => a.localeCompare(b))
            .map(([model, data]) => ({
              model,
              name: null,
              source: model in pricing.config.fixed ? "fixed" : "sub2api",
              matchedModel: model,
              matchedBy: "exact",
              data,
            }));
          return {
            rows: rows.slice(
              Number(args.offset),
              Number(args.offset) + Number(args.limit),
            ),
            total: rows.length,
          };
        }
        case "get_pricing":
          return structuredClone(pricing);
        case "configure_pricing": {
          if (args.expectedRevision !== pricing.revision) {
            throw {
              code: "CONFLICT",
              message: "价格配置已被外部修改，请重新保存",
            };
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
    },
  );
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

function reportedTokenMetric() {
  return screen.getByText("已报告 Token", {
    selector: ".usage-metrics .usage-metric span",
  }).parentElement!;
}

async function renderUsage() {
  const result = render(<Usage />);
  await waitFor(() => {
    expect(within(screen.getByRole("table")).getAllByRole("row")).toHaveLength(
      records.length + 1,
    );
    expect(screen.getByRole("button", { name: "刷新用量" })).toBeEnabled();
  });
  return result;
}

function cancelDialog(dialog: HTMLElement) {
  fireEvent(dialog, new Event("cancel", { bubbles: true, cancelable: true }));
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

describe("usage records", () => {
  it("groups request status, column settings and number format in order only on the request tab", async () => {
    await renderUsage();
    const toolbar = screen.getByRole("group", { name: "请求日志工具" });
    const status = within(toolbar).getByRole("combobox", { name: "请求状态" });
    const columns = within(toolbar).getByText("列设置");
    const format = within(toolbar).getByRole("combobox", {
      name: "日志数字格式",
    });
    expect(status).toHaveDisplayValue("全部状态");
    expect(format).toHaveDisplayValue("紧凑数字");
    expect(Array.from(toolbar.querySelectorAll("select, summary"))).toEqual([
      status,
      columns,
      format,
    ]);
    expect(screen.getByRole("tablist", { name: "用量明细" })).toContainElement(
      toolbar,
    );
    for (const tab of ["供应商", "模型", "定价"]) {
      await userEvent.click(screen.getByRole("tab", { name: tab }));
      expect(
        screen.queryByRole("group", { name: "请求日志工具" }),
      ).not.toBeInTheDocument();
      expect(
        screen.queryByRole("combobox", { name: "日志数字格式" }),
      ).not.toBeInTheDocument();
    }
    await userEvent.click(screen.getByRole("tab", { name: "请求日志" }));
    expect(
      screen.getByRole("group", { name: "请求日志工具" }),
    ).toBeInTheDocument();
    expect(calls("set_usage_settings")).toHaveLength(0);
  });

  it("saves column choices, restores defaults and switches compact and exact numbers from the toolbar", async () => {
    records[0].attempts[0].tokens.input = 12345;
    await renderUsage();
    const toolbar = screen.getByRole("group", { name: "请求日志工具" });
    const initialSettings = structuredClone(state.settings);
    await userEvent.click(within(toolbar).getByText("列设置"));
    const output = within(toolbar).getByRole("checkbox", { name: "输出" });
    expect(output).toBeChecked();
    await userEvent.click(output);
    await waitFor(() => expect(output).not.toBeChecked());
    expect(calls("set_usage_settings").at(-1)![1]).toEqual({
      settings: { ...initialSettings, logColumns: { output: false } },
    });
    expect(
      within(screen.getByRole("table")).queryByLabelText("输出"),
    ).not.toBeInTheDocument();

    await userEvent.click(
      within(toolbar).getByRole("button", { name: "恢复默认列" }),
    );
    await waitFor(() => expect(output).toBeChecked());
    const restoredSettings = { ...initialSettings, logColumns: {} };
    expect(calls("set_usage_settings").at(-1)![1]).toEqual({
      settings: restoredSettings,
    });
    expect(
      within(screen.getByRole("table")).getByLabelText("输出"),
    ).toHaveTextContent("400");

    const format = within(toolbar).getByRole("combobox", {
      name: "日志数字格式",
    });
    const inputMetric = () =>
      within(screen.getByRole("table")).getByLabelText("输入");
    expect(inputMetric()).toHaveTextContent("12.3K");
    await userEvent.selectOptions(format, "exact");
    await waitFor(() => expect(inputMetric()).toHaveTextContent("12,345"));
    expect(calls("set_usage_settings").at(-1)![1]).toEqual({
      settings: { ...restoredSettings, logNumberMode: "exact" },
    });
    await userEvent.selectOptions(format, "compact");
    await waitFor(() => expect(inputMetric()).toHaveTextContent("12.3K"));
    expect(calls("set_usage_settings").at(-1)![1]).toEqual({
      settings: { ...restoredSettings, logNumberMode: "compact" },
    });
    expect(calls("set_usage_settings")).toHaveLength(4);
  });

  it("counts interruptions only on request using the current filters", async () => {
    await renderUsage();
    await userEvent.selectOptions(
      screen.getByRole("combobox", { name: "用量客户端" }),
      "claude",
    );
    await chooseRange("all");
    expect(calls("get_usage_interruptions")).toHaveLength(0);
    await userEvent.click(screen.getByRole("button", { name: "统计中断" }));
    await screen.findByText("中断统计 · 本网关");
    expect(calls("get_usage_interruptions")).toHaveLength(1);
    expect(calls("get_usage_interruptions")[0][1].filter).toMatchObject({
      client: "claude",
    });
    expect(screen.getByText(/读取 3 个逻辑请求/)).toHaveTextContent(
      "证据不足 1",
    );
    await userEvent.click(screen.getByRole("button", { name: "刷新用量" }));
    expect(calls("get_usage_interruptions")).toHaveLength(1);
  });

  it("loads the overview while the request log is still pending", async () => {
    let resolveLogs!: (value: {
      rows: UsageRecord[];
      total: number;
      page: number;
      detailSince: number;
    }) => void;
    const logsResponse = new Promise<{
      rows: UsageRecord[];
      total: number;
      page: number;
      detailSince: number;
    }>((resolve) => {
      resolveLogs = resolve;
    });
    mock.command
      .mockImplementationOnce(async () => ({
        ...structuredClone(dashboard),
        totals: { ...dashboard.totals, requests: 7 },
      }))
      .mockImplementationOnce(() => logsResponse);

    render(<Usage />);

    await waitFor(() => expect(reportedTokenMetric()).toHaveTextContent("420"));
    expect(calls("get_usage_dashboard")).toHaveLength(1);
    expect(calls("get_usage_logs")).toHaveLength(1);
    expect(
      screen.getByRole("table").querySelector("tbody"),
    ).toBeEmptyDOMElement();

    await act(async () =>
      resolveLogs({
        rows: structuredClone(records),
        total: 1,
        page: 1,
        detailSince: 0,
      }),
    );
    expect(
      await within(screen.getByRole("table")).findByTitle(
        "Fixture Provider · Codex",
      ),
    ).toBeInTheDocument();
  });

  it("does not render a trend or heatmap and never queries historical data", async () => {
    await renderUsage();
    expect(
      screen.queryByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("img", { name: /年度用量热力图/ }),
    ).not.toBeInTheDocument();
    expect(calls("get_usage_heatmap")).toHaveLength(0);
    await chooseRange("all");
    expect(
      screen.queryByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("img", { name: /年度用量热力图/ }),
    ).not.toBeInTheDocument();
    expect(calls("get_usage_heatmap")).toHaveLength(0);
  });

  it("keeps the previous overview and rows visible until a refresh resolves", async () => {
    await renderUsage();
    let resolveDashboard!: (value: Dashboard) => void;
    const dashboardResponse = new Promise<Dashboard>((resolve) => {
      resolveDashboard = resolve;
    });
    mock.command.mockImplementationOnce(() => dashboardResponse);

    fireEvent.click(screen.getByRole("button", { name: "刷新用量" }));

    expect(reportedTokenMetric()).toHaveTextContent("420");
    expect(
      within(screen.getByRole("table")).getByTitle("Fixture Provider · Codex"),
    ).toBeInTheDocument();
    await act(async () =>
      resolveDashboard({
        ...structuredClone(dashboard),
        totals: {
          ...dashboard.totals,
          tokens: { ...dashboard.totals.tokens, output: 900 },
        },
      }),
    );
    await waitFor(() => expect(reportedTokenMetric()).toHaveTextContent("920"));
    expect(
      within(screen.getByRole("table")).getByTitle("Fixture Provider · Codex"),
    ).toBeInTheDocument();
  });

  it("discards an older overview response after the selected range changes", async () => {
    let resolveStale!: (value: Dashboard) => void;
    const staleResponse = new Promise<Dashboard>((resolve) => {
      resolveStale = resolve;
    });
    mock.command.mockImplementationOnce(() => staleResponse);
    render(<Usage />);

    await waitFor(() => expect(screen.getByRole("table")).toBeInTheDocument());
    dashboard = {
      ...dashboard,
      totals: {
        ...dashboard.totals,
        tokens: { ...dashboard.totals.tokens, output: 7000 },
      },
    };
    await chooseRange("7");
    await waitFor(() =>
      expect(reportedTokenMetric()).toHaveTextContent("7.02K"),
    );

    await act(async () =>
      resolveStale({
        ...structuredClone(dashboard),
        totals: {
          ...dashboard.totals,
          tokens: { ...dashboard.totals.tokens, output: 9900 },
        },
      }),
    );
    expect(reportedTokenMetric()).toHaveTextContent("7.02K");
  });

  it("keeps unavailable counts and prices distinct from measured zero", async () => {
    records = [
      record({
        id: "fixture-missing",
        attempts: [attempt({ tokens: absentTokens, price: null })],
      }),
      record({
        id: "fixture-zero",
        attempts: [
          attempt({
            tokens: zeroTokens,
            price: { ...attempt().price!, cost: "0", multiplier: "1" },
          }),
        ],
      }),
    ];
    totalRows = 2;
    dashboard.totals.tokens = absentTokens;
    await renderUsage();
    const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
    const unavailable = within(rows[0]).getAllByRole("cell");
    const zero = within(rows[1]).getAllByRole("cell");
    for (const label of ["输入", "输出", "缓存读", "缓存写"]) {
      expect(
        within(unavailable[1]).getByTitle(label).querySelector(".usage-number"),
      ).toHaveTextContent(/^—$/);
      expect(
        within(zero[1]).getByTitle(label).querySelector(".usage-number"),
      ).toHaveTextContent(/^0$/);
    }
    expect(unavailable[2]).toHaveTextContent(/^未定价$/);
    expect(zero[2]).toHaveTextContent(/^\$0$/);
    const tokenMetric = screen.getByText("已报告 Token").parentElement!;
    expect(within(tokenMetric).getByText("—")).toBeInTheDocument();

    dashboard.totals.tokens = zeroTokens;
    fireEvent.click(screen.getByRole("button", { name: "刷新用量" }));
    await waitFor(() =>
      expect(within(tokenMetric).getByText("0")).toBeInTheDocument(),
    );
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
    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((cell) => cell.textContent),
    ).toEqual(["时间 / 供应商 / 模型", "Token", "费用", "性能"]);
    const row = within(table).getAllByRole("row")[1];
    const cells = within(row).getAllByRole("cell");
    expect(cells).toHaveLength(4);
    expect(cells[0]).toHaveTextContent(/Fixture Provider/);
    expect(cells[0]).toHaveTextContent(/Codex/);
    expect(cells[0]).toHaveTextContent(/fixture-response-model/);
    expect(within(cells[1]).getByTitle("输入")).toHaveTextContent("0");
    expect(within(cells[1]).getByTitle("输出")).toHaveTextContent("400");
    expect(within(cells[1]).getByTitle("缓存读")).toHaveTextContent("20");
    expect(cells[2]).toHaveTextContent("$0.004568×1.5");
    expect(cells[2]).toHaveAttribute("title", "0.0045678");
    expect(within(row).queryByText("First Provider")).not.toBeInTheDocument();
    expect(within(row).queryByTitle("HTTP 503")).not.toBeInTheDocument();

    fireEvent.keyDown(row, { key: "Enter" });
    const dialog = await screen.findByRole("dialog");
    expect(mock.command).toHaveBeenCalledWith("get_usage_detail", {
      id: "fixture-request",
    });
    expect(within(dialog).getByText("HTTP 200")).toBeInTheDocument();
    expect(within(dialog).getByText("已完成")).toBeInTheDocument();
    expect(
      within(dialog).getByRole("heading", { name: "此前尝试" }),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByText("实际尝试").nextElementSibling,
    ).toHaveTextContent(/^2$/);
    expect(
      within(dialog).getByText("全部尝试费用").nextElementSibling,
    ).toHaveTextContent("$0.0060");
    const firstSummary = within(dialog).getByText("1 · First Provider · 503");
    fireEvent.click(firstSummary);
    expect(
      within(firstSummary.parentElement!).getByText("fixture-first-model"),
    ).toBeVisible();
    expect(
      within(firstSummary.parentElement!).getByText("9,999"),
    ).toBeVisible();
    expect(
      within(dialog).queryByText("2 · Fixture Provider · 200"),
    ).not.toBeInTheDocument();
  });

  it("labels web search operations instead of showing their model", async () => {
    const search = {
      ...attempt({
        requestedModel: "gpt-request-model",
        responseModel: "gpt-response-model",
      }),
      operation: "web_search" as const,
    };
    records = [record({ attempts: [search] })];
    await renderUsage();

    const modelCell = within(screen.getByRole("table"))
      .getAllByRole("row")[1]
      .querySelectorAll("td")[0];
    expect(
      within(modelCell).getByText("网络搜索", { exact: true }),
    ).toBeInTheDocument();
    expect(modelCell).not.toHaveTextContent("gpt-request-model");
    expect(modelCell).not.toHaveTextContent("gpt-response-model");
    expect(within(modelCell).getByText("搜索")).toBeInTheDocument();
  });

  it("marks streaming and compaction requests in the model cell", async () => {
    records = [
      record({
        id: "fixture-compaction",
        attempts: [
          attempt({
            operation: "compaction",
            stream: true,
          }),
        ],
      }),
    ];
    await renderUsage();
    const modelCell = within(screen.getByRole("table"))
      .getAllByRole("row")[1]
      .querySelectorAll("td")[0];
    expect(within(modelCell).getByText("流式")).toBeInTheDocument();
    expect(within(modelCell).getByText("压缩")).toBeInTheDocument();
  });

  it("returns focus to request and source entry points after dialog cancellation", async () => {
    await renderUsage();
    const requestRow = within(screen.getByRole("table")).getAllByRole("row")[1];
    fireEvent.click(requestRow);
    const detail = await screen.findByRole("dialog");
    cancelDialog(detail);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    await waitFor(() => expect(requestRow).toHaveFocus());

    const sourcesButton = screen.getByRole("button", { name: "数据来源" });
    fireEvent.click(sourcesButton);
    const sources = await screen.findByRole("dialog");
    cancelDialog(sources);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
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
    records = [
      record({
        attempts: [attempt({ price: { ...attempt().price!, rates } })],
      }),
    ];
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
    expect(
      pairs.every((pair) => pair.classList.contains("usage-detail-pair")),
    ).toBe(true);
    expect(pairs.map((pair) => pair.querySelector("dd")?.textContent)).toEqual([
      "$0",
      "$2",
      "$3",
      "$4",
      "$5",
      "$6",
      "$7",
      "$8",
      "$9",
      "$10",
      "$11",
    ]);
  });

  it("shows request speed only when complete and retains first-token timing", async () => {
    records = [
      record({
        id: "fixture-session",
        source: "codex",
        estimatedSpeed: true,
        attempts: [
          attempt({
            provider: null,
            firstTokenMs: null,
            status: null,
            outcome: "completed",
          }),
        ],
      }),
    ];
    totalRows = records.length;
    await renderUsage();
    const table = screen.getByRole("table");
    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((cell) => cell.textContent),
    ).toEqual(["时间 / 供应商 / 模型", "Token", "费用", "性能"]);
    const rows = within(table).getAllByRole("row").slice(1);
    expect(rows).toHaveLength(1);
    expect(screen.queryByText("Token/s")).not.toBeInTheDocument();
    expect(screen.queryByText("估算 Token/s")).not.toBeInTheDocument();

    fireEvent.click(rows[0]);
    const dialog = await screen.findByRole("dialog");
    expect(within(dialog).getByText("会话完成")).toBeInTheDocument();
    expect(
      within(dialog).queryByText("速度", { selector: "dt" }),
    ).not.toBeInTheDocument();
    expect(within(dialog).queryByText(/Token\/s/)).not.toBeInTheDocument();
    const firstTokenLabels = within(dialog).getAllByText("首字", {
      selector: "dt",
    });
    expect(firstTokenLabels).toHaveLength(1);
    for (const label of firstTokenLabels) {
      expect(label.nextElementSibling).toHaveTextContent("—");
    }
  });

  it("jumps only to valid pages and resets the page when filters change", async () => {
    const user = userEvent.setup();
    totalRows = 61;
    await renderUsage();
    const initial = calls("get_usage_logs").length;
    fireEvent.change(screen.getByRole("spinbutton", { name: "跳转页码" }), {
      target: { value: "5" },
    });
    fireEvent.keyDown(screen.getByRole("spinbutton", { name: "跳转页码" }), {
      key: "Enter",
    });
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(initial);
    fireEvent.change(screen.getByRole("spinbutton", { name: "跳转页码" }), {
      target: { value: "4" },
    });
    fireEvent.keyDown(screen.getByRole("spinbutton", { name: "跳转页码" }), {
      key: "Enter",
    });
    await waitFor(() => {
      expect(lastFilter().page).toBe(4);
      expect(screen.getByRole("button", { name: "4" })).toHaveAttribute(
        "aria-current",
        "page",
      );
      expect(screen.getByRole("button", { name: "下一页" })).toBeDisabled();
    });
    await user.click(screen.getByRole("button", { name: "上一页" }));
    await waitFor(() => {
      expect(lastFilter().page).toBe(3);
      expect(screen.getByRole("button", { name: "3" })).toHaveAttribute(
        "aria-current",
        "page",
      );
    });

    await user.selectOptions(
      screen.getByRole("combobox", { name: "用量客户端" }),
      "codex",
    );
    await waitFor(() =>
      expect(lastFilter()).toMatchObject({ page: 1, client: "codex" }),
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "用量供应商" }),
      "provider-final",
    );
    expect(screen.queryByLabelText("计价模型筛选")).not.toBeInTheDocument();
    expect(screen.queryByLabelText("用量来源")).not.toBeInTheDocument();
    expect(lastFilter().model).toBeUndefined();
    expect(lastFilter().source).toBeUndefined();
    await user.selectOptions(
      screen.getByRole("combobox", { name: "请求状态" }),
      "5xx",
    );
    await waitFor(() =>
      expect(lastFilter()).toMatchObject({
        page: 1,
        client: "codex",
        provider: "provider-final",
        status: "5xx",
      }),
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "请求类型" }),
      "compaction",
    );
    await waitFor(() =>
      expect(lastFilter()).toMatchObject({
        page: 1,
        operation: "compaction",
      }),
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "用量客户端" }),
      "claude",
    );
    await waitFor(() => {
      expect(lastFilter().client).toBe("claude");
      expect(lastFilter().provider).toBeUndefined();
    });
  });

  it("does not expose trend dragging or issue chart queries", async () => {
    await renderUsage();
    expect(
      screen.queryByRole("img", { name: "用量趋势，拖动选择时间范围" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("img", { name: /年度用量热力图/ }),
    ).not.toBeInTheDocument();
    expect(calls("get_usage_heatmap")).toHaveLength(0);
  });

  it("pauses periodic reloads for document and native hiding and respects manual refresh", async () => {
    vi.useFakeTimers();
    state.settings.refreshSeconds = 5;
    const view = render(<Usage />);
    await flush();
    const initial = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(5_000));
    expect(calls("get_usage_logs")).toHaveLength(initial + 1);

    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "hidden",
    });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    const documentHidden = calls("get_usage_logs").length;
    await act(async () => vi.advanceTimersByTime(15_000));
    expect(calls("get_usage_logs")).toHaveLength(documentHidden);
    Object.defineProperty(document, "visibilityState", {
      configurable: true,
      value: "visible",
    });
    act(() => document.dispatchEvent(new Event("visibilitychange")));
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(documentHidden + 1);

    expect(mock.listeners.has("app-visibility")).toBe(true);
    act(() => mock.listeners.get("app-visibility")!(false));
    const nativeHidden = calls("get_usage_logs").length;
    const hiddenStateReads = calls("get_usage_state").length;
    await act(async () => mock.listeners.get("usage-state")?.(undefined));
    await act(async () => vi.advanceTimersByTime(15_000));
    expect(calls("get_usage_state")).toHaveLength(hiddenStateReads);
    expect(calls("get_usage_logs")).toHaveLength(nativeHidden);
    act(() => mock.listeners.get("app-visibility")!(true));
    await flush();
    expect(calls("get_usage_logs")).toHaveLength(nativeHidden + 1);

    fireEvent.change(screen.getByRole("combobox", { name: "用量刷新频率" }), {
      target: { value: "0" },
    });
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
    await user.click(screen.getByRole("button", { name: "数据来源" }));
    const dialog = screen.getByRole("dialog");
    const recording = within(dialog).getByRole("checkbox", {
      name: "记录网关用量",
    });
    const autoSync = within(dialog).getByRole("checkbox", {
      name: "自动同步会话",
    });
    expect(recording).toBeChecked();
    expect(autoSync).toBeChecked();
    await user.click(recording);
    expect(autoSync).toBeChecked();
    expect(calls("set_usage_settings")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(state.settings).toMatchObject({
        recording: false,
        autoSync: true,
      }),
    );

    await user.click(autoSync);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(state.settings).toMatchObject({
        recording: false,
        autoSync: false,
      }),
    );
    await user.click(recording);
    expect(autoSync).not.toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(state.settings).toMatchObject({
        recording: true,
        autoSync: false,
      }),
    );
    expect(calls("sync_usage")).toHaveLength(0);
    expect(calls("configure_pricing")).toHaveLength(0);
  });

  it("keeps source settings drafts while a background refresh updates sync status", async () => {
    vi.useFakeTimers();
    state.settings.refreshSeconds = 5;
    const onDirtyChange = vi.fn();
    render(<Usage onDirtyChange={onDirtyChange} />);
    await flush();
    fireEvent.click(screen.getByRole("button", { name: "数据来源" }));
    const dialog = screen.getByRole("dialog");
    const recording = within(dialog).getByRole("checkbox", {
      name: "记录网关用量",
    });
    const autoSync = within(dialog).getByRole("checkbox", {
      name: "自动同步会话",
    });
    expect(
      within(dialog).queryByRole("spinbutton", { name: "成本倍率" }),
    ).not.toBeInTheDocument();
    expect(
      within(dialog).queryByRole("combobox", { name: "计价模型" }),
    ).not.toBeInTheDocument();
    fireEvent.click(recording);
    fireEvent.click(autoSync);
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    state = {
      ...state,
      settings: { ...state.settings, multiplier: "9", pricingModel: "request" },
      reports: {
        codex: {
          files: 4,
          imported: 2,
          skipped: 0,
          errors: 2,
          completedAt: timestamp,
        },
      },
    };
    await act(async () => vi.advanceTimersByTime(5_000));
    expect(within(dialog).getByText("2 项未同步")).toBeInTheDocument();
    expect(recording).not.toBeChecked();
    expect(autoSync).not.toBeChecked();
    expect(calls("set_usage_settings")).toHaveLength(0);

    fireEvent.click(within(dialog).getByRole("button", { name: "保存" }));
    await flush();
    expect(calls("set_usage_settings")).toHaveLength(1);
    expect(state.settings).toEqual({
      recording: false,
      autoSync: false,
      refreshSeconds: 5,
      multiplier: "9",
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
    expect(screen.getByRole("tab", { name: "定价" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(input).toHaveValue("2.5");
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    await user.click(screen.getByRole("tab", { name: "供应商" }));
    await waitFor(() =>
      expect(screen.getByRole("tab", { name: "供应商" })).toHaveAttribute(
        "aria-selected",
        "true",
      ),
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    expect(onDirtyChange).toHaveBeenLastCalledWith(false);
    expect(calls("configure_pricing")).toHaveLength(0);
  });
});

describe("usage pricing", () => {
  it("filters model prices and preserves zero and missing rate displays", async () => {
    render(<Pricing />);
    await screen.findByRole("button", { name: "gpt-alpha" });
    fireEvent.change(screen.getByRole("textbox", { name: "搜索价格模型" }), {
      target: { value: "GPT-ALPHA" },
    });
    await waitFor(() =>
      expect(
        within(screen.getByRole("table")).getAllByRole("row"),
      ).toHaveLength(2),
    );
    const rows = within(screen.getByRole("table")).getAllByRole("row");
    expect(rows).toHaveLength(2);
    const cells = within(rows[1]).getAllByRole("cell");
    expect(cells[1]).toHaveTextContent(/^1$/);
    expect(cells[2]).toHaveTextContent(/^2$/);
    expect(cells[3]).toHaveTextContent(/^0$/);
    expect(cells[4]).toHaveTextContent(/^—$/);
    expect(
      screen.queryByRole("button", { name: "gpt-beta" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "claude-gamma" }),
    ).not.toBeInTheDocument();
    expect(calls("configure_pricing")).toHaveLength(0);
  });

  it("fixes a price including zero and restores automatic rates with the latest revision", async () => {
    const user = userEvent.setup();
    pricing.config.excluded = ["claude-gamma"];
    render(<Pricing />);
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const editor = screen.getByRole("dialog");
    fireEvent.change(within(editor).getByLabelText("输入 · USD / 百万 Token"), {
      target: { value: "0" },
    });
    fireEvent.change(within(editor).getByLabelText("输出 · USD / 百万 Token"), {
      target: { value: "2.75" },
    });
    await user.click(within(editor).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    const firstSave = calls("configure_pricing")[0][1];
    expect(firstSave.expectedRevision).toBe("fixture-revision-1");
    expect(firstSave.config.fixed["gpt-alpha"]).toMatchObject({
      input_cost_per_token: "0.000000",
      output_cost_per_token: "0.00000275",
      cache_read_input_token_cost: "0.000000",
      output_cost_per_image: "0.01",
    });
    expect(firstSave.config.fixed["gpt-alpha"]).not.toHaveProperty(
      "cache_creation_input_token_cost",
    );
    expect(firstSave.config.excluded).toEqual(["claude-gamma"]);
    const fixedRow = screen
      .getByRole("button", { name: "gpt-alpha" })
      .closest("tr")!;
    expect(within(fixedRow).getByText("固定")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "gpt-alpha" }));
    await user.click(
      within(screen.getByRole("dialog")).getByRole("button", {
        name: "恢复自动价格",
      }),
    );
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    const restored = calls("configure_pricing")[1][1];
    expect(restored.expectedRevision).toBe("fixture-revision-2");
    expect(restored.config.fixed).not.toHaveProperty("gpt-alpha");
    expect(restored.config.excluded).toEqual(["claude-gamma"]);
    expect(restored.config.aliases).toEqual({ "fixture-alias": "gpt-alpha" });
    const restoredCells = within(
      screen.getByRole("button", { name: "gpt-alpha" }).closest("tr")!,
    ).getAllByRole("cell");
    expect(restoredCells[1]).toHaveTextContent(/^1$/);
    expect(restoredCells[2]).toHaveTextContent(/^2$/);
    expect(restoredCells[5]).toHaveTextContent(/^Sub2API$/);
  });

  it("returns focus to the price editor and sync entry points after cancellation", async () => {
    render(<Pricing />);
    const modelButton = await screen.findByRole("button", {
      name: "gpt-alpha",
    });
    fireEvent.click(modelButton);
    const editor = await screen.findByRole("dialog");
    cancelDialog(editor);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    await waitFor(() => expect(modelButton).toHaveFocus());

    const syncButton = screen.getByRole("button", { name: "自动同步" });
    fireEvent.click(syncButton);
    const sync = await screen.findByRole("dialog");
    cancelDialog(sync);
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    await waitFor(() => expect(syncButton).toHaveFocus());
  });

  it("changes only filtered sync selections and saves price sync independently", async () => {
    const user = userEvent.setup();
    render(<Pricing />);
    await screen.findByRole("button", { name: "gpt-alpha" });
    await user.click(screen.getByRole("button", { name: "自动同步" }));
    const dialog = screen.getByRole("dialog");
    fireEvent.change(
      within(dialog).getByRole("textbox", { name: "搜索同步模型" }),
      { target: { value: "GPT-" } },
    );
    expect(
      within(dialog).queryByRole("checkbox", { name: /claude-gamma/ }),
    ).not.toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "清空结果" }));
    expect(
      within(dialog).getByRole("checkbox", { name: /gpt-alpha/ }),
    ).not.toBeChecked();
    expect(
      within(dialog).getByRole("checkbox", { name: /gpt-beta/ }),
    ).not.toBeChecked();
    await user.click(within(dialog).getByRole("button", { name: "全选结果" }));
    await user.click(
      within(dialog).getByRole("checkbox", { name: /gpt-beta/ }),
    );
    await user.click(
      within(dialog).getByRole("checkbox", { name: "自动更新" }),
    );
    expect(calls("configure_pricing")).toHaveLength(0);
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(pricing.config.selected?.slice().sort()).toEqual([
      "claude-gamma",
      "gpt-alpha",
    ]);
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
      config: {
        ...pricing.config,
        excluded: ["claude-gamma"],
        aliases: { "external-alias": "gpt-beta" },
      },
      models: {
        ...pricing.models,
        "gpt-alpha": {
          ...pricing.models["gpt-alpha"],
          input_cost_per_token: "0.000099",
        },
      },
      revision: `fixture-revision-${++pricingRevision}`,
    };
    act(() => mock.listeners.get("pricing-state")?.(pricing));
    const row = screen
      .getByRole("button", { name: "gpt-alpha" })
      .closest("tr")!;
    await waitFor(() =>
      expect(within(row).getAllByRole("cell")[1]).toHaveTextContent(/^99$/),
    );
    expect(input).toHaveValue("2.5");
    expect(calls("configure_pricing")).toHaveLength(0);

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(calls("configure_pricing")[0][1]).toMatchObject({
      expectedRevision: "fixture-revision-2",
      config: {
        fixed: {
          "gpt-alpha": {
            input_cost_per_token: "0.0000025",
            output_cost_per_image: "0.01",
          },
        },
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
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "价格配置已被外部修改，请重新保存",
    );
    expect(calls("get_pricing")).toHaveLength(2);
    expect(calls("configure_pricing")[0][1].expectedRevision).toBe(
      "fixture-revision-1",
    );
    expect(output).toHaveValue("2.75");
    expect(pricing.config.fixed).not.toHaveProperty("gpt-alpha");
    expect(onDirtyChange).toHaveBeenLastCalledWith(true);

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(calls("configure_pricing")).toHaveLength(2);
    expect(calls("configure_pricing")[1][1]).toMatchObject({
      expectedRevision: "fixture-revision-2",
      config: {
        fixed: {
          "gpt-alpha": { output_cost_per_token: "0.00000275" },
          "gpt-beta": externalPrice,
        },
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
    const automatic = within(dialog).getByRole("checkbox", {
      name: "自动更新",
    });
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
    expect(
      within(dialog).getByRole("checkbox", { name: /gpt-alpha/ }),
    ).toBeChecked();
    expect(calls("configure_pricing")).toHaveLength(0);

    pricing = { ...pricing, revision: `fixture-revision-${++pricingRevision}` };
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "价格配置已被外部修改，请重新保存",
    );
    expect(calls("configure_pricing")[0][1].expectedRevision).toBe(
      "fixture-revision-2",
    );
    expect(calls("get_pricing")).toHaveLength(3);
    expect(automatic).not.toBeChecked();
    expect(beta).not.toBeChecked();

    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(calls("configure_pricing")[1][1].expectedRevision).toBe(
      "fixture-revision-3",
    );
    expect(pricing.config.autoUpdate).toBe(false);
    expect(pricing.config.selected?.slice().sort()).toEqual([
      "claude-gamma",
      "gpt-alpha",
    ]);
  });
});

describe("usage navigation", () => {
  it("opens usage and logs from the sidebar and protects price drafts from page and native navigation", async () => {
    const user = userEvent.setup();
    render(<App />);
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("frontend_ready"),
    );
    const navigation = screen.getByRole("navigation", { name: "主导航" });
    await user.click(within(navigation).getByRole("button", { name: "用量" }));
    await screen.findByRole("tab", { name: "请求日志" });
    await user.click(screen.getByRole("tab", { name: "定价" }));
    await user.click(await screen.findByRole("button", { name: "gpt-alpha" }));
    const input = within(screen.getByRole("dialog")).getByLabelText(
      "输入 · USD / 百万 Token",
    );
    fireEvent.change(input, { target: { value: "2.5" } });
    mock.confirm.mockResolvedValue(false);

    await user.click(within(navigation).getByRole("button", { name: "日志" }));
    expect(mock.confirm).toHaveBeenLastCalledWith(
      "离开当前页面会丢弃未保存的表单。",
    );
    await act(async () => mock.listeners.get("navigate")?.("settings"));
    expect(mock.confirm).toHaveBeenLastCalledWith(
      "打开设置会丢弃未保存的草稿。",
    );
    await act(async () =>
      mock.listeners.get("provider-settings")?.({
        id: "provider-final",
        clientId: "codex",
      }),
    );
    expect(mock.confirm).toHaveBeenLastCalledWith(
      "打开白名单会丢弃未保存的草稿。",
    );
    await act(async () => mock.listeners.get("navigate")?.("accounts"));
    expect(mock.confirm).toHaveBeenLastCalledWith(
      "离开当前页面会丢弃未保存的表单。",
    );
    expect(input).toHaveValue("2.5");
    expect(screen.getByRole("tab", { name: "定价" })).toHaveAttribute(
      "aria-selected",
      "true",
    );
    expect(calls("configure_pricing")).toHaveLength(0);

    mock.confirm.mockResolvedValue(true);
    await import("./EventLog");
    await user.click(within(navigation).getByRole("button", { name: "日志" }));
    await screen.findByRole("button", { name: "清空全部日志" });
    await waitFor(() =>
      expect(calls("get_app_events").length).toBeGreaterThan(0),
    );
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    const confirmations = mock.confirm.mock.calls.length;
    await user.click(within(navigation).getByRole("button", { name: "用量" }));
    expect(
      await screen.findByRole("tab", { name: "请求日志" }),
    ).toHaveAttribute("aria-selected", "true");
    expect(mock.confirm).toHaveBeenCalledTimes(confirmations);
  });
});

it("accepts only the latest detail selection and invalidates pending reads on close and unmount", async () => {
  records = [record({ id: "fixture-first" }), record({ id: "fixture-second" })];
  totalRows = 2;
  const original = mock.command.getMockImplementation()!;
  const pending: ((value: UsageRecord) => void)[] = [];
  mock.command.mockImplementation((name, args) =>
    name === "get_usage_detail"
      ? new Promise<UsageRecord>((resolve) => {
          pending.push(resolve);
        })
      : original(name, args),
  );
  const view = await renderUsage();
  const rows = within(screen.getByRole("table")).getAllByRole("row").slice(1);
  fireEvent.click(rows[0]);
  fireEvent.click(rows[1]);
  const detail = (index: number) =>
    record({ ...records[index], sessionId: `fixture-detail-${index}` });
  await act(async () => {
    pending[1](detail(1));
  });
  expect(screen.getByRole("dialog")).toHaveTextContent("fixture-second");
  await act(async () => {
    pending[0](detail(0));
  });
  expect(screen.getByRole("dialog")).toHaveTextContent("fixture-second");
  expect(screen.getByRole("dialog")).not.toHaveTextContent("fixture-first");
  fireEvent.click(rows[0]);
  cancelDialog(screen.getByRole("dialog"));
  await act(async () => {
    pending[2](detail(0));
  });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  fireEvent.click(rows[0]);
  view.unmount();
  await act(async () => {
    pending[3](detail(0));
  });
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
});

it("reports the selected source's latest outcome and exposes mixed failures", () => {
  const report = {
    files: 1,
    imported: 1,
    skipped: 0,
    errors: 0,
    completedAt: 100,
    phase: "done",
  };
  const value = {
    settings: {} as UsageSettings,
    syncing: false,
    error: null,
    reports: {
      claude: report,
      antigravity: { ...report, phase: "failed", errors: 1 },
    },
  };
  expect(syncStatus(value, "")).toBe("部分未同步");
  expect(syncStatus(value, "antigravity")).toBe("同步失败");
  expect(syncStatus(value, "claude")).toBe("已同步");
  value.reports.antigravity.phase = "partial";
  expect(syncStatus(value, "antigravity")).toBe("部分未同步");
  value.reports.antigravity.phase = "syncing";
  expect(syncStatus(value, "antigravity")).toBe("同步中…");
  expect(syncStatus(value, "codex")).toBe("");
});

it("requests fresh state after showing and ignores an older hidden response", async () => {
  const original = mock.command.getMockImplementation()!;
  const reads: ((value: UsageState) => void)[] = [];
  mock.command.mockImplementation((name, args) =>
    name === "get_usage_state"
      ? new Promise<UsageState>((resolve) => reads.push(resolve))
      : original(name, args),
  );
  render(<Usage />);
  await waitFor(() => expect(reads).toHaveLength(1));
  await act(async () => mock.listeners.get("app-visibility")!(false));
  await act(async () => mock.listeners.get("app-visibility")!(true));
  await waitFor(() => expect(reads).toHaveLength(2));
  const fresh = structuredClone(state);
  fresh.reports.antigravity = {
    files: 1,
    imported: 1,
    skipped: 0,
    errors: 1,
    phase: "failed",
    completedAt: null,
  };
  await act(async () => reads[1](fresh));
  expect(screen.getByText("同步失败")).toBeInTheDocument();
  await act(async () => reads[0](structuredClone(state)));
  expect(screen.getByText("同步失败")).toBeInTheDocument();
});
