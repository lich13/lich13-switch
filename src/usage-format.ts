import type { Attempt, Tokens } from "./usage-types";
export const date = (n: number) =>
  new Date(n).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });
export const preciseDate = (n: number) =>
  new Date(n).toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    fractionalSecondDigits: 3,
    hour12: false,
  });

export function promptTokens(attempts: Attempt[]): Tokens {
  const keys: (keyof Tokens)[] = [
    "inclusiveInput",
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
    "cacheWrite5m",
    "cacheWrite1h",
    "imageInput",
    "imageOutput",
    "audioInput",
    "audioOutput",
  ];
  return Object.fromEntries(
    keys.map((key) => {
      const values = attempts
        .map((a) => a.tokens[key])
        .filter((value): value is number => value != null);
      return [
        key,
        values.length ? values.reduce((sum, value) => sum + value, 0) : null,
      ];
    }),
  ) as Tokens;
}
export function missingReason(a: Attempt, field: string, source: string): string {
  if (
    field === "first_token" &&
    (source !== "proxy" || !a.stream || (a.operation ?? "model") !== "model")
  )
    return "不适用";
  const kind = a.availability?.[field] ?? a.usageStatus ?? "historical_missing";
  return (
    (
      {
        upstream_unreported: "上游未报告",
        partial: "上游未报告",
        reported: "上游未报告",
        ended_early: "提前结束",
        parse_incomplete: "解析不完整",
        historical_missing: "历史缺失",
        not_applicable: "不适用",
        session_supplemented: "会话补齐",
      } as Record<string, string>
    )[kind] ?? "历史缺失"
  );
}
