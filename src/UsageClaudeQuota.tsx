import { date } from "./usage-format";
import type { ClaudeUsageQuota } from "./usage-types";

export default function UsageClaudeQuota({
  quota,
}: {
  quota: ClaudeUsageQuota | null;
}) {
  return (
    <section
      className="usage-ag-card usage-claude-quota"
      aria-label="Claude 官方额度"
    >
      <header className="usage-ag-track-head">
        <h2 className="usage-ag-section-title">官方额度</h2>
        <span>{[quota?.account, quota?.plan].filter(Boolean).join(" · ")}</span>
      </header>
      {!quota && <p className="usage-empty">正在读取官方额度…</p>}
      {quota?.error && (
        <p className="form-error" role="alert">
          {quota.error}
        </p>
      )}
      <div className="usage-claude-windows">
        {quota?.windows.map((window) => (
          <div className="usage-ag-window" key={window.id}>
            <div className="usage-ag-window-head">
              <span>{window.label}</span>
              <strong>已用 {window.usedPercent.toFixed(1)}%</strong>
            </div>
            <div
              className="usage-ag-family-bar"
              role="meter"
              aria-label={window.label}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={window.usedPercent}
            >
              <span
                style={{
                  width: `${Math.min(100, Math.max(0, window.usedPercent))}%`,
                }}
              />
            </div>
            <dl className="usage-ag-window-stats">
              <div>
                <dt>重置</dt>
                <dd>{window.resetsAt == null ? "—" : date(window.resetsAt)}</dd>
              </div>
            </dl>
          </div>
        ))}
      </div>
    </section>
  );
}
