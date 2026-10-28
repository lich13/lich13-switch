import {
  clientName,
  type UsageRecord,
  type UsageSettings,
} from "./usage-types";
import { date, preciseDate, promptTokens, missingReason } from "./usage-format";
import { durationSeverity, firstTokenSeverity } from "./latency-health";
import {
  ArrowDown,
  ArrowUp,
  Archive,
  SquarePen,
  Brain,
  Percent,
} from "lucide-react";
import {
  cacheHitRate,
  columnVisible,
  interruptionLabel,
  logColumns,
  logMoney,
  logNumber,
  logSpeed,
  unpricedReason,
  type LogColumn,
} from "./usage-log-columns";

export function UsageLogControls({
  settings,
  save,
}: {
  settings: UsageSettings | undefined;
  save: (settings: UsageSettings) => void;
}) {
  const visibility = (id: LogColumn, visible: boolean) => {
    if (!settings || id === "time") return;
    save({
      ...settings,
      logColumns: { ...settings.logColumns, [id]: visible },
    });
  };
  return (
    <div className="usage-log-controls">
      <details className="usage-more">
        <summary>列设置</summary>
        <div>
          {logColumns.map((c) => (
            <label key={c.id}>
              <input
                type="checkbox"
                disabled={c.id === "time" || !settings}
                checked={columnVisible(c.id, settings, false)}
                onChange={(e) => visibility(c.id, e.target.checked)}
              />
              {c.label}
            </label>
          ))}
          <button
            disabled={!settings}
            onClick={() => settings && save({ ...settings, logColumns: {} })}
          >
            恢复默认列
          </button>
        </div>
      </details>
      <select
        aria-label="日志数字格式"
        value={settings?.logNumberMode === "exact" ? "exact" : "compact"}
        disabled={!settings}
        onChange={(e) =>
          settings &&
          save({
            ...settings,
            logNumberMode: e.target.value as "compact" | "exact",
          })
        }
      >
        <option value="compact">紧凑数字</option>
        <option value="exact">精确数字</option>
      </select>
    </div>
  );
}

export default function UsageLogTable({
  rows,
  busy,
  settings,
  name,
  open,
}: {
  rows: UsageRecord[];
  busy: boolean;
  settings: UsageSettings | undefined;
  name: (id: string | null) => string;
  open: (r: UsageRecord) => void;
}) {
  const visible = (id: LogColumn) => columnVisible(id, settings, false);
  const tokenFields = [
    "input",
    "output",
    "cacheRead",
    "cacheWrite",
    "reasoning",
    "hitRate",
  ] as const;
  const performanceFields = ["firstToken", "duration", "speed"] as const;
  const columns = [
    {
      id: "identity",
      label: "时间 / 供应商 / 模型",
      visible: true,
      width: "minmax(240px, 1.6fr)",
    },
    {
      id: "tokens",
      label: "Token",
      visible: tokenFields.some(visible),
      width: "minmax(max-content, 1fr)",
    },
    {
      id: "cost",
      label: "费用",
      visible: visible("cost"),
      width: "minmax(max-content, 0.65fr)",
    },
    {
      id: "performance",
      label: "性能",
      visible: [...performanceFields, "status" as const].some(visible),
      width: "minmax(132px, 0.8fr)",
    },
  ].filter((c) => c.visible);
  const secondary = columns.filter((c) => c.id !== "identity");
  const exact = settings?.logNumberMode === "exact";
  const tokenWidths = [
    ["input", "cacheRead"],
    ["output", "cacheWrite"],
    ["reasoning", "hitRate"],
  ].map((fields) => {
    const lengths = rows.flatMap((row) => {
      const attempt = row.attempts.at(-1);
      if (!attempt) return [];
      const tokens =
        row.source === "grok" ? promptTokens(row.attempts) : attempt.tokens;
      return fields
        .filter((field) => visible(field as LogColumn))
        .map((field) =>
          field === "hitRate"
            ? 6
            : logNumber(
                tokens[field as keyof typeof tokens] as number | null,
                exact,
              ).length,
        );
    });
    return `calc(${Math.max(4, ...lengths)}ch + 20px)`;
  });
  const tokenColumnCount = visible("reasoning") || visible("hitRate") ? 3 : 2;
  const performanceCount = [...performanceFields, "status" as const].filter(
    visible,
  ).length;

  return (
    <>
      <div className="usage-table-scroll">
        <table
          className={`usage-table usage-request-table${exact ? " exact" : ""}`}
          aria-label="请求记录"
          aria-busy={busy}
          style={
            {
              "--usage-token-columns": tokenWidths
                .slice(0, tokenColumnCount)
                .join(" "),
              "--usage-row-height": `${Math.max(64, performanceCount * 16 + 16)}px`,
              "--usage-columns": columns.map((c) => c.width).join(" "),
              "--usage-areas": `"${columns.map((c) => c.id).join(" ")}"`,
              "--usage-narrow-columns":
                secondary
                  .map((c) =>
                    c.id === "performance"
                      ? c.width
                      : `minmax(0, ${c.id === "cost" ? "0.65" : "1"}fr)`,
                  )
                  .join(" ") || "minmax(0, 1fr)",
              "--usage-narrow-areas": secondary.length
                ? `"${secondary.map(() => "identity").join(" ")}" "${secondary.map((c) => c.id).join(" ")}"`
                : '"identity"',
            } as React.CSSProperties
          }
        >
          <thead>
            <tr>
              {columns.map((c) => (
                <th
                  key={c.id}
                  className={
                    c.id === "identity" ? "usage-sticky-time" : "usage-numeric"
                  }
                >
                  {c.label}
                </th>
              ))}
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => {
              const a = r.attempts.at(-1);
              if (!a) return null;
              const native = r.source === "grok",
                t = native ? promptTokens(r.attempts) : a.tokens,
                state = native ? "" : interruptionLabel(a),
                rate = cacheHitRate(t),
                speed = native ? null : logSpeed(a),
                model =
                  native && r.attempts.length > 1
                    ? `多个模型（${r.attempts.length}）`
                    : a.operation === "web_search"
                      ? "网络搜索"
                      : ((a.upstreamModel
                          ? a.requestedModel
                          : a.responseModel) ??
                        a.requestedModel ??
                        "—");
              const token = (
                value: number | null | undefined,
                field: string,
                extra?: string,
              ) => (
                <span
                  className="usage-number"
                  tabIndex={0}
                  aria-label={
                    value == null
                      ? `— · ${missingReason(a, field, r.source)}`
                      : logNumber(value, true)
                  }
                  title={
                    value == null
                      ? missingReason(a, field, r.source)
                      : [logNumber(value, true), extra]
                          .filter(Boolean)
                          .join(" · ")
                  }
                >
                  {logNumber(value, exact)}
                </span>
              );
              const cells: Record<string, React.ReactNode> = {
                time: (
                  <>
                    <span
                      title={
                        a.status != null && a.status >= 400
                          ? `HTTP ${a.status}`
                          : undefined
                      }
                      className={
                        a.status != null && a.status >= 400 && !native
                          ? "usage-error-dot"
                          : undefined
                      }
                    />
                    {state && (
                      <span
                        className="usage-interruption"
                        title={state}
                        aria-label={state}
                      >
                        ⚠
                      </span>
                    )}
                    <time title={preciseDate(r.finishedAt ?? r.startedAt)}>
                      {date(r.finishedAt ?? r.startedAt)}
                    </time>
                  </>
                ),
                provider: (
                  <span
                    className="usage-provider-inline"
                    tabIndex={0}
                    aria-label={`${name(a.provider)} · ${clientName(r.client)}`}
                    title={`${name(a.provider)} · ${clientName(r.client)}`}
                  >
                    <strong>{name(a.provider)}</strong>
                    <small>{clientName(r.client)}</small>
                  </span>
                ),
                model: (
                  <span
                    className="usage-model-name"
                    tabIndex={0}
                    aria-label={
                      [
                        ...new Set(
                          [
                            a.requestedModel,
                            a.upstreamModel,
                            a.responseModel,
                          ].filter(Boolean),
                        ),
                      ].join(" → ") || model
                    }
                    title={
                      [
                        ...new Set(
                          [
                            a.requestedModel,
                            a.upstreamModel,
                            a.responseModel,
                          ].filter(Boolean),
                        ),
                      ].join(" → ") || model
                    }
                  >
                    <strong>{model}</strong>
                    <span className="usage-badges">
                      {a.stream && !native && (
                        <span className="usage-badge stream">流式</span>
                      )}
                      {(a.operation === "compaction" || a.compactionKind) && (
                        <span className="usage-badge compaction">压缩</span>
                      )}
                      {a.operation === "web_search" && (
                        <span className="usage-badge search">搜索</span>
                      )}
                    </span>
                  </span>
                ),
                input: token(
                  t.input,
                  "input",
                  native
                    ? undefined
                    : `原始输入 ${logNumber(a.rawInputTokens ?? a.inclusiveInputTokens, true)} · ${a.inputBasis === "exclusive" ? "不含缓存" : a.inputBasis === "inclusive" ? "含缓存" : "口径未知"}`,
                ),
                output: token(t.output, "output", "包含已报告的推理 token"),
                cacheRead: token(t.cacheRead, "cache_read"),
                cacheWrite: token(
                  t.cacheWrite,
                  "cache_write",
                  `5m ${logNumber(t.cacheWrite5m, true)} · 1h ${logNumber(t.cacheWrite1h, true)}`,
                ),
                reasoning: token(t.reasoning, "reasoning"),
                hitRate: (
                  <span
                    title={
                      rate == null ? "完整输入口径不足或分母为零" : `${rate}%`
                    }
                  >
                    {rate == null ? "—" : `${rate.toFixed(1)}%`}
                  </span>
                ),
                cost: (
                  <span
                    tabIndex={0}
                    aria-label={
                      native
                        ? "轮次汇总不估算费用"
                        : (a.price?.cost ?? unpricedReason(a))
                    }
                    title={
                      native
                        ? "轮次汇总不估算费用"
                        : (a.price?.cost ?? unpricedReason(a))
                    }
                  >
                    {native ? "—" : logMoney(a.price?.cost, exact)}
                    {!native && a.price && Number(a.price.multiplier) !== 1 && (
                      <span className="usage-multiplier">
                        ×{a.price.multiplier}
                      </span>
                    )}
                  </span>
                ),
                firstToken: (
                  <span
                    className={`usage-metric-value ${
                      native || !a.stream || a.firstTokenMs == null
                        ? "is-empty"
                        : `latency-${firstTokenSeverity(a.firstTokenMs)}`
                    }`}
                    tabIndex={0}
                    title={
                      native
                        ? "无可靠轮次首字时间"
                        : a.firstTokenMs == null
                          ? missingReason(a, "first_token", r.source)
                          : `${a.firstTokenMs} ms`
                    }
                  >
                    {native || !a.stream || a.firstTokenMs == null
                      ? "—"
                      : `${(a.firstTokenMs / 1000).toFixed(2)}s`}
                  </span>
                ),
                duration: (
                  <span
                    className={`usage-metric-value ${
                      native
                        ? "is-empty"
                        : `latency-${durationSeverity(a.durationMs)}`
                    }`}
                    tabIndex={0}
                    title={
                      state ||
                      (native ? "无可靠轮次耗时" : `${a.durationMs} ms`)
                    }
                  >
                    {native ? "—" : `${(a.durationMs / 1000).toFixed(2)}s`}
                    {state && <span className="usage-interruption"> ⚠</span>}
                  </span>
                ),
                speed: (
                  <span
                    className={`usage-metric-value ${speed == null ? "is-empty" : "tps"}`}
                    tabIndex={0}
                    title={
                      speed == null
                        ? "需要有效的输出 Token 和请求总耗时"
                        : `${speed} tok/s`
                    }
                  >
                    {speed == null ? "—" : speed.toFixed(1)}
                  </span>
                ),
                status: native ? "—" : (a.status ?? "—"),
              };
              const tokenLabels = {
                input: "输入",
                output: "输出",
                cacheRead: "缓存读",
                cacheWrite: "缓存写",
                reasoning: "推理",
                hitRate: "命中率",
              };
              const metricLabels = {
                ...tokenLabels,
                firstToken: "首字",
                duration: "总耗时",
                speed: "输出 TPS",
                status: "HTTP",
              };
              cells.identity = (
                <div className="usage-identity-grid">
                  <div className="usage-identity-time">{cells.time}</div>
                  {visible("provider") && (
                    <div className="usage-identity-provider">
                      {cells.provider}
                    </div>
                  )}
                  {visible("model") && (
                    <div className="usage-identity-model">{cells.model}</div>
                  )}
                </div>
              );
              const metric = (
                field:
                  | (typeof tokenFields)[number]
                  | (typeof performanceFields)[number]
                  | "status",
              ) => (
                <span
                  key={field}
                  className={`usage-token-item ${field}`}
                  aria-label={metricLabels[field]}
                  title={metricLabels[field]}
                >
                  <small>{metricLabels[field]}</small>
                  {cells[field]}
                </span>
              );
              cells.tokens = (
                <div
                  className={`usage-token-grid${visible("reasoning") || visible("hitRate") ? " has-extra" : ""}`}
                >
                  {tokenFields.filter(visible).map((field) => {
                    const Icon = {
                      input: ArrowDown,
                      output: ArrowUp,
                      cacheRead: Archive,
                      cacheWrite: SquarePen,
                      reasoning: Brain,
                      hitRate: Percent,
                    }[field];
                    return (
                      <span
                        key={field}
                        className={`usage-token-item ${field}`}
                        aria-label={tokenLabels[field]}
                        title={tokenLabels[field]}
                      >
                        <Icon size={13} aria-hidden="true" />
                        {cells[field]}
                      </span>
                    );
                  })}
                </div>
              );
              cells.performance = (
                <div className="usage-performance-grid">
                  {[...performanceFields, "status" as const]
                    .filter(visible)
                    .map(metric)}
                </div>
              );
              return (
                <tr
                  key={r.id}
                  tabIndex={0}
                  onClick={(e) => {
                    e.currentTarget.focus();
                    open(r);
                  }}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") open(r);
                  }}
                  aria-label={`${date(r.finishedAt ?? r.startedAt)} ${name(a.provider)} ${native ? "轮次详情" : "请求详情"}`}
                >
                  {columns.map((c) => (
                    <td
                      key={c.id}
                      data-field={c.id}
                      data-label={c.label}
                      title={
                        c.id === "identity"
                          ? preciseDate(r.finishedAt ?? r.startedAt)
                          : c.id === "cost" && !native
                            ? (a.price?.cost ?? unpricedReason(a))
                            : undefined
                      }
                      className={
                        c.id === "identity"
                          ? "usage-sticky-time"
                          : "usage-numeric"
                      }
                    >
                      <div className={`usage-cell usage-cell-${c.id}`}>
                        {cells[c.id]}
                      </div>
                    </td>
                  ))}
                </tr>
              );
            })}
          </tbody>
        </table>
        {rows.length === 0 && <div className="usage-empty">暂无请求</div>}
      </div>
    </>
  );
}
