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
import {
  compact,
  type Attempt,
  type Dashboard,
  type UsageFilter,
  type UsagePage,
  type UsageRecord,
  type UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async (name: string, callback: (value: unknown) => void) => {
    mock.listeners.set(name, callback);
    return () => mock.listeners.delete(name);
  }),
}));

import Usage from "./Usage";

type Source = UsageFilter["source"];
const timestamp = Date.UTC(2026, 9, 8, 2);
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

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: null,
    pricingBasis: "response",
    mappingRevision: null,
    repeatCount: 1,
    compactedUnpriced: null,
    status: 200,
    outcome: "success",
    startedAt: timestamp,
    durationMs: 5000,
    firstTokenMs: 1000,
    stream: true,
    transport: "http",
    tokens: {
      input: 1234,
      output: 1234567,
      cacheRead: 1234567890,
      cacheWrite: 0,
    },
    serviceTier: null,
    price: {
      version: "fixture-price-version",
      source: "fixture",
      model: "fixture-priced",
      multiplier: "1",
      rates: {},
      cost: "1.2345",
    },
    ...overrides,
  };
}

function record(source?: Source): UsageRecord {
  return {
    id: `fixture-record-${source ?? "all"}`,
    client: "codex",
    source: source === "sessions" ? "codex" : "proxy",
    startedAt: timestamp,
    sessionId: null,
    attempts: [attempt({ responseModel: `fixture-model-${source ?? "all"}` })],
    completed: true,
    estimatedSpeed: false,
    duplicateOf: null,
    deduplication: "",
  };
}

function dashboard(source?: Source): Dashboard {
  return {
    totals: {
      requests: source === "proxy" ? 1000 : source === "sessions" ? 400 : 1400,
      attempts: 1500,
      success: 1000,
      statusKnown: 1400,
      sessions: 0,
      tokens: { input: 1000000, output: 234000, cacheRead: 0, cacheWrite: 0 },
      cost: "12.3456",
      unpriced: 0,
      durationMs: 5000,
      measuredOutputs: 234000,
      generationMs: 4000,
    },
    trend: [],
    heatmap: [],
    providers: [],
    models: [],
    precision: "millisecond",
    detailSince: 0,
    sources: {},
  };
}

function page(filter: UsageFilter = {}): UsagePage {
  return {
    rows: [record(filter.source)],
    total: 40,
    page: filter.page ?? 1,
    detailSince: 0,
  };
}

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      const filter = args.filter as UsageFilter | undefined;
      switch (name) {
        case "get_gateway":
          return {
            providers: [{ id: "fixture-provider", name: "Fixture Provider" }],
          };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard(filter?.source);
        case "get_usage_logs":
          return page(filter);
        case "get_usage_heatmap":
          return [];
        case "get_usage_detail":
          return record(
            String(args.id).endsWith("sessions")
              ? "sessions"
              : String(args.id).endsWith("proxy")
                ? "proxy"
                : undefined,
          );
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
});

describe("decimal compact quantities", () => {
  it.each([
    [0, "0"],
    [999, "999"],
    [1000, "1K"],
    [1001, "1K"],
    [1234, "1.23K"],
    [999499, "999K"],
    [999500, "1M"],
    [1234567, "1.23M"],
    [999500000, "1B"],
    [1234567890, "1.23B"],
    [-1234, "-1.23K"],
  ])(
    "formats %s as %s with at most three significant digits",
    (value, expected) => {
      expect(compact(value as number)).toBe(expected);
    },
  );

  it.each([null, undefined, Number.NaN, Number.POSITIVE_INFINITY])(
    "keeps an unavailable quantity unknown (%s)",
    (value) => {
      expect(compact(value)).toBe("未提供");
    },
  );
});

it("renders compact token cells, exposes their full values on focus, and has no speed column", async () => {
  render(<Usage />);
  const row = (await screen.findByText("fixture-model-all")).closest("tr")!;
  expect(within(row).getByText("1.23K")).toHaveAttribute("title", "1,234");
  expect(within(row).getByText("1.23M")).toHaveAttribute(
    "aria-label",
    "1,234,567",
  );
  expect(within(row).getByText("1.23B")).toHaveAttribute(
    "title",
    "1,234,567,890",
  );
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
  const quantity = within(row).getByText("1.23K");
  fireEvent.focus(quantity);
  expect(quantity).toHaveTextContent("1,234");
  fireEvent.blur(quantity);
  expect(quantity).toHaveTextContent("1.23K");
});

it.each(["Escape", "blur"] as const)(
  "shows the exact overview token total on focus and dismisses it with %s",
  async (dismissal) => {
    const user = userEvent.setup();
    render(<Usage />);
    await screen.findByText("fixture-model-all");
    const metric = screen
      .getByText("实际 Token")
      .closest<HTMLElement>(".usage-metric")!;
    const controls = within(metric);
    const quantity = controls.getByText("1.23M");
    expect(quantity).toHaveAttribute("tabindex", "0");
    expect(quantity).toHaveAttribute("title", "1,234,000");
    expect(quantity).toHaveAttribute("aria-label", "1,234,000");
    expect(controls.queryByRole("tooltip")).not.toBeInTheDocument();

    act(() => quantity.focus());

    expect(quantity).toHaveFocus();
    expect(controls.getByRole("tooltip")).toBeVisible();
    expect(controls.getByRole("tooltip")).toHaveTextContent("1,234,000");
    expect(quantity).toHaveTextContent("1.23M");

    if (dismissal === "Escape") {
      await user.keyboard("{Escape}");
      expect(quantity).toHaveFocus();
    } else {
      await user.tab();
      expect(quantity).not.toHaveFocus();
    }

    expect(controls.queryByRole("tooltip")).not.toBeInTheDocument();
    expect(quantity).toHaveTextContent("1.23M");
  },
);

it("applies source selection to totals, request pages and heatmaps and resets pagination", async () => {
  const user = userEvent.setup();
  render(<Usage />);
  await screen.findByText("fixture-model-all");
  const source = screen.getByRole("combobox", { name: "用量来源" });
  expect(source).toHaveValue("");
  expect(
    within(source)
      .getAllByRole("option")
      .map((option) => option.textContent),
  ).toEqual(["全部（去重）", "本网关", "本机会话"]);
  await user.click(screen.getByRole("button", { name: "下一页" }));
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("get_usage_logs", {
      filter: expect.objectContaining({ page: 2 }),
    }),
  );
  await user.selectOptions(source, "proxy");
  await screen.findByText("fixture-model-proxy");
  expect(mock.command).toHaveBeenCalledWith("get_usage_dashboard", {
    filter: expect.objectContaining({ source: "proxy" }),
  });
  expect(mock.command).toHaveBeenCalledWith("get_usage_logs", {
    filter: expect.objectContaining({ source: "proxy", page: 1 }),
  });
  await user.click(screen.getByRole("button", { name: "用量时间范围" }));
  const range = screen.getByRole("dialog", { name: "选择时间范围" });
  await user.selectOptions(
    within(range).getByRole("combobox", { name: "用量时间范围" }),
    "all",
  );
  await user.click(within(range).getByRole("button", { name: "确定" }));
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("get_usage_heatmap", {
      filter: expect.objectContaining({ source: "proxy" }),
    }),
  );
  await user.selectOptions(source, "sessions");
  await screen.findByText("fixture-model-sessions");
  expect(mock.command).toHaveBeenCalledWith("get_usage_dashboard", {
    filter: expect.objectContaining({ source: "sessions" }),
  });
  expect(mock.command).toHaveBeenCalledWith("get_usage_logs", {
    filter: expect.objectContaining({ source: "sessions", page: 1 }),
  });
  await user.selectOptions(source, "");
  await screen.findByText("fixture-model-all");
  const latest = mock.command.mock.calls
    .filter(([name]) => name === "get_usage_dashboard")
    .at(-1)!;
  expect((latest[1] as { filter: UsageFilter }).filter.source).toBeUndefined();
});

it("does not let an older source response replace the current selection", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  const pending: Array<() => void> = [];
  mock.command.mockImplementation(
    (name: string, args: Record<string, unknown> = {}) => {
      const filter = args.filter as UsageFilter | undefined;
      if (
        filter?.source === "sessions" &&
        (name === "get_usage_dashboard" || name === "get_usage_logs")
      ) {
        return new Promise((resolve) =>
          pending.push(() =>
            resolve(
              name === "get_usage_dashboard"
                ? dashboard("sessions")
                : page(filter),
            ),
          ),
        );
      }
      return original(name, args);
    },
  );
  render(<Usage />);
  await screen.findByText("fixture-model-all");
  const source = screen.getByRole("combobox", { name: "用量来源" });
  await user.selectOptions(source, "sessions");
  await waitFor(() => expect(pending).toHaveLength(2));
  await user.selectOptions(source, "proxy");
  await screen.findByText("fixture-model-proxy");
  await act(async () => {
    for (const resolve of pending) resolve();
  });
  expect(source).toHaveValue("proxy");
  expect(screen.getByText("fixture-model-proxy")).toBeInTheDocument();
  expect(screen.queryByText("fixture-model-sessions")).not.toBeInTheDocument();
});

it("marks provider mappings as estimates and reports compacted attempts with unknown costs", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  const detail = record();
  detail.attempts = [
    attempt({
      id: "fixture-summary",
      repeatCount: 1000,
      compactedUnpriced: 2,
      pricingBasis: "compacted",
      price: {
        version: "fixture-summary-price",
        source: "compacted",
        model: "fixture-priced",
        multiplier: "1",
        rates: {},
        cost: "2",
      },
    }),
    attempt({
      id: "fixture-final",
      repeatCount: 1,
      pricingBasis: "provider_mapping",
      mappingRevision: "fixture-mapping-revision",
    }),
  ];
  mock.command.mockImplementation(
    (name: string, args: Record<string, unknown> = {}) =>
      name === "get_usage_detail"
        ? Promise.resolve(detail)
        : original(name, args),
  );
  render(<Usage />);
  const row = (await screen.findByText("fixture-model-all")).closest("tr")!;
  await user.click(row);
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  expect(
    within(drawer).getAllByText("供应商精确映射（估算）").length,
  ).toBeGreaterThan(0);
  expect(
    within(drawer).getAllByText("fixture-mapping-revision").length,
  ).toBeGreaterThan(0);
  expect(within(drawer).getByText("含未定价项 · $3.2345")).toBeInTheDocument();
  expect(
    within(drawer).getByRole("heading", { name: "尝试记录" }),
  ).toBeInTheDocument();
  expect(
    within(drawer).getByText("实际尝试").nextElementSibling,
  ).toHaveTextContent("1,001");
  expect(within(drawer).getByText(/合并 1K 次/)).toBeInTheDocument();
  expect(within(drawer).queryByText(/速度|Token\/s/)).not.toBeInTheDocument();
});

it("keeps the historical source limitation inside the data-sources drawer", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation(
    (name: string, args: Record<string, unknown> = {}) =>
      name === "get_usage_dashboard"
        ? Promise.resolve({
            ...dashboard((args.filter as UsageFilter)?.source),
            sourceHistoryIncomplete: true,
          })
        : original(name, args),
  );
  render(<Usage />);
  await user.selectOptions(
    screen.getByRole("combobox", { name: "用量来源" }),
    "proxy",
  );
  await screen.findByText("fixture-model-proxy");
  expect(
    screen.queryByText("部分历史汇总无法按来源拆分"),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "数据来源" }));
  const sources = screen.getByRole("dialog", { name: "数据来源" });
  await user.click(
    within(sources).getByText("历史数据", { selector: "summary" }),
  );
  expect(
    within(sources).getByText("部分旧日汇总仅支持全部来源查询。"),
  ).toBeVisible();
});
