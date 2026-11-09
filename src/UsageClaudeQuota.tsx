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
  const tightestRemaining = windows.length
    ? Math.min(
        ...windows.map((w) => Math.max(0, Math.min(100, 100 - w.usedPercent))),
      )
    : null;
  const summaryTone =
    tightestRemaining != null ? quotaTone(tightestRemaining) : "healthy";
  const accountMeta = [quota?.account, quota?.plan].filter(Boolean).join(" · ");

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
              <i className="usage-quota-dot" aria-hidden="true" />
              最紧剩余 {formatRemainingPercent(tightestRemaining)}
            </span>
          )}
        </div>
        {accountMeta && (
          <span className="usage-claude-meta">{accountMeta}</span>
        )}
      </header>
      {!quota && <p className="usage-empty">正在读取官方额度…</p>}
      {quota?.error && (
        <p className="form-error" role="alert">
          {quota.error}
        </p>
      )}
      {windows.length > 0 && (
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
                <div className="usage-claude-window-foot">
                  <span>已用 {used.toFixed(1)}%</span>
                  <span>
                    重置 {window.resetsAt == null ? "—" : date(window.resetsAt)}
                  </span>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}
