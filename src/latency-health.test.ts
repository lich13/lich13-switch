import { describe, expect, it } from "vitest";
import { durationSeverity, firstTokenSeverity } from "./latency-health";

describe("latency severity", () => {
  it.each([
    [0, "good"],
    [9999, "good"],
    [10000, "warn"],
    [29999, "warn"],
    [30000, "slow"],
    [59999, "slow"],
    [60000, "critical"],
  ] as const)("classifies a first token at %i ms as %s", (ms, severity) => {
    expect(firstTokenSeverity(ms)).toBe(severity);
  });

  it.each([
    [0, "good"],
    [59999, "good"],
    [60000, "warn"],
    [179999, "warn"],
    [180000, "slow"],
    [299999, "slow"],
    [300000, "critical"],
  ] as const)("classifies a duration of %i ms as %s", (ms, severity) => {
    expect(durationSeverity(ms)).toBe(severity);
  });
});
