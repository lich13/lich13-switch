import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { EventRecord } from "./EventLog";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));

import EventLog from "./EventLog";

const baseRecord: EventRecord = {
  id: "fixture-event-v020",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_030,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "upstream_service",
  action: "returned",
  level: "error",
  status: 502,
  errorCode: "UPSTREAM_SERVICE_ERROR",
  attempt: 1,
};

let eventRecords: EventRecord[];
type CircuitReason = NonNullable<
  NonNullable<EventRecord["details"]>["circuit"]
>["causeReason"];

function circuitRecord({
  causeReason,
  retryAt,
  waitSeconds,
  status = 502,
}: {
  causeReason?: CircuitReason;
  retryAt?: number;
  waitSeconds?: number;
  status?: number | null;
} = {}): EventRecord {
  return {
    ...baseRecord,
    id: "fixture-circuit-event",
    firstAt: 1_760_000_030,
    lastAt: 1_760_000_030,
    reason: "circuit_open",
    action: "stopped",
    level: "error",
    status,
    errorCode: "CIRCUIT_OPEN",
    category: "circuit_error",
    details: {
      healthScope: "provider",
      waitSeconds,
      circuit: {
        causeReason,
        retryAt,
        failures: 3,
        failureThreshold: 3,
        failedRequests: 3,
        requests: 3,
        errorRate: 1,
        minRequests: 1,
        trigger: "consecutive_failures",
      },
    },
  };
}

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  eventRecords = [baseRecord];
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_gateway") {
        return {
          clientId: args.clientId,
          providers: [{ id: "provider-1", name: "Fixture Provider" }],
        };
      }
      if (name === "get_app_events") {
        return {
          items: structuredClone(eventRecords),
          total: eventRecords.length,
          page: 1,
          error: null,
        };
      }
      if (name === "get_app_event") {
        return eventRecords.find((item) => item.id === args.id) ?? null;
      }
      return undefined;
    },
  );
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});

it("normalizes a numeric upstream code in the summary and keeps local code as a fallback", async () => {
  const numericUpstream = {
    ...baseRecord,
    details: {
      upstreamCode: 429,
      localCode: "CAPACITY_LIMITED",
      message: "上游容量暂时不足",
    },
  } as unknown as EventRecord;
  eventRecords = [numericUpstream];
  const firstView = render(<EventLog />);

  const summary = (await screen.findByTitle("429 · 上游容量暂时不足")).closest(
    "button",
  )!;
  expect(summary).toHaveTextContent("429");
  expect(summary).toHaveTextContent("上游容量暂时不足");
  expect(summary).not.toHaveTextContent("CAPACITY_LIMITED");

  eventRecords = [
    {
      ...baseRecord,
      id: "fixture-local-code",
      reason: "network",
      status: null,
      details: { localCode: "CONNECTION_FAILED", message: null },
    },
  ];
  firstView.unmount();
  render(<EventLog />);
  expect(
    await screen.findByTitle("CONNECTION_FAILED · 连接失败", { exact: true }),
  ).toBeInTheDocument();
});

it("keeps HTTP status and WebSocket close code as separate detail values", async () => {
  eventRecords = [
    {
      ...baseRecord,
      reason: "circuit_open",
      errorCode: "CIRCUIT_OPEN",
      category: "circuit_error",
      details: {
        healthScope: "provider",
        upstreamCode: "overloaded_error",
        localCode: "UPSTREAM_RESPONSE_ERROR",
        message: "上游暂时不可用",
        wsCloseCode: 1006,
        waitSeconds: 45,
        circuit: {
          causeReason: "upstream_service",
          retryAt: 1_760_000_075,
          failures: 6,
          failureThreshold: 6,
          failedRequests: 6,
          requests: 6,
          errorRate: 1,
          minRequests: 1,
          trigger: "transient_failures",
        },
      },
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  await user.click(await screen.findByRole("button", { name: "供应商熔断" }));
  const dialog = await screen.findByRole("dialog", { name: "供应商熔断" });
  expect(
    within(dialog).getByText("502 · WS 1006", { exact: true }),
  ).toBeInTheDocument();
  expect(
    within(dialog).getByText("上游错误码").nextElementSibling,
  ).toHaveTextContent("overloaded_error");
  expect(
    within(dialog).getByText("网关诊断码").nextElementSibling,
  ).toHaveTextContent("UPSTREAM_RESPONSE_ERROR");
  expect(
    within(dialog).getByText("影响范围").nextElementSibling,
  ).toHaveTextContent("整个供应商");
  expect(
    within(dialog).getByText("触发条件").nextElementSibling,
  ).toHaveTextContent("连续失败 6/6");
});

it("renders the original-provider retry action in Chinese", async () => {
  eventRecords = [
    {
      ...baseRecord,
      id: "fixture-retrying-same",
      action: "retrying_same",
      details: { countedFailure: false },
    },
  ];
  render(<EventLog />);

  expect(
    await screen.findByText("未计入熔断，原供应商等待重试", { exact: true }),
  ).toBeInTheDocument();
});

it("shows recorded circuit cause and deadline evidence in the detail drawer", async () => {
  const retryAt = 1_760_000_090;
  eventRecords = [
    circuitRecord({
      causeReason: "upstream_service",
      retryAt,
      waitSeconds: 60,
    }),
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  await user.click(await screen.findByRole("button", { name: "供应商熔断" }));
  const dialog = await screen.findByRole("dialog", { name: "供应商熔断" });

  expect(within(dialog).getByText("类别").nextElementSibling).toHaveTextContent(
    "熔断错误",
  );
  expect(
    within(dialog).getByText("熔断原因").nextElementSibling,
  ).toHaveTextContent("502");
  expect(
    within(dialog).getByText("熔断原因").nextElementSibling,
  ).toHaveTextContent("上游服务异常");
  expect(
    within(dialog).getByText("触发条件").nextElementSibling,
  ).toHaveTextContent("连续失败 3/3");
  expect(
    within(dialog).getByText("发生时等待").nextElementSibling,
  ).toHaveTextContent("60 秒");

  const expectedTime = new Date(retryAt * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
  expect(
    within(dialog).getByText("预计探测时间").nextElementSibling,
  ).toHaveTextContent(expectedTime);
});

it("marks missing circuit cause and retry evidence unrecorded instead of trusting old waitSeconds", async () => {
  eventRecords = [circuitRecord({ waitSeconds: 5, status: null })];
  const user = userEvent.setup();
  render(<EventLog />);

  await user.click(await screen.findByRole("button", { name: "供应商熔断" }));
  const dialog = await screen.findByRole("dialog", { name: "供应商熔断" });

  expect(
    within(dialog).getByText("熔断原因").nextElementSibling,
  ).toHaveTextContent("原因未记录");
  expect(
    within(dialog).getByText("发生时等待").nextElementSibling,
  ).toHaveTextContent("未记录");
  expect(
    within(dialog).getByText("预计探测时间").nextElementSibling,
  ).toHaveTextContent("未记录");
  expect(dialog).not.toHaveTextContent("5 秒");
});
