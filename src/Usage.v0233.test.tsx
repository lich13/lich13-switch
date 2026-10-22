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
    id: "fixture-v0233-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: null,
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt: timestamp,
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
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v0233-request",
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
      requests: records.length,
      attempts: records.length,
      success: records.length,
      statusKnown: records.length,
      sessions: 0,
      tokens: {
        input: 1_000,
        output: 2_000,
        cacheRead: 3_000,
        cacheWrite: 4_000,
      },
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
  const row = await screen.findByRole("row", { name: /请求详情/ });
  await user.click(row);
  return screen.findByRole("dialog", { name: "请求详情" });
}

it("explains known routing evidence in request details", async () => {
  records = [
    record({
      attempts: [
        attempt({
          routingEvidence: {
            input: "history",
            hasToolDeclarations: true,
            restriction: "capacity_full",
          },
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "输入证据")).toHaveTextContent(
    /^包含历史消息或工具结果$/,
  );
  expect(detailValue(dialog, "初始化声明")).toHaveTextContent(/^包含工具声明$/);
  expect(detailValue(dialog, "准入限制")).toHaveTextContent(
    /^并发或线程预留名额已满$/,
  );
});

it("shows unknown routing evidence enums as unrecorded and omits a false declaration", async () => {
  records = [
    record({
      attempts: [
        attempt({
          routingEvidence: {
            input: "future_input_kind",
            hasToolDeclarations: false,
            restriction: "future_restriction_kind",
          },
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(detailValue(dialog, "输入证据")).toHaveTextContent(/^未记录$/);
  expect(detailValue(dialog, "准入限制")).toHaveTextContent(/^未记录$/);
  expect(
    within(dialog).queryByText("future_input_kind"),
  ).not.toBeInTheDocument();
  expect(
    within(dialog).queryByText("future_restriction_kind"),
  ).not.toBeInTheDocument();
  expect(
    within(dialog).queryByText("初始化声明", { selector: "dt" }),
  ).not.toBeInTheDocument();
});

it("leaves routing evidence absent on legacy records", async () => {
  records = [record({ attempts: [attempt()] })];

  const dialog = await openFirstDetail();
  for (const label of ["输入证据", "初始化声明", "准入限制"]) {
    expect(
      within(dialog).queryByText(label, { selector: "dt" }),
    ).not.toBeInTheDocument();
  }
});

it("distinguishes a model detour from a permanent thread handoff", async () => {
  records = [
    record({
      attempts: [
        attempt({
          routingDecision: "model_detour_owner_preserved",
          routingEvidence: {
            input: "first_turn",
            hasToolDeclarations: false,
            restriction: "model_mismatch",
          },
        }),
      ],
    }),
  ];

  const dialog = await openFirstDetail();
  expect(
    within(dialog).getByText("模型绕行，线程归属保持"),
  ).toBeInTheDocument();
  expect(detailValue(dialog, "准入限制")).toHaveTextContent("模型白名单不匹配");
  expect(within(dialog).queryByText("压缩后接管")).not.toBeInTheDocument();
});
