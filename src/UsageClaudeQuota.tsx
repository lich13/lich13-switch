import { date } from "./usage-format";
import type { ClaudeUsageQuota } from "./usage-types";

export function quotaTone(
  remainingPercent: number,
): "healthy" | "warning" | "danger" {
  if (remainingPercent >= 50) return "healthy";
  if (remainingPercent >= 20) return "warning";
  return "danger";
}

export function formatRemainingPercent(value: number): string {
  const clamped = Math.max(0, Math.min(100, value));
  return `${clamped.toFixed(1)}%`;
}

export default function UsageClaudeQuota({
  quota,
}: {
  quota: ClaudeUsageQuota | null;
}) {
  const windows = quota?.windows ?? [];
  const primaryWindow =
    windows.find((w) => w.id === "five_hour") ?? windows[0] ?? null;
  const primaryRemaining = primaryWindow
    ? Math.max(0, Math.min(100, 100 - primaryWindow.usedPercent))
    : null;
  const tightestRemaining = windows.length
    ? Math.min(
        ...windows.map((w) => Math.max(0, Math.min(100, 100 - w.usedPercent))),
      )
    : null;
  const summaryTone =
    tightestRemaining != null ? quotaTone(tightestRemaining) : "healthy";
  const ringCircumference = 2 * Math.PI * 10;
  const ringOffset =
    primaryRemaining == null
      ? ringCircumference
      : ringCircumference * (1 - primaryRemaining / 100);

  return (
    <section
      className="usage-ag-card usage-claude-quota"
      aria-label="Claude 官方额度"
    >
      <header className="usage-ag-track-head">
        <div className="usage-claude-head-title">
          <h2 className="usage-ag-section-title">官方额度</h2>
          {tightestRemaining != null && (
            <span
              className={`usage-quota-remaining-pill is-${summaryTone}`}
              title="所有周期中最紧窗口的剩余百分比"
            >
              {primaryRemaining != null && (
                <svg
                  className="usage-quota-ring"
                  viewBox="0 0 24 24"
                  aria-hidden="true"
                >
                  <circle
                    className="usage-quota-ring-track"
                    cx="12"
                    cy="12"
                    r="10"
                  />
                  <circle
                    className="usage-quota-ring-fill"
                    cx="12"
                    cy="12"
                    r="10"
                    strokeDasharray={ringCircumference}
                    strokeDashoffset={ringOffset}
                  />
                </svg>
              )}
              最紧剩余 {formatRemainingPercent(tightestRemaining)}
            </span>
          )}
        </div>
        <span>{[quota?.account, quota?.plan].filter(Boolean).join(" · ")}</span>
      </header>
      {!quota && <p className="usage-empty">正在读取官方额度…</p>}
      {quota?.error && (
        <p className="form-error" role="alert">
          {quota.error}
        </p>
      )}
      <div className="usage-claude-windows">
        {windows.map((window) => {
          const used = Math.min(100, Math.max(0, window.usedPercent));
          const remaining = Math.min(100, Math.max(0, 100 - used));
          const tone = quotaTone(remaining);
          return (
            <div
              className={`usage-ag-window usage-claude-window is-${tone}`}
              data-tone={tone}
              key={window.id}
            >
              <div className="usage-ag-window-head">
                <span>{window.label}</span>
                <span className={`usage-quota-remaining-pill is-${tone}`}>
                  剩余 {formatRemainingPercent(remaining)}
                </span>
              </div>
              <div
                className={`usage-ag-family-bar usage-quota-dual-bar is-${tone}`}
                role="meter"
                aria-label={window.label}
                aria-valuemin={0}
                aria-valuemax={100}
                aria-valuenow={Number(remaining.toFixed(1))}
              >
                <span
                  className="usage-quota-fill-remaining"
                  style={{
                    width: `${remaining}%`,
                  }}
                />
              </div>
              <dl className="usage-ag-window-stats">
                <div>
                  <dt>已用</dt>
                  <dd>已用 {used.toFixed(1)}%</dd>
                </div>
                <div>
                  <dt>重置</dt>
                  <dd>
                    {window.resetsAt == null ? "—" : date(window.resetsAt)}
                  </dd>
                </div>
              </dl>
            </div>
          );
        })}
      </div>
    </section>
  );
}
