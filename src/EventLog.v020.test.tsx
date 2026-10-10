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

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  eventRecords = [baseRecord];
  mock.command.mockImplementation(async (name: string, args: Record<string, unknown> = {}) => {
    if (name === "get_gateway") {
      return {
        clientId: args.clientId,
        providers: [{ id: "provider-1", name: "Fixture Provider" }],
      };
    }
    if (name === "get_app_events") {
      return { items: structuredClone(eventRecords), total: eventRecords.length, page: 1, error: null };
    }
    if (name === "get_app_event") {
      return eventRecords.find((item) => item.id === args.id) ?? null;
    }
    return undefined;
  });
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

  const summary = (await screen.findByText("429 · 上游容量暂时不足")).closest("button")!;
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
    await screen.findByText("CONNECTION_FAILED · 连接失败", { exact: true }),
  ).toBeInTheDocument();
});

it("keeps HTTP status and WebSocket close code as separate detail values", async () => {
  eventRecords = [
    {
      ...baseRecord,
      details: {
        upstreamCode: "overloaded_error",
        localCode: "UPSTREAM_RESPONSE_ERROR",
        message: "上游暂时不可用",
        wsCloseCode: 1006,
        circuit: {
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

  await user.click(
    (await screen.findByText("overloaded_error · 上游暂时不可用")).closest("button")!,
  );
  const dialog = await screen.findByRole("dialog", { name: "上游服务异常" });
  expect(within(dialog).getByText("HTTP 502 · WS 1006", { exact: true })).toBeInTheDocument();
  expect(within(dialog).getByText("上游错误码").nextElementSibling).toHaveTextContent("overloaded_error");
  expect(within(dialog).getByText("本地分类").nextElementSibling).toHaveTextContent("UPSTREAM_RESPONSE_ERROR");
  expect(within(dialog).getByText("触发条件").nextElementSibling).toHaveTextContent("5xx 连续失败达到阈值");
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

  expect(await screen.findByText("未计入熔断，原供应商等待重试", { exact: true })).toBeInTheDocument();
});
