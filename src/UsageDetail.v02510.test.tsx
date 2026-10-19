import { render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import Detail from "./UsageDetail";
import type { UsageRecord } from "./usage-types";

function record(): UsageRecord {
  return {
    id: "fixture-delivery",
    client: "codex",
    source: "proxy",
    startedAt: 1000,
    sessionId: null,
    completed: true,
    estimatedSpeed: false,
    duplicateOf: null,
    deduplication: "",
    attempts: [
      {
        id: "fixture-attempt",
        provider: "fixture-provider",
        requestedModel: "fixture-model",
        responseModel: "fixture-model",
        pricingModel: "fixture-model",
        responseId: "fixture-response",
        status: 200,
        outcome: "success",
        startedAt: 1000,
        durationMs: 100,
        firstTokenMs: 10,
        stream: true,
        transport: "http",
        tokens: { input: 10, output: 2, cacheRead: null, cacheWrite: null },
        serviceTier: null,
        price: null,
        end: {
          cause: "client_disconnected",
          terminalReceived: true,
          responseStarted: true,
        },
        transfer: {
          sentAt: 1000,
          firstByteAt: 1010,
          terminalAt: 1100,
          endedAt: 1100,
          upstreamBytes: 88,
          downstreamBytes: 17,
          terminalHandedOff: false,
          upstreamEof: false,
          downstreamError: "DOWNSTREAM_PEER_RESET",
          timeoutPhase: null,
          timeoutSeconds: null,
          lastProgressMs: 0,
          clientRequestHash: null,
          responseHash: null,
        },
      },
    ],
  };
}

it("keeps the real terminal and failed delivery as independent facts", () => {
  render(
    <Detail record={record()} name={(id) => id ?? ""} onClose={vi.fn()} />,
  );
  expect(screen.getByText("上游已结束，下游未完成交付")).toBeInTheDocument();
  expect(screen.getByText("DOWNSTREAM_PEER_RESET")).toBeInTheDocument();
  expect(screen.getByText("HTTP 200")).toBeInTheDocument();
});

it("does not invent failed delivery when a legacy record has no transfer evidence", () => {
  const value = record();
  delete value.attempts[0].transfer;
  render(<Detail record={value} name={(id) => id ?? ""} onClose={vi.fn()} />);
  expect(
    screen.queryByText("上游已结束，下游未完成交付"),
  ).not.toBeInTheDocument();
  expect(screen.getByText("上游已完成，客户端随后断开")).toBeInTheDocument();
});
