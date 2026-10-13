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

type TransferWithDiagnostics = NonNullable<Attempt["transfer"]>;

type AttemptFixture = Omit<Attempt, "transfer"> & {
  transfer: TransferWithDiagnostics | null;
};

type RecordFixture = Omit<UsageRecord, "attempts"> & {
  attempts: AttemptFixture[];
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

function transfer(
  overrides: Partial<TransferWithDiagnostics> = {},
): TransferWithDiagnostics {
  return {
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
    wsCloseCode: null,
    downstreamError: null,
    timeoutPhase: "idle",
    timeoutSeconds: 600,
    lastProgressMs: 23,
    clientRequestHash: null,
    responseHash: null,
    ...overrides,
  };
}

function attempt(overrides: Partial<AttemptFixture> = {}): AttemptFixture {
  return {
    id: "fixture-v0231-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: "fixture-response-id",
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt,
    durationMs: 5_000,
    firstTokenMs: 250,
    stream: true,
    transport: "http",
    tokens: {
      input: 1_000,
      output: 2_000,
      cacheRead: 3_000,
      cacheWrite: 4_000,
    },
    serviceTier: null,
    price: null,
    transfer: transfer(),
    ...overrides,
  };
}

function record(overrides: Partial<RecordFixture> = {}): RecordFixture {
  return {
    id: "fixture-v0231-request",
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
      requests: records.length,
      attempts: records.length,
      success: records.length,
      statusKnown: records.length,
      sessions: 0,
      tokens: { input: 1_000, output: 2_000, cacheRead: 3_000, cacheWrite: 4_000 },
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

let records: RecordFixture[];

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

async function openFirstDetail() {
  render(<Usage />);
  const user = userEvent.setup();
  const row = (await screen.findByRole("table")).querySelector("tbody tr");
  if (!row) throw new Error("用量列表缺少记录");
  await user.click(row);
  return screen.findByRole("dialog", { name: "请求详情" });
}

it("distinguishes an unknown incomplete upstream terminal and shows its diagnostics", async () => {
  records = [
    record({
      id: "fixture-upstream-incomplete",
      completed: false,
      attempts: [
        attempt({
          outcome: "unknown",
          end: {
            cause: "upstream_incomplete",
            terminalReceived: true,
            responseStarted: true,
          },
          transfer: transfer({
            terminalEvent: "response.incomplete",
            terminalStatus: "incomplete",
            incompleteReason: "unrecognized",
            upstreamClientRequestHash: "0123456789abcdef0123456789abcdef",
          }),
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "结束原因")).toHaveTextContent(
    "上游未完成，原因未确认",
  );
  expect(within(dialog).queryByText("上游返回应用错误")).not.toBeInTheDocument();
  expect(within(dialog).queryByText("上游已完成")).not.toBeInTheDocument();

  expect(detailValue(dialog, "终态事件")).toHaveTextContent("response.incomplete");
  expect(detailValue(dialog, "终态状态")).toHaveTextContent("incomplete");
  expect(detailValue(dialog, "未完成原因")).toHaveTextContent("未识别");
  expect(detailValue(dialog, "未完成原因")).not.toHaveTextContent("unrecognized");
  expect(detailValue(dialog, "上游关联摘要")).toHaveTextContent(/^0123456789ab$/);
});

it("keeps a completed upstream result separate from a downstream error", async () => {
  records = [
    record({
      id: "fixture-completed-downstream-error",
      attempts: [
        attempt({
          outcome: "success",
          end: {
            cause: "completed",
            terminalReceived: true,
            responseStarted: true,
          },
          transfer: transfer({ downstreamError: "fixture-downstream-write-error" }),
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "结束原因")).toHaveTextContent("上游已完成");
  expect(detailValue(dialog, "下游错误")).toHaveTextContent(
    "fixture-downstream-write-error",
  );
  expect(within(dialog).getByText("已完成")).toBeInTheDocument();
  expect(within(dialog).queryByText("提前结束")).not.toBeInTheDocument();
  expect(within(dialog).queryByText("结果未确认")).not.toBeInTheDocument();
});

it("keeps older transfer records without optional terminal diagnostics readable", async () => {
  records = [
    record({
      id: "fixture-legacy-transfer",
      attempts: [
        attempt({
          end: null,
          transfer: transfer(),
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "结束原因")).toHaveTextContent("历史结束原因未记录");
  expect(detailValue(dialog, "上游结束")).toHaveTextContent("正常 EOF");
  expect(within(dialog).getByText("已完成")).toBeInTheDocument();
});

it("shows a known incomplete reason and omits absent optional diagnostic fields", async () => {
  records = [
    record({
      id: "fixture-known-incomplete-reason",
      completed: false,
      attempts: [
        attempt({
          outcome: "unknown",
          end: {
            cause: "upstream_incomplete",
            terminalReceived: true,
            responseStarted: true,
          },
          transfer: transfer({ incompleteReason: "max_output_tokens" }),
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "未完成原因")).toHaveTextContent("max_output_tokens");
  for (const label of ["终态事件", "终态状态", "上游关联摘要"]) {
    expect(within(dialog).queryByText(label, { selector: "dt" })).not.toBeInTheDocument();
  }
});
