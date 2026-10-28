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
  // v0.25.10 persisted expected-close status records remain readable after
  // v0.25.11 stops creating new records for those transport outcomes.
  id: "fixture-v02510-legacy-delivery",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_001,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "request_interrupted",
  errorCode: "REQUEST_INTERRUPTED",
  action: "not_retried",
  level: "info",
  status: 200,
  attempt: null,
  details: {
    healthScope: "request",
    localCode: "DOWNSTREAM_CLOSED",
    message: "下游连接关闭，未确认客户端主动取消",
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
let rawResponse: {
  headers: { name: string; value: string; base64: boolean }[];
  event: string | null;
  body: string;
  base64: boolean;
  incomplete: string | null;
} | null;

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  eventRecords = [structuredClone(baseRecord)];
  rawResponse = null;
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
      if (name === "get_app_event_raw") return rawResponse;
      return undefined;
    },
  );
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});

it("loads and copies the complete error, and opens policy by event ID without the shortened preview", async () => {
  const user = userEvent.setup();
  const write = vi.spyOn(navigator.clipboard, "writeText");
  const openPolicy = vi.fn();
  const record: EventRecord = {
    ...structuredClone(baseRecord),
    reason: "capacity",
    level: "warning",
    details: {
      message: "short preview",
      errorBasis: "capacity",
      rawError: {
        id: "fixture-raw",
        digest: "fixture-digest",
        incomplete: null,
      },
    },
  };
  eventRecords = [record];
  rawResponse = {
    headers: [{ name: "x-fixture", value: "complete header", base64: false }],
    event: "error",
    body: `event: error\r\ndata: ${"fixture ".repeat(1000)} 'quoted fixture' selected model is at capacity. please try a different model.\r\n\r\n`,
    base64: false,
    incomplete: null,
  };
  render(<EventLog onOpenPolicy={openPolicy} />);
  const errorButton = await screen.findByRole("button", {
    name: "临时容量限制",
  });
  expect(
    within(errorButton.closest("tr")!)
      .getByText("200")
      .closest(".event-status"),
  ).not.toHaveClass("success");
  await user.click(errorButton);
  const dialog = await screen.findByRole("dialog");
  await user.click(
    await within(dialog).findByRole("button", { name: "复制完整错误" }),
  );
  expect(write).toHaveBeenCalledWith(
    `x-fixture: complete header\n\n${rawResponse.body}`,
  );
  expect(mock.command).toHaveBeenCalledWith("get_app_event_raw", {
    id: record.id,
  });
  await user.click(within(dialog).getByRole("button", { name: "查看策略" }));
  expect(openPolicy).toHaveBeenCalledWith(
    expect.objectContaining({
      clientId: "codex",
      recordedCategory: "capacity",
      input: expect.objectContaining({
        eventId: record.id,
        status: 200,
        message: undefined,
        code: undefined,
      }),
    }),
  );
});

it("renders historical v0.25.10 downstream status records with HTTP and byte evidence", async () => {
  const user = userEvent.setup();
  render(<EventLog />);

  const row = await screen.findByRole("button", {
    name: "请求交付提前结束",
  });
  expect(row).not.toHaveTextContent("状态记录");
  expect(row).toHaveTextContent("DOWNSTREAM_CLOSED");
  expect(row).toHaveTextContent("请求交付提前结束");
  expect(row).not.toHaveTextContent("客户端已取消");

  await user.click(row);
  const dialog = await screen.findByRole("dialog", {
    name: "请求交付提前结束",
  });
  expect(within(dialog).getByText("类别").nextElementSibling).toHaveTextContent(
    "状态记录",
  );
  expect(
    within(dialog).getByText("HTTP 状态").nextElementSibling,
  ).toHaveTextContent("200");
  expect(
    within(dialog).getByText("网关诊断码").nextElementSibling,
  ).toHaveTextContent("DOWNSTREAM_CLOSED");
  expect(
    within(dialog).getByText("错误摘要").nextElementSibling,
  ).toHaveTextContent("下游连接关闭，未确认客户端主动取消");
  expect(
    within(dialog).getByText("上游已读取").nextElementSibling,
  ).toHaveTextContent("31 字节");
  expect(
    within(dialog).getByText("交给传输层").nextElementSibling,
  ).toHaveTextContent("11 字节（不代表客户端已收到）");
  expect(
    within(dialog).getByText("已观察上游终态").nextElementSibling,
  ).toHaveTextContent("否");
});

it("uses a distinct terminal-undelivered reason and de-duplicates equal code and reason", async () => {
  eventRecords = [
    {
      ...structuredClone(baseRecord),
      id: "fixture-v02510-terminal-undelivered",
      details: {
        healthScope: "request",
        upstreamCode: "DOWNSTREAM_WRITE_FAILED",
        upstreamReason: "DOWNSTREAM_WRITE_FAILED",
        localCode: "DOWNSTREAM_WRITE_FAILED",
        message: null,
        countedFailure: false,
        delivery: {
          durationMs: 24,
          upstreamBytes: 88,
          handedBytes: 17,
          terminalReceived: true,
          terminalHandedOff: false,
          requestHash: null,
          responseHash: null,
        },
      },
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  const row = await screen.findByRole("button", {
    name: "上游已结束，下游未完成交付",
  });
  const summary = row.textContent ?? "";
  expect(summary.match(/DOWNSTREAM_WRITE_FAILED/g)).toHaveLength(1);
  expect(summary.match(/上游已结束，下游未完成交付/g)).toHaveLength(1);

  await user.click(row);
  const dialog = await screen.findByRole("dialog", {
    name: "上游已结束，下游未完成交付",
  });
  expect(within(dialog).getByText("类别").nextElementSibling).toHaveTextContent(
    "状态记录",
  );
  expect(
    within(dialog).getByText("错误摘要").nextElementSibling,
  ).toHaveTextContent("上游已结束，下游未完成交付");
  expect(
    within(dialog).getByText("已观察上游终态").nextElementSibling,
  ).toHaveTextContent("是");
  expect(
    within(dialog).getByText("终态已交给传输层").nextElementSibling,
  ).toHaveTextContent("否");
});

it("keeps legacy interruption records readable without delivery details", async () => {
  eventRecords = [
    {
      id: "fixture-v02510-legacy",
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
  expect(dialog).not.toHaveTextContent("上游已结束，下游未完成交付");
  expect(dialog).not.toHaveTextContent("交给传输层");
});

it("shows fresh exhausted evidence as status and does not retain the capacity countdown", async () => {
  eventRecords = [
    {
      ...structuredClone(baseRecord),
      reason: "rate_limit",
      action: "trying_next",
      status: 429,
      details: {
        errorBasis: "quota_exhausted",
        upstreamCode: "USAGE_LIMIT_EXCEEDED",
        upstreamReason: "USAGE_LIMIT_EXCEEDED",
        healthScope: "request",
        countedFailure: false,
        quotaCheck: {
          verdict: "exhausted",
          source: "new_api",
          basis: "key_quota",
          checkedAt: 1_760_000_001,
        },
      },
    },
  ];
  render(<EventLog />);
  const row = await screen.findByRole("button", { name: "额度已耗尽" });
  expect(row).not.toHaveTextContent("状态记录");
  expect(row).toHaveTextContent("额度已耗尽");
  expect(row).not.toHaveTextContent("USAGE_LIMIT_EXCEEDED");
  await userEvent.setup().click(row);
  const dialog = await screen.findByRole("dialog", { name: "额度已耗尽" });
  expect(dialog).toHaveTextContent("当前 Key 配额");
  expect(dialog).toHaveTextContent("new-api");
  expect(dialog).toHaveTextContent("尝试下一供应商");
  expect(dialog).not.toHaveTextContent("等待 45 秒");
});

it("falls back for unknown limit and quota enums", async () => {
  eventRecords = [
    {
      ...structuredClone(baseRecord),
      reason: "rate_limit",
      details: {
        errorBasis: "future_basis",
        rpmWaitReason: "future_wait",
        quotaCheck: {
          verdict: "future_verdict",
          source: "future_source",
          basis: "future_basis",
          checkedAt: 1_760_000_001,
        },
      },
    },
  ];
  render(<EventLog />);
  await userEvent
    .setup()
    .click(await screen.findByRole("button", { name: "上游限流" }));
  const dialog = await screen.findByRole("dialog", { name: "上游限流" });
  expect(dialog).toHaveTextContent("未识别分类");
  expect(dialog).toHaveTextContent("未识别来源");
  expect(dialog).toHaveTextContent("未识别依据");
  expect(dialog).toHaveTextContent("额度查询结果未知");
});

it("shows actual failover separately from the rejected supplier cooldown", async () => {
  eventRecords = [
    {
      ...structuredClone(baseRecord),
      reason: "upstream_service",
      action: "trying_next",
      status: 429,
      details: {
        errorBasis: "billing",
        healthScope: "provider",
        countedFailure: false,
        waitSeconds: 300,
      },
    },
  ];
  render(<EventLog />);
  const row = await screen.findByRole("button", { name: "余额或消费额度不足" });
  await userEvent.setup().click(row);
  const dialog = await screen.findByRole("dialog", {
    name: "余额或消费额度不足",
  });
  expect(dialog).toHaveTextContent("尝试下一供应商");
  expect(dialog).toHaveTextContent("当前供应商冷却 300 秒");
  expect(dialog).not.toHaveTextContent("等待 300 秒重试");
});
