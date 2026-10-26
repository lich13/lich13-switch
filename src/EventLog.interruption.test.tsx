import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { EventRecord } from "./EventLog";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
  confirm: vi.fn(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));
vi.mock("./confirmation", () => ({ confirmAction: mock.confirm }));

import EventLog from "./EventLog";

const resetRecord: EventRecord = {
  id: "fixture-request-reset",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_001,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "request_interrupted",
  errorCode: "REQUEST_INTERRUPTED",
  action: "not_retried",
  level: "warning",
  status: 200,
  attempt: null,
  details: {
    healthScope: "request",
    localCode: "DOWNSTREAM_PEER_RESET",
    message: "下游连接被重置，未确认客户端主动取消",
    countedFailure: false,
    delivery: {
      durationMs: 24,
      upstreamBytes: 31,
      handedBytes: 11,
      terminalReceived: false,
      terminalHandedOff: false,
      requestHash: null,
      responseHash: null,
    },
  },
};

let eventRecords: EventRecord[];

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(true);
  eventRecords = [structuredClone(resetRecord)];
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

it("shows reset evidence as an ordinary interruption without claiming cancellation", async () => {
  const user = userEvent.setup();
  render(<EventLog />);

  const row = await screen.findByRole("button", {
    name: "请求交付提前结束",
  });
  expect(row).not.toHaveTextContent("普通错误");
  expect(row).toHaveTextContent("DOWNSTREAM_PEER_RESET");
  expect(row).toHaveTextContent("未确认客户端主动取消");
  expect(row).not.toHaveTextContent("熔断错误");

  await user.click(row);
  const dialog = await screen.findByRole("dialog", {
    name: "请求交付提前结束",
  });
  expect(within(dialog).getByText("类别").nextElementSibling).toHaveTextContent(
    "普通错误",
  );
  expect(
    within(dialog).getByText("网关诊断码").nextElementSibling,
  ).toHaveTextContent("DOWNSTREAM_PEER_RESET");
  expect(
    within(dialog).getByText("错误摘要").nextElementSibling,
  ).toHaveTextContent("下游连接被重置，未确认客户端主动取消");
  expect(
    within(dialog).getByText("处理结果").nextElementSibling,
  ).toHaveTextContent("响应已开始，未重试");
  expect(
    within(dialog).getByText("交给传输层").nextElementSibling,
  ).toHaveTextContent("11 字节（不代表客户端已收到）");
  expect(
    within(dialog).getByText("已观察上游终态").nextElementSibling,
  ).toHaveTextContent("否");
  expect(
    within(dialog).getByText("终态已交给传输层").nextElementSibling,
  ).toHaveTextContent("否");
  expect(dialog).not.toHaveTextContent("客户端已取消");
});

it("renders an explicit cancellation as a status record", async () => {
  eventRecords = [
    {
      ...structuredClone(resetRecord),
      id: "fixture-explicit-cancel",
      action: "returned",
      level: "info",
      details: {
        healthScope: "request",
        localCode: "CANCELLED",
        message: "收到明确取消指令，已结束转发",
        countedFailure: false,
      },
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  const row = await screen.findByRole("button", {
    name: "请求交付提前结束",
  });
  expect(row).not.toHaveTextContent("状态记录");
  await user.click(row);

  const dialog = await screen.findByRole("dialog", {
    name: "请求交付提前结束",
  });
  expect(within(dialog).getByText("类别").nextElementSibling).toHaveTextContent(
    "状态记录",
  );
  expect(
    within(dialog).getByText("网关诊断码").nextElementSibling,
  ).toHaveTextContent("CANCELLED");
  expect(
    within(dialog).getByText("错误摘要").nextElementSibling,
  ).toHaveTextContent("收到明确取消指令，已结束转发");
});

it("keeps legacy interruption records readable when delivery details are absent", async () => {
  eventRecords = [
    {
      id: "fixture-legacy-interruption",
      firstAt: 1_760_000_000,
      lastAt: 1_760_000_000,
      count: 1,
      clientId: "codex",
      providerId: "provider-1",
      model: "fixture-model",
      reason: "request_interrupted",
      errorCode: "REQUEST_INTERRUPTED",
      action: "returned",
      level: "warning",
      status: 200,
      attempt: null,
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  const row = await screen.findByRole("button", {
    name: "请求交付提前结束",
  });
  expect(row).not.toHaveTextContent("普通错误");
  await user.click(row);

  const dialog = await screen.findByRole("dialog", {
    name: "请求交付提前结束",
  });
  expect(dialog).toHaveTextContent("历史未记录");
  expect(dialog).not.toHaveTextContent("请求持续");
  expect(dialog).not.toHaveTextContent("终态已交给传输层");
});
