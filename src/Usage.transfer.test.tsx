import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { Attempt, UsageRecord } from "./usage-types";
import { detailValue, serveUsage, usageDashboard } from "./test-fixtures";

vi.mock("./bridge", async () => (await import("./test-bridge")).bridge);

import Usage from "./Usage";

const startedAt = Date.UTC(2026, 9, 8, 2);

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-v0212-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: "fixture-response-id",
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt,
    durationMs: 5000,
    firstTokenMs: 250,
    stream: true,
    transport: "http",
    tokens: {
      input: 1000,
      output: 2000,
      cacheRead: 3000,
      cacheWrite: 4000,
    },
    serviceTier: null,
    price: null,
    transfer: {
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
      wsCloseCode: 1000,
      downstreamError: null,
      timeoutPhase: "idle",
      timeoutSeconds: 600,
      lastProgressMs: 23,
      clientRequestHash: null,
      responseHash: null,
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v0212-request",
    client: "codex",
    source: "proxy",
    startedAt,
    sessionId: null,
    attempts: [attempt()],
    completed: true,
    duplicateOf: null,
    deduplication: "",
    ...overrides,
  };
}

let records: UsageRecord[];

beforeEach(() => {
  records = [record()];
  serveUsage(
    () => records,
    () => usageDashboard(1),
  );
});

it("labels the request table time column as end time", async () => {
  render(<Usage />);
  const table = await screen.findByRole("table");
  expect(
    within(table).getByRole("columnheader", {
      name: "时间 / 供应商 / 模型",
    }),
  ).toBeInTheDocument();
});

it("shows transfer timestamps, byte counts, and timeout metadata in request details", async () => {
  const user = userEvent.setup();
  render(<Usage />);
  await user.click(
    (await screen.findByText("fixture-response")).closest("tr")!,
  );
  const dialog = await screen.findByRole("dialog", { name: "请求详情" });

  for (const label of [
    "请求进入",
    "上游发送",
    "收到首字节",
    "协议终态",
    "本地结束",
  ]) {
    expect(detailValue(dialog, label)).not.toHaveTextContent("—");
  }
  expect(detailValue(dialog, "上游读取").textContent).toMatch(/1,?024 B/);
  expect(detailValue(dialog, "交给 HTTP 层").textContent).toMatch(/2,?048 B/);
  expect(detailValue(dialog, "终态已交出")).toHaveTextContent("是");
  expect(detailValue(dialog, "上游结束")).toHaveTextContent("正常 EOF");
  expect(detailValue(dialog, "WS 关闭码")).toHaveTextContent("1000");
  expect(detailValue(dialog, "超时阶段")).toHaveTextContent("idle");
  expect(detailValue(dialog, "超时阈值")).toHaveTextContent("600 秒");
  expect(detailValue(dialog, "距最后进展")).toHaveTextContent("23 ms");
});
