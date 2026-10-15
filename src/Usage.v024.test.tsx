import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type {
  Attempt,
  Dashboard,
  Tokens,
  UsageFilter,
  UsagePage,
  UsageRecord,
  UsageState,
} from "./usage-types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));

vi.mock("./bridge", () => ({
  preview: false,
  command: mock.command,
  subscribe: vi.fn(async () => () => {}),
}));

import Usage from "./Usage";

const timestamp = Date.UTC(2026, 9, 15, 2);
const firstTokens: Tokens = { input: 12, output: 34, cacheRead: 2, cacheWrite: 3 };
const secondTokens: Tokens = { input: 20, output: 56, cacheRead: 4, cacheWrite: 5 };
const totalTokens: Tokens = { input: 32, output: 90, cacheRead: 6, cacheWrite: 8 };

function attempt(id: string, model: string, usage: Tokens): Attempt {
  return {
    id,
    provider: null,
    requestedModel: model,
    responseModel: model,
    pricingModel: model,
    responseId: null,
    status: null,
    outcome: "completed",
    startedAt: timestamp,
    durationMs: 1_000,
    firstTokenMs: 100,
    stream: false,
    transport: "session",
    tokens: usage,
    serviceTier: null,
    price: null,
  };
}

const record: UsageRecord = {
  id: "fixture-grok-session",
  client: "grok",
  source: "grok",
  startedAt: timestamp,
  sessionId: "fixture-session",
  attempts: [
    attempt("fixture-grok-attempt-one", "fixture-grok-model-one", firstTokens),
    attempt("fixture-grok-attempt-two", "fixture-grok-model-two", secondTokens),
  ],
  completed: true,
  estimatedSpeed: false,
  duplicateOf: null,
  deduplication: "独立记录",
};

function modelGroup(id: string, requests: number, attempts: number): Dashboard["models"][number] {
  return {
    id,
    totals: {
      requests,
      attempts,
      success: 0,
      statusKnown: 0,
      sessions: 0,
      tokens: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      cost: "0",
      unpriced: attempts,
      durationMs: 0,
      measuredOutputs: 0,
      generationMs: 0,
    },
  };
}

const dashboard: Dashboard = {
  totals: {
    requests: 1,
    success: 1,
    statusKnown: 0,
    sessions: 1,
    tokens: totalTokens,
    cost: "42.50",
    unpriced: 1,
    durationMs: 1_000,
    measuredOutputs: 34,
    generationMs: 1_000,
  },
  providers: [],
  models: [
    modelGroup("fixture-grok-model-one", 0, 2),
    modelGroup("fixture-grok-model-two", 1, 1),
  ],
  precision: "millisecond",
  detailSince: 0,
  sources: { grok: 1 },
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

beforeEach(() => {
  mock.command.mockReset();
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_gateway":
          return { providers: [] };
        case "get_usage_dashboard":
          return structuredClone(dashboard);
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          return {
            rows: [structuredClone(record)],
            total: 1,
            page: filter.page ?? 1,
            detailSince: 0,
          } satisfies UsagePage;
        }
        case "get_usage_detail":
          return structuredClone(record);
        case "get_usage_state":
          return structuredClone(usageState);
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
});

function lastFilter(name: string): UsageFilter {
  const call = mock.command.mock.calls
    .filter(([command]) => command === name)
    .at(-1);
  if (!call) throw new Error(`Missing fixture command: ${name}`);
  return (call[1] as { filter: UsageFilter }).filter;
}

function detailValue(dialog: HTMLElement, label: string): HTMLElement {
  const term = within(dialog)
    .getAllByText(label, { selector: "dt" })
    .find((candidate) => !candidate.closest("details"));
  if (!term) throw new Error(`主详情区缺少字段：${label}`);
  return term.nextElementSibling as HTMLElement;
}

function estimatedCostValue(): HTMLElement {
  const label = screen.getByText("估算费用", {
    selector: ".usage-metric > span",
  });
  return label.nextElementSibling as HTMLElement;
}

it("filters Grok session summaries independently and labels their details", async () => {
  const user = userEvent.setup();
  render(<Usage />);

  await screen.findByRole("table");
  await waitFor(() => expect(estimatedCostValue()).not.toHaveTextContent(/^—$/));
  await user.selectOptions(screen.getByLabelText("用量客户端"), "grok");
  await user.selectOptions(screen.getByLabelText("用量来源"), "grok_sessions");

  expect(
    screen.getByRole("option", { name: "Grok 轮次汇总（独立）" }),
  ).toBeInTheDocument();
  expect(
    screen.getByText(/Grok 会话按用户轮次汇总/),
  ).toHaveTextContent("不与网关请求相加");
  expect(screen.getByRole("tab", { name: "轮次日志" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await waitFor(() => expect(estimatedCostValue()).toHaveTextContent(/^—$/));
  expect(estimatedCostValue()).toHaveAttribute("title", "轮次汇总不估算费用");
  await waitFor(() => {
    expect(lastFilter("get_usage_dashboard")).toMatchObject({
      client: "grok",
      source: "grok_sessions",
    });
    expect(lastFilter("get_usage_logs")).toMatchObject({
      client: "grok",
      source: "grok_sessions",
    });
  });

  const row = screen.getByRole("table").querySelector("tbody tr");
  if (!row) throw new Error("用量列表缺少 Grok 会话 fixture");
  const cells = row.querySelectorAll("td");
  expect(row).toHaveAttribute("aria-label", expect.stringContaining("轮次详情"));
  expect(cells[2]).toHaveTextContent("多个模型（2）");
  expect(cells[3]).toHaveTextContent("32");
  expect(cells[4]).toHaveTextContent("90");
  expect(cells[5]).toHaveTextContent("6");
  await user.click(row);
  const dialog = await screen.findByRole("dialog", { name: "Grok 轮次详情" });
  expect(detailValue(dialog, "来源")).toHaveTextContent(/^Grok Build 会话$/);
  expect(detailValue(dialog, "轮次 ID")).toHaveTextContent(/^fixture-grok-session$/);
  expect(detailValue(dialog, "模型调用")).toHaveTextContent(/^2$/);
  const modelUsageHeading = screen.getByRole("heading", { name: "模型用量" });
  expect(modelUsageHeading).toBeInTheDocument();
  const modelUsage = modelUsageHeading.nextElementSibling as HTMLElement;
  const aggregateCostLabel = within(modelUsage).getByText("估算费用", {
    selector: "dt",
  });
  expect(aggregateCostLabel.nextElementSibling).toHaveTextContent(/—$/);
  expect(mock.command).toHaveBeenCalledWith("get_usage_detail", {
    id: "fixture-grok-session",
    source: "grok_sessions",
  });
});

it("ranks Grok model groups by model calls and leaves unknown fees blank", async () => {
  const user = userEvent.setup();
  render(<Usage />);

  await screen.findByRole("table");
  await user.selectOptions(screen.getByLabelText("用量客户端"), "grok");
  await user.selectOptions(screen.getByLabelText("用量来源"), "grok_sessions");
  await waitFor(() => {
    expect(lastFilter("get_usage_dashboard")).toMatchObject({
      client: "grok",
      source: "grok_sessions",
    });
  });
  await user.click(screen.getByRole("tab", { name: "模型" }));

  const table = await waitFor(() => {
    const candidate = screen.getByRole("table");
    expect(candidate).toHaveClass("usage-ranking");
    return candidate;
  });
  const countButton = table.querySelector<HTMLButtonElement>(
    "thead th:nth-child(2) button",
  );
  if (!countButton) throw new Error("模型榜单缺少调用数排序表头");

  const rows = () =>
    Array.from(table.querySelectorAll("tbody tr"), (row) => {
      const cells = row.querySelectorAll("td");
      return {
        model: cells[0]?.textContent?.trim(),
        count: cells[1]?.textContent?.trim(),
        cost: cells[3]?.textContent?.trim(),
      };
    });
  const countLabel = countButton.textContent?.trim();
  const initialRows = rows();

  await user.click(countButton);
  await waitFor(() => expect(rows()).not.toEqual(initialRows));
  const sortedRows = rows();

  expect({ countLabel, initialRows, sortedRows }).toEqual({
    countLabel: "模型调用",
    initialRows: [
      { model: "fixture-grok-model-one", count: "2", cost: "—" },
      { model: "fixture-grok-model-two", count: "1", cost: "—" },
    ],
    sortedRows: [
      { model: "fixture-grok-model-two", count: "1", cost: "—" },
      { model: "fixture-grok-model-one", count: "2", cost: "—" },
    ],
  });
});
