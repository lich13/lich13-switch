import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen } from "@testing-library/react";
import {
  cacheHitRate,
  columnVisible,
  interruption,
  logMoney,
  logNumber,
  logSpeed,
} from "./usage-log-columns";
import type { Attempt, UsageRecord, UsageSettings } from "./usage-types";
import UsageLogTable from "./UsageLogTable";
const defaultSettings: UsageSettings = {
  recording: true,
  autoSync: false,
  refreshSeconds: 0,
  multiplier: "1",
  pricingModel: "response",
};
const tokens = {
  input: 100,
  output: 60,
  cacheRead: 50,
  cacheWrite: 0,
  inclusiveInput: 150,
  reasoning: 20,
} as Attempt["tokens"];
const attempt = {
  tokens,
  durationMs: 2500,
  firstTokenMs: 500,
  stream: true,
  usageStatus: "reported",
  provider: "fixture",
  requestedModel: "fixture-request",
  responseModel: "fixture-response",
  status: 200,
  parseReasons: [],
} as unknown as Attempt;
describe("request log columns", () => {
  it("distinguishes unknown, zero, compact boundaries and exact costs", () => {
    expect([null, 0, 9999, 10000, 1050000].map((v) => logNumber(v))).toEqual([
      "—",
      "0",
      "9,999",
      "10.0K",
      "1.1M",
    ]);
    expect(logNumber(1050000, true)).toBe("1,050,000");
    expect(logMoney("0.000000123456")).toBe("$1.235e-7");
    expect(logMoney("0.000000123456", true)).toBe("$0.000000123456");
    expect(logMoney("0")).toBe("$0");
  });
  it("uses explicit columns before narrow defaults and calculates only reported measures", () => {
    expect(columnVisible("time", undefined, true)).toBe(true);
    expect(columnVisible("speed", undefined, true)).toBe(false);
    expect(
      columnVisible(
        "speed",
        { ...defaultSettings, logColumns: { speed: true } },
        true,
      ),
    ).toBe(true);
    expect(logSpeed(attempt)).toBe(30);
    expect(logSpeed({ ...attempt, stream: false })).toBeNull();
    expect(logSpeed({ ...attempt, durationMs: 500 })).toBeNull();
    expect(logSpeed({ ...attempt, outputReported: false })).toBeNull();
    expect(
      logSpeed({
        ...attempt,
        usageStatus: "session_supplemented",
        parseReasons: ["prompt_cache_total_mismatch"],
      }),
    ).toBeNull();
    expect(cacheHitRate(tokens)).toBeCloseTo(100 / 3);
    expect(cacheHitRate({ ...tokens, inclusiveInput: 0 })).toBeNull();
    expect(
      cacheHitRate({ ...tokens, inclusiveInput: null, cacheWrite: null }),
    ).toBeNull();
    const end = {
      cause: "client_disconnected",
      responseStarted: true,
      terminalReceived: false,
    } as const;
    expect(interruption({ ...attempt, end, outputReported: false })).toBe(
      "partial",
    );
    expect(interruption({ ...attempt, end, outputReported: true })).toBe(
      "reported",
    );
    expect(
      interruption({
        ...attempt,
        end,
        outputReported: true,
        usageStatus: "session_supplemented",
        parseReasons: ["prompt_cache_total_mismatch"],
      }),
    ).toBe("partial");
  });
  it("persists only an explicit visibility change, locks time and opens details by keyboard", () => {
    const save = vi.fn(),
      open = vi.fn();
    const settings: UsageSettings = {
      ...defaultSettings,
      logColumns: {},
      logNumberMode: "compact",
    };
    render(
      <UsageLogTable
        rows={[
          {
            id: "fixture",
            client: "dsh",
            source: "proxy",
            startedAt: 1,
            attempts: [attempt],
          } as UsageRecord,
        ]}
        busy={false}
        settings={settings}
        save={save}
        name={() => "Fixture"}
        open={open}
      />,
    );
    expect(save).not.toHaveBeenCalled();
    expect(screen.getByRole("checkbox", { name: "结束时间" })).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "推理" }));
    expect(save).toHaveBeenCalledWith({
      ...settings,
      logColumns: { reasoning: true },
    });
    const row = screen.getByRole("row", { name: /请求详情/ });
    fireEvent.keyDown(row, { key: "Enter" });
    expect(open).toHaveBeenCalledOnce();
    expect(screen.getByText(/Fixture · DSH/)).toBeInTheDocument();
    expect(
      screen.getByTitle("fixture-request → fixture-response"),
    ).toBeInTheDocument();
  });
});
