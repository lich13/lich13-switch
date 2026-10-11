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
    id: "fixture-v0211-attempt",
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
    price: null,
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v0211-request",
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
      cost: "0",
      unpriced: 1,
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
          return {
            providers: [{ id: "fixture-provider", name: "Fixture Provider" }],
          };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard();
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          const page = filter.page ?? 1;
          const rows = records.slice((page - 1) * 20, page * 20);
          return {
            rows: structuredClone(rows),
            total: records.length,
            page,
            detailSince: 0,
          } satisfies UsagePage;
        }
        case "get_usage_detail":
          return structuredClone(records.find((item) => item.id === args.id));
        default:
          throw new Error(`Unexpected fixture command: ${name}`);
      }
    },
  );
});

it("renders explicit lifecycle causes and keeps legacy proxy cancellations marked as unrecorded", async () => {
  records = [
    record({
      id: "fixture-history-cancel",
      attempts: [
        attempt({
          id: "attempt-history-cancel",
          outcome: "cancelled",
          end: null,
        }),
      ],
    }),
    record({
      id: "fixture-disconnected-unknown",
      attempts: [
        attempt({
          id: "attempt-disconnected-unknown",
          outcome: "unknown",
          end: {
            cause: "client_disconnected",
            terminalReceived: false,
            responseStarted: true,
          },
        }),
      ],
      completed: false,
    }),
    record({
      id: "fixture-disconnected-after-complete",
      attempts: [
        attempt({
          id: "attempt-disconnected-after-complete",
          end: {
            cause: "client_disconnected",
            terminalReceived: true,
            responseStarted: true,
          },
        }),
      ],
    }),
    record({
      id: "fixture-gateway-stop",
      attempts: [
        attempt({
          id: "attempt-gateway-stop",
          outcome: "cancelled",
          end: {
            cause: "gateway_stopped",
            terminalReceived: false,
            responseStarted: true,
          },
        }),
      ],
      completed: false,
    }),
    record({
      id: "fixture-unsettled",
      attempts: [
        attempt({
          id: "attempt-unsettled",
          outcome: "unknown",
          end: {
            cause: "unsettled",
            terminalReceived: false,
            responseStarted: false,
          },
        }),
      ],
      completed: false,
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);
  const table = await screen.findByRole("table");
  const rows = within(table).getAllByRole("row");
  expect(rows).toHaveLength(records.length + 1);
  expect(within(rows[1]).getByText("fixture-response")).toBeInTheDocument();

  const cases = [
    ["历史结束原因未记录"],
    ["客户端断开，完成状态未知"],
    ["上游已完成，客户端随后断开"],
    ["网关停止"],
    ["结束原因未确认"],
  ];
  for (const [label] of cases) {
    const index = cases.findIndex(([expected]) => expected === label);
    const target = within(screen.getByRole("table")).getAllByRole("row")[
      index + 1
    ];
    await user.click(target);
    const drawer = await screen.findByRole("dialog", { name: "请求详情" });
    expect(within(drawer).getAllByText(label).length).toBeGreaterThan(0);
    await user.click(within(drawer).getByRole("button", { name: "关闭详情" }));
    expect(
      screen.queryByRole("dialog", { name: "请求详情" }),
    ).not.toBeInTheDocument();
  }
});

it("explains a completed upstream response followed by a client disconnect", async () => {
  records = [
    record({
      id: "fixture-success-disconnect",
      attempts: [
        attempt({
          end: {
            cause: "client_disconnected",
            terminalReceived: true,
            responseStarted: true,
          },
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);
  await user.click(
    (await screen.findByText("fixture-response")).closest("tr")!,
  );
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  expect(
    within(drawer).getAllByText("上游已完成，客户端随后断开").length,
  ).toBeGreaterThan(0);
  expect(
    within(drawer).queryByText("客户端断开，完成状态未知"),
  ).not.toBeInTheDocument();
});
