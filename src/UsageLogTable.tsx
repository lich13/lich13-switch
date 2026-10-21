import { useEffect, useState } from "react";
import {
  clientName,
  type UsageRecord,
  type UsageSettings,
} from "./usage-types";
import { date, preciseDate, promptTokens, missingReason } from "./usage-format";
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

export default function UsageLogTable({
  rows,
  busy,
  settings,
  save,
  name,
  open,
}: {
  rows: UsageRecord[];
  busy: boolean;
  settings: UsageSettings | undefined;
  save: (settings: UsageSettings) => void;
  name: (id: string | null) => string;
  open: (r: UsageRecord) => void;
}) {
  const [narrow, setNarrow] = useState(window.innerWidth < 900);
  useEffect(() => {
    const resize = () => setNarrow(window.innerWidth < 900);
    window.addEventListener("resize", resize);
    return () => window.removeEventListener("resize", resize);
  }, []);
  const columns = logColumns.filter((c) =>
      columnVisible(c.id, settings, narrow),
    ),
    exact = settings?.logNumberMode === "exact";
  const visibility = (id: LogColumn, visible: boolean) => {
    if (!settings || id === "time") return;
    save({
      ...settings,
      logColumns: { ...settings.logColumns, [id]: visible },
    });
  };
  return (
    <>
      <div className="usage-log-controls">
        <details className="usage-more">
          <summary>列设置</summary>
          <div>
            {logColumns.map((c) => (
              <label key={c.id}>
                <input
                  type="checkbox"
                  disabled={c.id === "time" || !settings}
                  checked={columnVisible(c.id, settings, narrow)}
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
          value={exact ? "exact" : "compact"}
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
      <div className="usage-table-scroll">
        <table className="usage-table usage-request-table" aria-busy={busy}>
          <colgroup>
            {columns.map((c) => (
              <col key={c.id} style={{ width: c.width }} />
            ))}
          </colgroup>
          <thead>
            <tr>
              {columns.map((c) => (
                <th
                  key={c.id}
                  className={
                    c.id === "time"
                      ? "usage-sticky-time"
                      : c.id === "provider" || c.id === "model"
                        ? ""
                        : "usage-numeric"
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
                      : (a.responseModel ?? a.requestedModel ?? "—");
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
              const cells: Record<LogColumn, React.ReactNode> = {
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
                    title={`${name(a.provider)} · ${clientName(r.client)}`}
                  >
                    {name(a.provider)} · {clientName(r.client)}
                  </span>
                ),
                model: (
                  <span
                    className="usage-model-name"
                    title={
                      a.requestedModel && a.responseModel
                        ? `${a.requestedModel} → ${a.responseModel}`
                        : model
                    }
                  >
                    {model}
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
                    title={
                      speed == null
                        ? "仅流式且用量和生成时长完整时计算"
                        : `${speed} tok/s`
                    }
                  >
                    {speed == null ? "—" : speed.toFixed(1)}
                  </span>
                ),
                status: native ? "—" : (a.status ?? "—"),
              };
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
                      title={
                        c.id === "time"
                          ? preciseDate(r.finishedAt ?? r.startedAt)
                          : c.id === "cost" && !native
                            ? (a.price?.cost ?? unpricedReason(a))
                            : undefined
                      }
                      className={
                        c.id === "time"
                          ? "usage-sticky-time"
                          : c.id === "provider" || c.id === "model"
                            ? ""
                            : "usage-numeric"
                      }
                    >
                      {cells[c.id]}
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
