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
  id: "fixture-event-v0257",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_030,
  count: 1,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "rate_limit",
  action: "retrying_same",
  level: "warning",
  status: 429,
  errorCode: "RATE_LIMITED",
  attempt: 1,
};

let eventRecords: EventRecord[];

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

it("keeps the upstream code and shows the daily quota reason in summary and detail", async () => {
  eventRecords = [
    {
      ...baseRecord,
      details: {
        upstreamCode: "429",
        upstreamReason: "DAILY_LIMIT_EXCEEDED",
        message: "日额度已用尽",
      },
    },
  ];
  const user = userEvent.setup();
  render(<EventLog />);

  const summary = await screen.findByRole("button", { name: /日额度已用尽/ });
  expect(summary).toHaveTextContent(
    "日额度已用尽 · 429 · DAILY_LIMIT_EXCEEDED",
  );
  await user.click(summary);

  const dialog = await screen.findByRole("dialog", { name: "日额度已用尽" });
  expect(
    within(dialog).getByText("上游错误码").nextElementSibling,
  ).toHaveTextContent("429");
  expect(
    within(dialog).getByText("上游原因").nextElementSibling,
  ).toHaveTextContent("DAILY_LIMIT_EXCEEDED");
});

it("keeps temporary rate-limit retries distinct from daily quota exhaustion", async () => {
  eventRecords = [
    {
      ...baseRecord,
      id: "fixture-temporary-limit",
      details: {
        upstreamCode: "429",
        upstreamReason: "rate_limit_exceeded",
        message: "上游限流",
        waitSeconds: 9,
        countedFailure: false,
      },
    },
  ];
  render(<EventLog />);

  const summary = await screen.findByRole("button", { name: /上游限流/ });
  expect(summary).toHaveTextContent("上游限流 · 429 · rate_limit_exceeded");
  expect(screen.getByText(/原供应商等待 9 秒/)).toBeInTheDocument();
});

it("reads old event records without an upstream reason field", async () => {
  render(<EventLog />);
  const summary = await screen.findByRole("button", { name: /上游限流/ });
  const user = userEvent.setup();
  await user.click(summary);

  const dialog = await screen.findByRole("dialog", { name: "上游限流" });
  expect(
    within(dialog).getByText("上游原因").nextElementSibling,
  ).toHaveTextContent("未提供");
});
