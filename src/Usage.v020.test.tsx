import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type {
  Attempt,
  Dashboard,
  UsageFilter,
  UsagePage,
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

import Usage from "./Usage";

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
    id: "fixture-v020-attempt",
    provider: "fixture-provider",
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
    tokens: {
      input: 1000,
      output: 2000,
      cacheRead: 3000,
      cacheWrite: 4000,
    },
    serviceTier: null,
    price: {
      version: "fixture-price-version",
      source: "fixture",
      model: "fixture-priced",
      multiplier: "1",
      rates: {},
      cost: "1.0000",
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v020-request",
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

function dashboard(): Dashboard {
  return {
    totals: {
      requests: 1,
      attempts: 1,
      success: 1,
      statusKnown: 1,
      sessions: 0,
      tokens: { input: 1000, output: 2000, cacheRead: 3000, cacheWrite: 4000 },
      cost: "1.0000",
      unpriced: 0,
      durationMs: 0,
      measuredOutputs: 0,
      generationMs: 0,
    },
    providers: [],
    models: [],
    precision: "millisecond",
    detailSince: 0,
    sources: {},
  };
}

let records: UsageRecord[];

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  records = [record()];
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_gateway":
          return { providers: [{ id: "fixture-provider", name: "Fixture Provider" }] };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard();
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          const page = filter.page ?? 1;
          const rows: UsagePage["rows"] = records.slice((page - 1) * 20, page * 20);
          return { rows: structuredClone(rows), total: records.length, page, detailSince: 0 };
        }
        case "get_usage_detail":
          return structuredClone(records.find((item) => item.id === args.id));
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
});

it("renders null table quantities as dashes while keeping decimal K/M/B compact values", async () => {
  records = [
    record({
      id: "fixture-null-table",
      attempts: [
        attempt({
          requestedModel: null,
          responseModel: null,
          tokens: { input: null, output: null, cacheRead: null, cacheWrite: null },
        }),
      ],
    }),
    record({
      id: "fixture-compact-table",
      attempts: [
        attempt({
          tokens: { input: 1234, output: 1234567, cacheRead: 1234567890, cacheWrite: null },
        }),
      ],
    }),
  ];
  render(<Usage />);

  const compactRow = (await screen.findByText("1.23M")).closest("tr")!;
  expect(within(compactRow).getByText("1.23M")).toBeInTheDocument();
  expect(within(compactRow).getByText("1.23B")).toBeInTheDocument();
  expect(within(compactRow).getByText("1.23K")).toBeInTheDocument();
  const missingRow = within(screen.getByRole("table")).getAllByRole("row")[1];
  expect(missingRow.querySelectorAll("td")[2]).toHaveTextContent("—");
  expect(missingRow.querySelectorAll("td")[3]).toHaveTextContent("—");
});

it("shows reasoned missing token fields and preserves a real zero duration", async () => {
  records = [
    record({
      id: "fixture-missing-detail",
      attempts: [
        attempt({
          tokens: { input: null, output: null, cacheRead: null, cacheWrite: null },
          durationMs: 0,
          firstTokenMs: null,
          stream: false,
          availability: {
            input: "upstream_unreported",
            output: "ended_early",
            cache_read: "parse_incomplete",
            cache_write: "historical_missing",
            first_token: "not_applicable",
          },
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);

  await user.click((await screen.findByText("fixture-response")).closest("tr")!);
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  expect(within(drawer).getAllByLabelText("— · 上游未报告")).toHaveLength(2);
  expect(within(drawer).getAllByLabelText("— · 提前结束")).toHaveLength(2);
  expect(within(drawer).getAllByLabelText("— · 解析不完整")).toHaveLength(2);
  expect(within(drawer).getAllByLabelText("— · 历史缺失")).toHaveLength(2);
  for (const label of within(drawer).getAllByText("总耗时", { selector: "dt" })) {
    expect(label.nextElementSibling).toHaveTextContent("0.000s");
  }
  for (const label of within(drawer).getAllByText("首字", { selector: "dt" })) {
    const firstToken = label.nextElementSibling!;
    expect(firstToken).toHaveTextContent("—");
    expect(firstToken.querySelector("span")).toHaveAttribute("title", "不适用");
  }
});

it("marks a session first-token value as not applicable", async () => {
  records = [
    record({
      id: "fixture-session-detail",
      source: "codex",
      attempts: [
        attempt({
          responseModel: "fixture-session-model",
          firstTokenMs: null,
          availability: { first_token: "not_applicable" },
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);

  await user.click((await screen.findByText("fixture-session-model")).closest("tr")!);
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  for (const label of within(drawer).getAllByText("首字", { selector: "dt" })) {
    const firstToken = label.nextElementSibling!;
    expect(firstToken).toHaveTextContent("—");
    expect(firstToken.querySelector("span")).toHaveAttribute("title", "不适用");
  }
});
