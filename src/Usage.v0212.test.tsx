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

const startedAt = Date.UTC(2026, 9, 8, 2);
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
    id: "fixture-v0212-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: "fixture-response-id",
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt,
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
    transfer: {
      receivedAt: startedAt,
      sentAt: startedAt + 100,
      firstByteAt: startedAt + 500,
      terminalAt: startedAt + 4_000,
      endedAt: startedAt + 5_000,
      upstreamBytes: 1_024,
      downstreamBytes: 2_048,
      terminalHandedOff: true,
      upstreamEof: true,
      upstreamEnd: "EOF",
      wsCloseCode: 1000,
      downstreamError: null,
      timeoutPhase: "idle",
      timeoutSeconds: 600,
      lastProgressMs: 23,
      clientRequestHash: null,
      responseHash: null,
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v0212-request",
    client: "codex",
    source: "proxy",
    startedAt,
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
          return { providers: [{ id: "fixture-provider", name: "Fixture Provider" }] };
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

function detailValue(dialog: HTMLElement, label: string): HTMLElement {
  const dt = within(dialog)
    .getAllByText(label, { selector: "dt" })
    .find((candidate) => !candidate.closest("details"));
  if (!dt) throw new Error(`主详情区缺少字段：${label}`);
  return dt.nextElementSibling as HTMLElement;
}

it("labels the request table time column as end time", async () => {
  render(<Usage />);
  const table = await screen.findByRole("table");
  expect(within(table).getByRole("columnheader", { name: "结束时间" })).toBeInTheDocument();
  expect(within(table).queryByRole("columnheader", { name: "时间" })).not.toBeInTheDocument();
});

it("shows transfer timestamps, byte counts, and timeout metadata in request details", async () => {
  const user = userEvent.setup();
  render(<Usage />);
  await user.click((await screen.findByText("fixture-response")).closest("tr")!);
  const dialog = await screen.findByRole("dialog", { name: "请求详情" });

  for (const label of ["请求进入", "上游发送", "收到首字节", "协议终态", "本地结束"]) {
    expect(detailValue(dialog, label)).not.toHaveTextContent("—");
  }
  expect(detailValue(dialog, "上游读取").textContent).toMatch(/1,?024 B/);
  expect(detailValue(dialog, "交给下游网络层").textContent).toMatch(/2,?048 B/);
  expect(detailValue(dialog, "终态已交出")).toHaveTextContent("是");
  expect(detailValue(dialog, "上游结束")).toHaveTextContent("正常 EOF");
  expect(detailValue(dialog, "WS 关闭码")).toHaveTextContent("1000");
  expect(detailValue(dialog, "超时阶段")).toHaveTextContent("idle");
  expect(detailValue(dialog, "超时阈值")).toHaveTextContent("600 秒");
  expect(detailValue(dialog, "距最后进展")).toHaveTextContent("23 ms");
});
