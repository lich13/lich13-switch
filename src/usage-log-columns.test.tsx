import { describe, expect, it, vi } from "vitest";
import { fireEvent, render, screen, within } from "@testing-library/react";
import {
  cacheHitRate,
  columnVisible,
  interruption,
  logMoney,
  logNumber,
  logSpeed,
  generationSpeed,
} from "./usage-log-columns";
import type { Attempt, UsageRecord, UsageSettings } from "./usage-types";
import UsageLogTable, { UsageLogControls } from "./UsageLogTable";
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
  it("keeps total TPS independent of near-zero generation timing", () => {
    for (const generationMs of [0, 50, null, undefined]) {
      expect(generationSpeed({ ...attempt, generationMs })).toBeNull();
    }
    expect(generationSpeed({ ...attempt, generationMs: 51 })).toBeCloseTo(
      60000 / 51,
    );
    expect(logSpeed({ ...attempt, firstTokenMs: 2499, generationMs: 1 })).toBe(
      24,
    );
  });
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
  it("uses explicit choices before the grouped defaults and calculates only reported measures", () => {
    expect(columnVisible("time", undefined, true)).toBe(true);
    expect(columnVisible("speed", undefined, true)).toBe(true);
    expect(
      columnVisible(
        "speed",
        { ...defaultSettings, logColumns: { speed: false } },
        true,
      ),
    ).toBe(false);
    expect(
      columnVisible(
        "speed",
        { ...defaultSettings, logColumns: { speed: true } },
        true,
      ),
    ).toBe(true);
    expect(logSpeed(attempt)).toBe(24);
    expect(logSpeed({ ...attempt, stream: false })).toBe(24);
    expect(logSpeed({ ...attempt, durationMs: 500 })).toBe(120);
    expect(logSpeed({ ...attempt, outputReported: false })).toBeNull();
    expect(
      logSpeed({
        ...attempt,
        usageStatus: "session_supplemented",
        parseReasons: ["prompt_cache_total_mismatch"],
      }),
    ).toBe(24);
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
  it("persists only an explicit visibility change and locks time", () => {
    const save = vi.fn();
    const settings: UsageSettings = {
      ...defaultSettings,
      logColumns: {},
      logNumberMode: "compact",
    };
    render(<UsageLogControls settings={settings} save={save} />);
    expect(save).not.toHaveBeenCalled();
    fireEvent.click(screen.getByText("列设置"));
    expect(screen.getByRole("checkbox", { name: "结束时间" })).toBeDisabled();
    fireEvent.click(screen.getByRole("checkbox", { name: "推理" }));
    expect(save).toHaveBeenCalledWith({
      ...settings,
      logColumns: { reasoning: true },
    });
  });
  it("disables settings changes until settings are loaded", () => {
    const save = vi.fn();
    render(<UsageLogControls settings={undefined} save={save} />);
    expect(
      screen.getByRole("combobox", { name: "日志数字格式" }),
    ).toBeDisabled();
    fireEvent.click(screen.getByText("列设置"));
    for (const checkbox of screen.getAllByRole("checkbox")) {
      expect(checkbox).toBeDisabled();
    }
    expect(screen.getByRole("button", { name: "恢复默认列" })).toBeDisabled();
    expect(save).not.toHaveBeenCalled();
  });
  it("opens details by keyboard and retains grouped columns", () => {
    const open = vi.fn();
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
        settings={defaultSettings}
        name={() => "Fixture"}
        open={open}
      />,
    );
    const row = screen.getByRole("row", { name: /请求详情/ });
    fireEvent.keyDown(row, { key: "Enter" });
    expect(open).toHaveBeenCalledOnce();
    expect(screen.getByTitle("Fixture · DSH")).toBeInTheDocument();
    expect(
      screen.getAllByRole("columnheader").map((c) => c.textContent),
    ).toEqual(["时间 / 供应商 / 模型", "Token", "费用", "性能"]);
    expect(
      screen.getByTitle("fixture-request → fixture-response"),
    ).toBeInTheDocument();
  });
  it("keeps selected optional metrics and exact long numbers in their groups", () => {
    const renderTable = (settings: UsageSettings) => (
      <UsageLogTable
        rows={[
          {
            id: "fixture-exact",
            client: "dsh",
            source: "proxy",
            startedAt: 1,
            attempts: [
              {
                ...attempt,
                tokens: { ...tokens, input: 1234567890123 },
                price: { cost: "0.000000123456", multiplier: "1" },
              },
            ],
          } as UsageRecord,
        ]}
        busy={false}
        settings={settings}
        name={() => "Fixture"}
        open={() => {}}
      />
    );
    const settings: UsageSettings = {
      ...defaultSettings,
      logNumberMode: "exact",
      logColumns: { reasoning: true, hitRate: true, status: true },
    };
    const { rerender } = render(renderTable(settings));
    const row = screen.getByRole("row", { name: /请求详情/ });
    expect(within(row).getByText("1,234,567,890,123")).toBeInTheDocument();
    expect(within(row).getByText("$0.000000123456")).toBeInTheDocument();
    expect(within(row).getByLabelText("推理")).toHaveTextContent("20");
    expect(within(row).getByLabelText("HTTP")).toHaveTextContent("200");
    expect(within(row).getByLabelText("命中率")).toBeInTheDocument();
    for (const label of [
      "输入",
      "输出",
      "缓存读",
      "缓存写",
      "推理",
      "命中率",
    ]) {
      const metric = within(row).getByLabelText(label);
      expect(metric.querySelector("svg")).toHaveAttribute(
        "aria-hidden",
        "true",
      );
      expect(within(metric).queryByText(label)).not.toBeInTheDocument();
    }
    rerender(
      renderTable({
        ...settings,
        logColumns: {
          provider: false,
          model: false,
          input: false,
          output: false,
          cacheRead: false,
          cacheWrite: false,
          cost: false,
          firstToken: false,
          duration: false,
          speed: false,
        },
      }),
    );
    expect(
      screen.getAllByRole("columnheader").map((cell) => cell.textContent),
    ).toEqual(["时间 / 供应商 / 模型"]);
    expect(within(row).queryByText("$0.000000123456")).not.toBeInTheDocument();
  });
  it.each([
    [0, 0, "good", "good"],
    [10000, 60000, "warn", "warn"],
    [30000, 180000, "slow", "slow"],
    [60000, 300000, "critical", "critical"],
  ] as const)(
    "colors measured first-token %i ms and duration %i ms without coloring their labels",
    (firstTokenMs, durationMs, firstSeverity, durationSeverity) => {
      render(
        <UsageLogTable
          rows={[
            {
              id: "fixture-latency",
              client: "codex",
              source: "proxy",
              startedAt: 1,
              attempts: [{ ...attempt, firstTokenMs, durationMs }],
            } as UsageRecord,
          ]}
          busy={false}
          settings={defaultSettings}
          name={() => "Fixture"}
          open={() => {}}
        />,
      );
      for (const [label, severity] of [
        ["首字", firstSeverity],
        ["总耗时", durationSeverity],
      ]) {
        const metric = screen.getByLabelText(label);
        const value = metric.querySelector(".usage-metric-value")!;
        expect(value).toHaveClass(`latency-${severity}`);
        expect(value).not.toHaveClass("is-empty");
        expect(metric.querySelector("small")).not.toHaveAttribute("class");
      }
      const speed = screen
        .getByLabelText("输出 TPS")
        .querySelector(".usage-metric-value")!;
      expect(speed).toHaveClass(durationMs === 0 ? "is-empty" : "tps");
      expect(speed.className).not.toContain("latency-");
    },
  );
  it.each([
    ["proxy", { firstTokenMs: null }, ["首字"]],
    ["proxy", { stream: false }, ["首字"]],
    ["grok", {}, ["首字", "总耗时", "输出 TPS"]],
  ] as const)(
    "keeps missing or inapplicable metrics muted for %s with %j",
    (source, overrides, labels) => {
      render(
        <UsageLogTable
          rows={[
            {
              id: "fixture-missing-latency",
              client: "grok",
              source,
              startedAt: 1,
              attempts: [{ ...attempt, ...overrides }],
            } as UsageRecord,
          ]}
          busy={false}
          settings={defaultSettings}
          name={() => "Fixture"}
          open={() => {}}
        />,
      );
      for (const label of labels) {
        const value = screen
          .getByLabelText(label)
          .querySelector(".usage-metric-value")!;
        expect(value).toHaveTextContent("—");
        expect(value).toHaveClass("is-empty");
        expect(value.className).not.toContain("latency-");
        expect(value).not.toHaveClass("tps");
      }
    },
  );
});

it("matches output tokens over total request time without a TTFT dependency", () => {
  for (const firstTokenMs of [null, 10000, 80000]) {
    expect(
      logSpeed({
        ...attempt,
        tokens: { ...tokens, input: null, output: 120 },
        durationMs: 70000,
        firstTokenMs,
      })?.toFixed(1),
    ).toBe("1.7");
  }
  for (const output of [null, 0, -1, NaN, Infinity]) {
    expect(logSpeed({ ...attempt, tokens: { ...tokens, output } })).toBeNull();
  }
  for (const durationMs of [0, -1, NaN, Infinity]) {
    expect(logSpeed({ ...attempt, durationMs })).toBeNull();
  }
  expect(
    logSpeed({ ...attempt, tokens: { ...tokens, imageOutput: 10 } }),
  ).toBeNull();
});
