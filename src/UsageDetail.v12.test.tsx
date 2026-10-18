import { render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import type { Attempt, UsageRecord } from "./usage-types";
import Detail from "./UsageDetail";

const startedAt = Date.UTC(2026, 9, 18, 2);

function attempt(): Attempt {
  return {
    id: "fixture-v12-detail-attempt",
    provider: "fixture-provider",
    requestedModel: "fixture-model",
    responseModel: "fixture-model",
    pricingModel: "fixture-model",
    responseId: "fixture-response",
    status: 200,
    outcome: "success",
    startedAt,
    durationMs: 100,
    firstTokenMs: 10,
    stream: true,
    transport: "http",
    tokens: { input: 10, output: 2, cacheRead: null, cacheWrite: null },
    serviceTier: null,
    price: null,
  };
}

function record(detailArchived?: boolean): UsageRecord {
  return {
    ...(detailArchived === undefined ? {} : { detailArchived }),
    id: "fixture-v12-detail",
    client: "codex",
    source: "proxy",
    startedAt,
    finishedAt: startedAt + 100,
    sessionId: null,
    attempts: [attempt()],
    completed: true,
    estimatedSpeed: false,
    duplicateOf: null,
    deduplication: "",
  };
}

it("shows the archived marker for a compacted usage detail", () => {
  render(<Detail record={record(true)} name={(id) => id ?? ""} onClose={vi.fn()} />);

  expect(screen.getByText("细节已归档")).toBeInTheDocument();
});

it("keeps legacy details unmarked when the archive field is absent", () => {
  render(<Detail record={record()} name={(id) => id ?? ""} onClose={vi.fn()} />);

  expect(screen.queryByText("细节已归档")).not.toBeInTheDocument();
});
