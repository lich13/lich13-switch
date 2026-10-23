export type LatencySeverity = "good" | "warn" | "slow" | "critical";

export function firstTokenSeverity(ms: number): LatencySeverity {
  if (ms >= 60_000) return "critical";
  if (ms >= 30_000) return "slow";
  if (ms >= 10_000) return "warn";
  return "good";
}

export function durationSeverity(ms: number): LatencySeverity {
  if (ms >= 300_000) return "critical";
  if (ms >= 180_000) return "slow";
  if (ms >= 60_000) return "warn";
  return "good";
}
