import { render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import Detail from "./UsageDetail";
import type { Attempt, UsageRecord } from "./usage-types";

const startedAt = Date.UTC(2026, 9, 21, 2);

function attempt(overrides: Partial<Attempt> = {}): Attempt {
  return {
    id: "fixture-v02513-detail-attempt",
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
    ...overrides,
  };
}

function record(overrides: Partial<Attempt> = {}): UsageRecord {
  return {
    id: "fixture-v02513-detail",
    client: "codex",
    source: "proxy",
    startedAt,
    finishedAt: startedAt + 100,
    sessionId: null,
    attempts: [attempt(overrides)],
    completed: true,
    estimatedSpeed: false,
    duplicateOf: null,
    deduplication: "",
  };
}

const knownOutcomes = [
  ["window_consumed", "真实生成已交付，旧窗口失效"],
  ["window_released", "未输出，已释放并保存压缩机会"],
  ["boundary_registered", "新压缩窗口已保存"],
  ["ownership_saved", "归属已保存"],
  ["save_failed", "归属保存失败"],
] as const;

it.each(knownOutcomes)(
  "renders the routing outcome %s without changing the request status",
  (outcome, label) => {
    render(
      <Detail
        record={record({ routingOutcome: outcome })}
        name={(id) => id ?? ""}
        onClose={vi.fn()}
      />,
    );

    expect(screen.getByText(label)).toBeInTheDocument();
    expect(screen.getByText("HTTP 200")).toBeInTheDocument();
  },
);

it("falls back for an unknown routing outcome", () => {
  render(
    <Detail
      record={record({ routingOutcome: "future_routing_outcome" })}
      name={(id) => id ?? ""}
      onClose={vi.fn()}
    />,
  );

  expect(screen.getByText("路由结果未确认")).toBeInTheDocument();
});

it("does not infer a routing outcome when the field is absent", () => {
  render(
    <Detail
      record={
        record({
          routingDecision: "compaction_handoff",
          routingEvidence: {
            input: "compaction",
            hasToolDeclarations: false,
            boundary: "fresh",
          },
        })
      }
      name={(id) => id ?? ""}
      onClose={vi.fn()}
    />,
  );

  expect(screen.queryByText("最终路由结果", { selector: "dt" })).not.toBeInTheDocument();
  expect(screen.queryByText("新压缩窗口已保存")).not.toBeInTheDocument();
});

it("keeps the initial boundary evidence beside the final routing outcome", () => {
  render(
    <Detail
      record={
        record({
          routingDecision: "compaction_handoff",
          routingEvidence: {
            input: "compaction",
            hasToolDeclarations: false,
            boundary: "fresh",
          },
          routingOutcome: "boundary_registered",
        })
      }
      name={(id) => id ?? ""}
      onClose={vi.fn()}
    />,
  );

  expect(screen.getByText("边界状态")).toBeInTheDocument();
  expect(screen.getByText("新鲜边界")).toBeInTheDocument();
  expect(screen.getByText("最终路由结果")).toBeInTheDocument();
  expect(screen.getByText("新压缩窗口已保存")).toBeInTheDocument();
});
