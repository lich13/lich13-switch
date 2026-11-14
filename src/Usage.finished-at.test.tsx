import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type {
  Attempt,
  Dashboard,
  UsageFilter,
  UsagePage,
  UsageRecord,
} from "./usage-types";
import { mock } from "./test-bridge";
import { detailValue, usageDashboard, usageState } from "./test-fixtures";

vi.mock("./bridge", async () => (await import("./test-bridge")).bridge);

import Usage from "./Usage";

const startedAt = Date.UTC(2026, 9, 8, 2, 10, 20, 123);
const finishedAt = startedAt + 4_321;

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
    duplicateOf: null,
    deduplication: "",
    ...overrides,
  };
}

function dashboard(): Dashboard {
  return usageDashboard(records.length, {
    tokens: { input: 10, output: 20, cacheRead: null, cacheWrite: null },
  });
}

let records: UsageRecord[];
let usageLogsPending: Promise<void> | null = null;

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  records = [record()];
  usageLogsPending = null;
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
          if (usageLogsPending) await usageLogsPending;
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

it("uses the second-level finishedAt value for rows and the cell title", async () => {
  render(<Usage />);
  const table = await screen.findByRole("table");
  expect(
    within(table).getByRole("columnheader", { name: "时间 / 供应商 / 模型" }),
  ).toBeInTheDocument();
  const row = await screen.findByRole("row", { name: /请求详情$/ });
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
  let releaseUsageLogs!: () => void;
  usageLogsPending = new Promise<void>((resolve) => {
    releaseUsageLogs = resolve;
  });
  render(<Usage />);
  const table = await screen.findByRole("table");
  expect(within(table).getAllByRole("row")).toHaveLength(1);
  expect(
    within(table).queryByRole("row", { name: /请求详情$/ }),
  ).not.toBeInTheDocument();
  releaseUsageLogs();
  const row = await screen.findByRole("row", { name: /请求详情$/ });
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
  const row = await screen.findByRole("row", { name: /请求详情$/ });
  await user.click(row);
  const dialog = await screen.findByRole("dialog", { name: "请求详情" });
  expect(detailValue(dialog, "列表时间依据")).toHaveTextContent("会话事件时间");
  expect(within(dialog).getByText("会话")).toBeInTheDocument();
  expect(
    mock.command.mock.calls.filter(([name]) => name === "get_usage_detail"),
  ).toHaveLength(1);
});
