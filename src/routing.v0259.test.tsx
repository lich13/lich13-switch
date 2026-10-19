import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { EventRecord } from "./EventLog";
import type {
  Attempt,
  Dashboard,
  UsageFilter,
  UsagePage,
  UsageRecord,
  UsageState,
} from "./usage-types";
import { gatewayDemo } from "./gateway-preview";
import {
  providerHandoffStatus,
  providerRuntimeStatus,
} from "./provider-status";
import type { GatewayState } from "./types";

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

import EventLog from "./EventLog";
import Usage from "./Usage";

const timestamp = Date.UTC(2026, 9, 8, 2);
const providerId = "fixture-provider";

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
    id: "fixture-v0259-attempt",
    provider: providerId,
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
    id: "fixture-v0259-request",
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

function dashboard(records: UsageRecord[]): Dashboard {
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

const baseEvent: EventRecord = {
  id: "fixture-v0259-event",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_030,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "thread_routing",
  action: "returned",
  level: "info",
  status: null,
  errorCode: "THREAD_ROUTING",
  attempt: 2,
};

let records: UsageRecord[];
let eventRecords: EventRecord[];

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  records = [record()];
  eventRecords = [baseEvent];
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_gateway":
          return {
            clientId: args.clientId,
            providers: [{ id: providerId, name: "Fixture Provider" }],
          };
        case "get_usage_state":
          return structuredClone(usageState);
        case "get_usage_dashboard":
          return dashboard(records);
        case "get_usage_logs": {
          const filter = args.filter as UsageFilter;
          const page = filter.page ?? 1;
          return {
            rows: structuredClone(records.slice((page - 1) * 20, page * 20)),
            total: records.length,
            page,
            detailSince: 0,
          } satisfies UsagePage;
        }
        case "get_usage_detail":
          return structuredClone(records.find((item) => item.id === args.id));
        case "get_app_events":
          return {
            items: structuredClone(eventRecords),
            total: eventRecords.length,
            page: 1,
            error: null,
          };
        case "get_app_event":
          return eventRecords.find((item) => item.id === args.id) ?? null;
        default:
          return undefined;
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

async function openUsageDetail() {
  render(<Usage />);
  const user = userEvent.setup();
  const row = await screen.findByRole("row", {
    name: /Fixture Provider/,
  });
  await user.click(row);
  return screen.findByRole("dialog", { name: "请求详情" });
}

async function openEventDetail() {
  render(<EventLog />);
  const user = userEvent.setup();
  await user.click(await screen.findByRole("button", { name: "线程路由结果" }));
  return screen.findByRole("dialog", { name: "线程路由结果" });
}

it("renders routing reason, identity, and boundary in usage details", async () => {
  records = [
    record({
      attempts: [
        attempt({
          routingDecision: "boundary_not_observed",
          routingEvidence: {
            input: "history",
            hasToolDeclarations: false,
            identity: "legacy_session",
            boundary: "claimed",
          },
        }),
      ],
    }),
  ];

  const dialog = await openUsageDetail();
  expect(detailValue(dialog, "线程调度")).toHaveTextContent(
    "未观察到可用压缩边界",
  );
  expect(detailValue(dialog, "线程身份")).toHaveTextContent(
    "旧式 session 回退",
  );
  expect(detailValue(dialog, "边界状态")).toHaveTextContent("边界领取中");
});

it("keeps legacy usage records free of new routing fields", async () => {
  records = [record({ attempts: [attempt()] })];
  const dialog = await openUsageDetail();
  for (const label of ["线程调度", "线程身份", "边界状态"]) {
    expect(
      within(dialog).queryByText(label, { selector: "dt" }),
    ).not.toBeInTheDocument();
  }
});

it("renders attempted routing as unconfirmed and preserves failure wording", async () => {
  eventRecords = [
    {
      ...baseEvent,
      details: {
        routing: {
          decision: "boundary_not_observed",
          evidence: {
            input: "history",
            hasToolDeclarations: false,
            identity: "reliable",
            boundary: "claimed",
          },
          outcome: "attempted",
          attemptId: "00000000-0000-0000-0000-000000000012",
          failure: "transport",
        },
      },
    },
  ];
  const dialog = await openEventDetail();
  expect(detailValue(dialog, "线程调度")).toHaveTextContent(
    "未观察到可用压缩边界",
  );
  expect(detailValue(dialog, "路由结果")).toHaveTextContent(
    "已尝试，尚未确认归属保存",
  );
  expect(detailValue(dialog, "线程身份")).toHaveTextContent("可靠线程标识");
  expect(detailValue(dialog, "边界状态")).toHaveTextContent("边界领取中");
  expect(detailValue(dialog, "故障转移依据")).toHaveTextContent("传输故障");
  expect(dialog).not.toHaveTextContent("归属已保存");
});

it("falls back safely for unknown routing enums and unknown failover failure", async () => {
  records = [
    record({
      failoverEvidence: {
        stage: "unknown_owner",
        failure: "future_failure" as never,
        eligibleCandidates: 0,
        exclusions: {},
        wait: null,
        exit: "future_exit",
      },
    }),
  ];
  const usageDialog = await openUsageDetail();
  expect(detailValue(usageDialog, "失败分类")).toHaveTextContent(
    "故障原因未确认",
  );
  await userEvent
    .setup()
    .click(within(usageDialog).getByRole("button", { name: "关闭详情" }));

  eventRecords = [
    {
      ...baseEvent,
      details: {
        routing: {
          decision: "future_decision",
          evidence: {
            input: "future_input",
            hasToolDeclarations: false,
            identity: "future_identity",
            boundary: "future_boundary",
          },
          outcome: "future_outcome",
          failure: "future_failure",
        },
      },
    },
  ];
  const eventDialog = await openEventDetail();
  expect(detailValue(eventDialog, "线程调度")).toHaveTextContent(
    "调度原因未记录",
  );
  expect(detailValue(eventDialog, "路由结果")).toHaveTextContent(
    "路由结果未确认",
  );
  expect(detailValue(eventDialog, "输入证据")).toHaveTextContent("未记录");
  expect(detailValue(eventDialog, "线程身份")).toHaveTextContent(
    "线程身份未确认",
  );
  expect(detailValue(eventDialog, "边界状态")).toHaveTextContent(
    "边界状态未确认",
  );
  expect(detailValue(eventDialog, "故障转移依据")).toHaveTextContent(
    "故障原因未确认",
  );
});

it("does not render routing fields for legacy event details", async () => {
  eventRecords = [{ ...baseEvent, details: undefined }];
  const dialog = await openEventDetail();
  for (const label of ["线程调度", "路由结果", "线程身份", "边界状态"]) {
    expect(
      within(dialog).queryByText(label, { selector: "dt" }),
    ).not.toBeInTheDocument();
  }
});

it("shows claim-in-flight handoff text while retaining the quota waiting wording", () => {
  const provider = structuredClone(gatewayDemo.providers[0]);
  const state = structuredClone(gatewayDemo) as GatewayState;
  expect(
    providerHandoffStatus(provider, {
      ...state,
      handoffWaiting: [{ providerId: provider.id, state: "claim_in_flight" }],
    }),
  ).toBe("正在尝试接管");
  expect(
    providerRuntimeStatus(provider, {
      ...state,
      capacityRetries: [
        { providerId: provider.id, retryIn: 9, existingOwner: true },
      ],
    }),
  ).toBe("临时限流 · 原供应商等待9秒");
});
