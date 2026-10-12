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

const startedAt = Date.UTC(2026, 9, 8, 2, 10, 20, 123);
const finishedAt = startedAt + 4_321;
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
    id: "fixture-v022-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-response",
    responseModel: "fixture-response",
    pricingModel: "fixture-response",
    responseId: "fixture-response-id",
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt,
    durationMs: finishedAt - startedAt,
    firstTokenMs: 250,
    stream: true,
    transport: "http",
    tokens: {
      input: 10,
      output: 20,
      cacheRead: null,
      cacheWrite: null,
    },
    serviceTier: null,
    price: null,
    transfer: {
      receivedAt: startedAt,
      sentAt: startedAt + 100,
      firstByteAt: startedAt + 500,
      terminalAt: finishedAt,
      endedAt: finishedAt + 456,
      upstreamBytes: 128,
      downstreamBytes: 256,
      terminalHandedOff: true,
      upstreamEof: true,
      upstreamEnd: "EOF",
      wsCloseCode: null,
      downstreamError: null,
      timeoutPhase: null,
      timeoutSeconds: null,
      lastProgressMs: null,
      clientRequestHash: null,
      responseHash: null,
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v022-request",
    client: "codex",
    source: "proxy",
    startedAt,
    finishedAt,
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
      requests: records.length,
      attempts: records.length,
      success: records.length,
      statusKnown: records.length,
      sessions: 0,
      tokens: { input: 10, output: 20, cacheRead: null, cacheWrite: null },
      cost: "0",
      unpriced: records.length,
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
            providers: [
              { id: "fixture-provider", name: "Fixture Provider" },
            ],
          };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard();
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          const page = filter.page ?? 1;
          const rows: UsagePage["rows"] = records.slice(
            (page - 1) * 20,
            page * 20,
          );
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
  if (!dt) throw new Error(`missing detail field: ${label}`);
  return dt.nextElementSibling as HTMLElement;
}

it("uses the second-level finishedAt value for rows and the cell title", async () => {
  render(<Usage />);
  const table = await screen.findByRole("table");
  expect(within(table).getByRole("columnheader", { name: "结束时间" })).toBeInTheDocument();
  const row = within(table).getAllByRole("row")[1];
  const cell = within(row).getAllByRole("cell")[0];
  expect(cell).toHaveAttribute(
    "title",
    new Date(finishedAt).toLocaleString("zh-CN", {
      year: "numeric",
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      fractionalSecondDigits: 3,
      hour12: false,
    }),
  );
  expect(cell).toHaveTextContent(
    new Date(finishedAt).toLocaleString("zh-CN", {
      month: "2-digit",
      day: "2-digit",
      hour: "2-digit",
      minute: "2-digit",
      second: "2-digit",
      hour12: false,
    }),
  );
});

it("opens one detail request and preserves protocol end precision and event-time basis", async () => {
  const user = userEvent.setup();
  render(<Usage />);
  const row = (await screen.findByRole("table")).querySelector("tbody tr")!;
  await user.click(row);
  const dialog = await screen.findByRole("dialog", { name: "请求详情" });
  expect(
    mock.command.mock.calls.filter(([name]) => name === "get_usage_detail"),
  ).toHaveLength(1);
  expect(detailValue(dialog, "列表时间依据")).toHaveTextContent("协议终态");
  expect(detailValue(dialog, "协议终态")).toHaveTextContent(".444");
  expect(detailValue(dialog, "本地结束")).toHaveTextContent(".900");
});

it("labels a session row by its finished event and keeps one session detail request", async () => {
  records = [
    record({
      id: "fixture-v022-session",
      source: "codex",
      finishedAt: finishedAt + 9_876,
      attempts: [
        attempt({
          id: "fixture-v022-session-attempt",
          provider: null,
          status: null,
          stream: false,
          transport: "session",
          transfer: null,
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);
  const row = (await screen.findByRole("table")).querySelector("tbody tr")!;
  await user.click(row);
  const dialog = await screen.findByRole("dialog", { name: "请求详情" });
  expect(detailValue(dialog, "列表时间依据")).toHaveTextContent("会话事件时间");
  expect(within(dialog).getByText("会话")).toBeInTheDocument();
  expect(
    mock.command.mock.calls.filter(([name]) => name === "get_usage_detail"),
  ).toHaveLength(1);
});
