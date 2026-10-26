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
  id: "fixture-v02512-event",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_001,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "failover_exhausted",
  errorCode: "FAILOVER_EXHAUSTED",
  action: "returned",
  level: "error",
  status: 502,
  attempt: 1,
};

let eventRecords: EventRecord[];

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  eventRecords = [structuredClone(baseRecord)];
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_gateway") {
        return {
          clientId: args.clientId,
          providers: [{ id: "provider-1", name: "Fixture Provider" }],
        };
      }
      if (name === "get_app_events") {
        const reason = (args.filter as { reason?: string } | undefined)?.reason;
        const items = eventRecords.filter(
          (record) => !reason || record.reason === reason,
        );
        return {
          items: structuredClone(items),
          total: items.length,
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

it("keeps legacy failover_exhausted events readable and filterable under the generic title", async () => {
  eventRecords = [
    structuredClone(baseRecord),
    {
      ...structuredClone(baseRecord),
      id: "fixture-v02512-network",
      reason: "network",
      errorCode: "NETWORK_ERROR",
      action: "returned",
      level: "warning",
      status: null,
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  const reasonFilter = screen.getByRole("combobox", { name: "原因" });
  expect(
    within(reasonFilter).getByRole("option", { name: "请求未完成" }),
  ).toHaveValue("failover_exhausted");

  await user.selectOptions(reasonFilter, "failover_exhausted");
  const row = await screen.findByRole("button", { name: "请求未完成" });
  expect(row).toHaveTextContent("FAILOVER_EXHAUSTED");
  expect(row).toHaveTextContent("请求未完成");
  expect(
    screen.queryByRole("button", { name: "连接失败" }),
  ).not.toBeInTheDocument();

  await user.click(row);
  const dialog = await screen.findByRole("dialog", { name: "请求未完成" });
  expect(
    within(dialog).getByText("错误码").nextElementSibling,
  ).toHaveTextContent("FAILOVER_EXHAUSTED");
  expect(
    within(dialog).getByText("错误摘要").nextElementSibling,
  ).toHaveTextContent("请求未完成");
});

it("uses the existing fallback labels for unknown diagnostic enum values", async () => {
  eventRecords = [
    {
      ...structuredClone(baseRecord),
      reason: "rate_limit",
      errorCode: "RATE_LIMITED",
      action: "returned",
      level: "warning",
      status: 429,
      details: {
        errorBasis: "future_basis",
        rpmWaitReason: "future_wait",
        phase: "future_phase",
        quotaCheck: {
          verdict: "future_verdict",
          source: "future_source",
          basis: "future_basis",
          checkedAt: 1_760_000_001,
        },
      },
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  await user.click(await screen.findByRole("button", { name: "上游限流" }));
  const dialog = await screen.findByRole("dialog", { name: "上游限流" });
  expect(
    within(dialog).getByText("分类依据").nextElementSibling,
  ).toHaveTextContent("未识别分类");
  expect(
    within(dialog).getByText("RPM 等待原因").nextElementSibling,
  ).toHaveTextContent("RPM 已满");
  expect(
    within(dialog).getByText("额度核验").nextElementSibling,
  ).toHaveTextContent("额度查询结果未知");
  expect(
    within(dialog).getByText("查询来源").nextElementSibling,
  ).toHaveTextContent("未识别来源");
  expect(
    within(dialog).getByText("查询依据").nextElementSibling,
  ).toHaveTextContent("未识别依据");
  expect(within(dialog).getByText("阶段").nextElementSibling).toHaveTextContent(
    "未提供",
  );
});
