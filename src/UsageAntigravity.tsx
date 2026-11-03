import { date } from "./usage-format";
import {
  compact,
  money,
  numeric,
  type AntigravityTrackWindow,
  type AntigravityUsageStats,
} from "./usage-types";

function formatDuration(ms: number): string {
  if (!ms || ms <= 0) return "—";
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const minutes = Math.floor(ms / 60_000);
  const seconds = Math.round((ms % 60_000) / 1000);
  return `${minutes}m ${seconds}s`;
}

function formatCostOrUnpriced(
  costUsd: string,
  unpricedRequests: number,
  requests: number,
): string {
  if (
    Number(costUsd) === 0 &&
    unpricedRequests > 0 &&
    unpricedRequests >= requests
  ) {
    return "未定价";
  }
  return money(costUsd);
}

function TrackWindowBlock({
  title,
  window: w,
}: {
  title: string;
  window: AntigravityTrackWindow;
}) {
  return (
    <div className="usage-ag-window">
      <div className="usage-ag-window-head">
        <span>{title}</span>
        <strong title={w.costUsd}>
          {formatCostOrUnpriced(w.costUsd, w.unpricedRequests, w.requests)}
        </strong>
      </div>
      <dl className="usage-ag-window-stats">
        <div>
          <dt>请求</dt>
          <dd title={numeric(w.requests)}>{compact(w.requests)}</dd>
        </div>
        <div>
          <dt>Token</dt>
          <dd title={numeric(w.tokens)}>{compact(w.tokens)}</dd>
        </div>
        <div>
          <dt>思考</dt>
          <dd title={numeric(w.reasoningTokens)}>
            {compact(w.reasoningTokens)}
          </dd>
        </div>
        <div>
          <dt>速率</dt>
          <dd title={`${numeric(Math.round(w.tokensPerHour))} Token/h`}>
            {compact(Math.round(w.tokensPerHour))}/h
          </dd>
        </div>
      </dl>
    </div>
  );
}

export default function UsageAntigravity({
  stats,
  busy,
  onSelectSession,
}: {
  stats: AntigravityUsageStats | null;
  busy: boolean;
  onSelectSession: (sessionId: string, shortHash: string) => void;
}) {
  if (!stats) {
    return (
      <p className="usage-empty">
        {busy ? "正在加载 Antigravity 统计…" : "暂无 Antigravity 用量数据"}
      </p>
    );
  }

  const mix = stats.tokenMix;
  const segments = [
    { key: "input", label: "新输入", value: mix.input },
    { key: "cacheRead", label: "缓存读取", value: mix.cacheRead },
    { key: "cacheWrite", label: "缓存写入", value: mix.cacheWrite },
    { key: "visibleOutput", label: "可见输出", value: mix.visibleOutput },
    { key: "reasoning", label: "思考输出", value: mix.reasoning },
  ];
  const segmentTotal = segments.reduce((sum, item) => sum + item.value, 0);

  const totalCostLabel = formatCostOrUnpriced(
    stats.costUsd,
    stats.unpricedRequests,
    stats.requests,
  );

  return (
    <div className="usage-ag-panel" aria-busy={busy}>
      <section className="usage-ag-card" aria-label="Token 构成与缓存节省">
        <div className="usage-ag-kpis">
          <div>
            <span>请求轮次</span>
            <strong title={numeric(stats.requests)}>
              {compact(stats.requests)}
            </strong>
          </div>
          <div>
            <span>脱敏会话</span>
            <strong title={numeric(stats.sessions)}>
              {compact(stats.sessions)}
            </strong>
          </div>
          <div>
            <span>估算费用</span>
            <strong title={stats.costUsd}>{totalCostLabel}</strong>
          </div>
          <div>
            <span>思考占输出比</span>
            <strong>
              {mix.thinkingSharePercent == null
                ? "—"
                : `${mix.thinkingSharePercent.toFixed(1)}%`}
            </strong>
          </div>
          <div>
            <span>缓存命中率</span>
            <strong
              title={
                mix.cacheInputEligible
                  ? `${numeric(mix.cacheReadEligible)} / ${numeric(mix.cacheInputEligible)}`
                  : undefined
              }
            >
              {mix.cacheHitPercent == null
                ? "—"
                : `${mix.cacheHitPercent.toFixed(1)}%`}
            </strong>
          </div>
          <div>
            <span>缓存估算节省</span>
            <strong
              className="usage-ag-savings"
              title={mix.cacheSavingsUsd}
            >
              {money(mix.cacheSavingsUsd)}
            </strong>
          </div>
        </div>

        <div
          className="usage-ag-mix-bar"
          role="img"
          aria-label={`Token 构成：总计 ${numeric(mix.total)}`}
        >
          {segmentTotal > 0 ? (
            segments
              .filter((s) => s.value > 0)
              .map((s) => (
                <span
                  key={s.key}
                  className={`usage-ag-mix-seg is-${s.key}`}
                  style={{ width: `${(s.value / segmentTotal) * 100}%` }}
                  title={`${s.label}：${numeric(s.value)}`}
                />
              ))
          ) : (
            <span className="usage-ag-mix-empty" />
          )}
        </div>

        <div className="usage-ag-mix-legend">
          {segments.map((s) => (
            <span key={s.key} className="usage-ag-legend-item">
              <i className={`usage-ag-dot is-${s.key}`} aria-hidden="true" />
              <span>{s.label}</span>
              <b title={numeric(s.value)}>{compact(s.value)}</b>
            </span>
          ))}
        </div>
      </section>

      <section
        className="usage-ag-tracks"
        aria-label="双轨 5 小时与 7 天滚动窗口"
      >
        {stats.tracks.map((track) => (
          <article
            key={track.id}
            className="usage-ag-card usage-ag-track-card"
            data-track={track.id}
          >
            <header className="usage-ag-track-head">
              <h2>{track.label}</h2>
            </header>
            <div className="usage-ag-windows">
              <TrackWindowBlock title="5h 突发窗口" window={track.burst5h} />
              <TrackWindowBlock title="7d 周滚动" window={track.weekly7d} />
            </div>
          </article>
        ))}
      </section>

      {stats.families.length > 0 && (
        <section className="usage-ag-card" aria-label="模型家族分布">
          <h2 className="usage-ag-section-title">模型家族分布</h2>
          <div className="usage-ag-families">
            {stats.families.map((fam) => (
              <div
                key={fam.family}
                className="usage-ag-family-item"
                data-family={fam.family}
              >
                <div className="usage-ag-family-top">
                  <strong>{fam.label}</strong>
                  <span>{fam.sharePercent.toFixed(1)}%</span>
                </div>
                <div className="usage-ag-family-bar" aria-hidden="true">
                  <span
                    style={{
                      width: `${Math.min(100, Math.max(0, fam.sharePercent))}%`,
                    }}
                  />
                </div>
                <div className="usage-ag-family-meta">
                  <span title={`${numeric(fam.requests)} 次请求`}>
                    {compact(fam.requests)} 请求
                  </span>
                  <span title={`${numeric(fam.tokens)} Token`}>
                    {compact(fam.tokens)} Token
                  </span>
                  <span title={`思考 ${numeric(fam.reasoningTokens)}`}>
                    思考 {compact(fam.reasoningTokens)}
                  </span>
                  <b title={fam.costUsd}>{money(fam.costUsd)}</b>
                </div>
              </div>
            ))}
          </div>
        </section>
      )}

      <section className="usage-ag-card" aria-label="脱敏会话排行">
        <h2 className="usage-ag-section-title">脱敏会话排行</h2>
        {stats.topSessions.length === 0 ? (
          <p className="usage-empty">当前时间范围暂无 Antigravity 会话记录</p>
        ) : (
          <div className="usage-table-scroll">
            <table className="usage-table usage-ag-session-table">
              <thead>
                <tr>
                  <th>脱敏会话</th>
                  <th>主要模型</th>
                  <th>轮次</th>
                  <th>Token / 思考</th>
                  <th>缓存命中</th>
                  <th>累计耗时</th>
                  <th>估算费用</th>
                  <th>最近活跃</th>
                  <th>操作</th>
                </tr>
              </thead>
              <tbody>
                {stats.topSessions.map((row) => (
                  <tr key={row.sessionId}>
                    <td>
                      <code className="usage-ag-hash">#{row.shortHash}</code>
                    </td>
                    <td className="usage-model" title={row.primaryModel}>
                      {row.primaryModel}
                    </td>
                    <td>{numeric(row.requests)}</td>
                    <td
                      title={`总计 ${numeric(row.totalTokens)} · 思考 ${numeric(row.reasoningTokens)}`}
                    >
                      {compact(row.totalTokens)} /{" "}
                      {compact(row.reasoningTokens)}
                    </td>
                    <td>
                      {row.cacheHitPercent == null
                        ? "—"
                        : `${row.cacheHitPercent.toFixed(1)}%`}
                    </td>
                    <td>{formatDuration(row.durationMs)}</td>
                    <td title={row.costUsd}>
                      {formatCostOrUnpriced(
                        row.costUsd,
                        row.unpricedRequests,
                        row.requests,
                      )}
                    </td>
                    <td>{date(row.lastActiveAt)}</td>
                    <td>
                      <button
                        type="button"
                        className="usage-ag-session-btn"
                        aria-label={`查看会话 #${row.shortHash} 请求`}
                        onClick={() =>
                          onSelectSession(row.sessionId, row.shortHash)
                        }
                      >
                        查看请求
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}
