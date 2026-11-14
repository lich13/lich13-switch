import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { Attempt, UsageRecord } from "./usage-types";
import { serveUsage, usageDashboard } from "./test-fixtures";

vi.mock("./bridge", async () => (await import("./test-bridge")).bridge);

import Usage from "./Usage";

const timestamp = Date.UTC(2026, 9, 8, 2);

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-v020-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-request",
    responseModel: "fixture-response",
    pricingModel: "fixture-priced",
    responseId: null,
    pricingBasis: "response",
    status: 200,
    outcome: "success",
    startedAt: timestamp,
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
    price: {
      version: "fixture-price-version",
      source: "fixture",
      model: "fixture-priced",
      multiplier: "1",
      rates: {},
      cost: "1.0000",
    },
    ...overrides,
  };
}

function record(overrides: Partial<UsageRecord> = {}): UsageRecord {
  return {
    id: "fixture-v020-request",
    client: "codex",
    source: "proxy",
    startedAt: timestamp,
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
    () => usageDashboard(1, { cost: "1.0000", unpriced: 0 }),
  );
});

it("renders null table quantities as dashes while keeping K/M compact values", async () => {
  records = [
    record({
      id: "fixture-null-table",
      attempts: [
        attempt({
          requestedModel: null,
          responseModel: null,
          tokens: {
            input: null,
            output: null,
            cacheRead: null,
            cacheWrite: null,
          },
        }),
      ],
    }),
    record({
      id: "fixture-compact-table",
      attempts: [
        attempt({
          tokens: {
            input: 1234,
            output: 1234567,
            cacheRead: 1234567890,
            cacheWrite: null,
          },
        }),
      ],
    }),
  ];
  render(<Usage />);

  const compactRow = (await screen.findByText("1.2M")).closest("tr")!;
  expect(within(compactRow).getByText("1.2M")).toBeInTheDocument();
  expect(within(compactRow).getByText("1234.6M")).toBeInTheDocument();
  expect(within(compactRow).getByText("1,234")).toBeInTheDocument();
  const missingRow = within(screen.getByRole("table")).getAllByRole("row")[1];
  expect(
    missingRow.querySelector('[data-field="identity"] .usage-identity-model'),
  ).toHaveTextContent("—");
  const missingTokens = missingRow.querySelector('[data-field="tokens"]')!;
  expect(within(missingTokens as HTMLElement).getByLabelText("输入")).toHaveTextContent(/^—$/);
  expect(within(missingTokens as HTMLElement).getByLabelText("输出")).toHaveTextContent(/^—$/);
});

it("shows reasoned missing token fields and preserves a real zero duration", async () => {
  records = [
    record({
      id: "fixture-missing-detail",
      attempts: [
        attempt({
          tokens: {
            input: null,
            output: null,
            cacheRead: null,
            cacheWrite: null,
          },
          durationMs: 0,
          firstTokenMs: null,
          stream: false,
          availability: {
            input: "upstream_unreported",
            output: "ended_early",
            cache_read: "parse_incomplete",
            cache_write: "historical_missing",
            first_token: "not_applicable",
          },
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);

  await user.click(
    (await screen.findByText("fixture-response")).closest("tr")!,
  );
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  expect(within(drawer).getAllByLabelText("— · 上游未报告")).toHaveLength(1);
  expect(within(drawer).getAllByLabelText("— · 提前结束")).toHaveLength(1);
  expect(within(drawer).getAllByLabelText("— · 解析不完整")).toHaveLength(1);
  expect(within(drawer).getAllByLabelText("— · 历史缺失")).toHaveLength(2);
  for (const label of within(drawer).getAllByText("总耗时", {
    selector: "dt",
  })) {
    expect(label.nextElementSibling).toHaveTextContent("0.000s");
  }
  for (const label of within(drawer).getAllByText("首字", { selector: "dt" })) {
    const firstToken = label.nextElementSibling!;
    expect(firstToken).toHaveTextContent("—");
    expect(firstToken.querySelector("span")).toHaveAttribute("title", "不适用");
  }
});

it("marks a session first-token value as not applicable", async () => {
  records = [
    record({
      id: "fixture-session-detail",
      source: "codex",
      attempts: [
        attempt({
          responseModel: "fixture-session-model",
          firstTokenMs: null,
          availability: { first_token: "not_applicable" },
        }),
      ],
    }),
  ];
  const user = userEvent.setup();
  render(<Usage />);

  await user.click(
    (await screen.findByText("fixture-session-model")).closest("tr")!,
  );
  const drawer = await screen.findByRole("dialog", { name: "请求详情" });
  for (const label of within(drawer).getAllByText("首字", { selector: "dt" })) {
    const firstToken = label.nextElementSibling!;
    expect(firstToken).toHaveTextContent("—");
    expect(firstToken.querySelector("span")).toHaveAttribute("title", "不适用");
  }
});
