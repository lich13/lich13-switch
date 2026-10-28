import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
import type { UsageRecord, UsageSettings, UsageState } from "./usage-types";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import UsageLogTable from "./UsageLogTable";
import Detail from "./UsageDetail";
import Sources from "./UsageSources";

const settings: UsageSettings = {
  recording: true,
  autoSync: false,
  refreshSeconds: 0,
  multiplier: "1",
  pricingModel: "response",
};
function record(): UsageRecord {
  return {
    id: "fixture-request",
    client: "antigravity",
    source: "antigravity",
    startedAt: 1767225600000,
    finishedAt: 1767225602000,
    completed: true,
    estimatedSpeed: false,
    sessionId: null,
    duplicateOf: null,
    deduplication: "",
    attempts: [
      {
        id: "fixture-attempt",
        provider: null,
        requestedModel: null,
        responseModel: "fixture-response",
        modelDisplayName: "Fixture Model (High)",
        apiProvider: "API_PROVIDER_FIXTURE",
        pricingModel: "fixture-response",
        responseId: null,
        status: null,
        outcome: "success",
        startedAt: 1767225600000,
        durationMs: 2000,
        firstTokenMs: 250,
        generationMs: 1750,
        stream: true,
        transport: "local_api",
        tokens: {
          input: 100,
          output: 20,
          cacheRead: 40,
          cacheWrite: null,
          reasoning: 12,
        },
        price: null,
        serviceTier: null,
        availability: { duration: "reported", first_token: "reported" },
      },
    ],
  };
}
it("shows the display model and measured app performance without a fabricated HTTP status", () => {
  render(
    <UsageLogTable
      rows={[record()]}
      settings={settings}
      name={() => "本机会话"}
      open={vi.fn()}
      busy={false}
    />,
  );
  const table = screen.getByRole("table", { name: "请求记录" });
  expect(within(table).getByText("Fixture Model (High)")).toBeInTheDocument();
  expect(within(table).getByText("0.25s")).toBeInTheDocument();
  expect(within(table).getByText("2.00s")).toBeInTheDocument();
  expect(within(table).getByText("10.0")).toBeInTheDocument();
  expect(within(table).getByText("未定价")).toBeInTheDocument();
  expect(within(table).queryByText("HTTP 200")).not.toBeInTheDocument();
});
it("leaves missing time and speed empty even in legacy app rows", () => {
  const r = record();
  const a = r.attempts[0];
  a.durationMs = 0;
  a.firstTokenMs = null;
  a.generationMs = null;
  a.availability = {};
  a.parseReasons = ["attempt_time_unreported"];
  render(
    <UsageLogTable
      rows={[r]}
      settings={settings}
      name={() => "本机会话"}
      open={vi.fn()}
      busy={false}
    />,
  );
  expect(screen.queryByText("0.00s")).not.toBeInTheDocument();
  expect(screen.queryByText("0.0")).not.toBeInTheDocument();
});
it("shows real response identity, service, request outcome and separate generation speed in details", () => {
  const r = record();
  r.attempts[0].outcome = "failed";
  r.completed = false;
  render(<Detail record={r} name={() => "本机会话"} onClose={vi.fn()} />);
  expect(screen.getByText("请求失败")).toBeInTheDocument();
  expect(screen.getByText("API_PROVIDER_FIXTURE")).toBeInTheDocument();
  expect(screen.getByText("0.250s")).toBeInTheDocument();
  expect(screen.getByText("2.000s")).toBeInTheDocument();
  expect(screen.getByText("1.750s")).toBeInTheDocument();
  expect(screen.getByText("10.0 tok/s")).toBeInTheDocument();
  expect(screen.getByText("11.4 tok/s")).toBeInTheDocument();
});
it("runs explicit app history repair through the existing synchronization command", async () => {
  const state = {
    settings,
    syncing: false,
    reports: {},
    error: null,
  } as UsageState;
  mock.command.mockResolvedValue(state);
  const changed = vi.fn();
  render(
    <Sources
      state={state}
      counts={{}}
      historyIncomplete={false}
      onClose={vi.fn()}
      onChange={changed}
    />,
  );
  await userEvent.click(
    screen.getByRole("button", { name: "补全 Antigravity 历史" }),
  );
  expect(mock.command).toHaveBeenCalledWith("sync_usage", {
    rebuild: "antigravity",
  });
  expect(changed).toHaveBeenCalledWith(state);
});
