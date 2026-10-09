import { useCallback, useEffect, useRef, useState } from "react";
import {
  ArrowDown,
  ArrowUp,
  ChevronLeft,
  ChevronRight,
  Database,
  RefreshCw,
  ChevronDown,
  CalendarDays,
  X,
} from "lucide-react";
import { command, subscribe } from "./bridge";
import { errorOf, type GatewayState } from "./types";
import { confirmAction } from "./confirmation";
import Pricing from "./UsagePricing";
import {
  clientName,
  averageFirstToken,
  firstTokenLabel,
  firstTokenTitle,
  type Totals,
  compact,
  money,
  numeric,
  tokenTotal,
  type Attempt,
  type Dashboard,
  type Group,
  type Point,
  type UsageFilter,
  type UsagePage,
  type UsageRecord,
  type UsageState,
} from "./usage-types";
import "./usage.css";
const date = (n: number) =>
  new Date(n).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    hour12: false,
  });
const local = (n: number) => {
  const d = new Date(n);
  return new Date(n - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
};
const ranges = [
  ["today", "今日"],
  ["1", "最近 24 小时"],
  ["7", "7 天"],
  ["14", "14 天"],
  ["30", "30 天"],
  ["all", "全部"],
  ["custom", "自定义"],
];
type RankingView = {
  page: number;
  sort: "requests" | "cost" | "tokens" | "firstToken";
  asc: boolean;
};
const initialRanking: RankingView = { page: 1, sort: "requests", asc: false };
const empty: UsagePage = { rows: [], page: 1, total: 0, detailSince: 0 };
const outcomes: Record<string, string> = {
  success: "已完成",
  limited: "正常结束",
  rejected: "请求被拒绝",
  model_unavailable: "模型不支持",
  cancelled: "已取消",
  failure: "请求失败",
  unknown: "结果未确认",
  completed: "会话完成",
  reported: "会话已报告",
};
export default function Usage({
  onDirtyChange,
}: {
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [range, setRange] = useState("today"),
    [from, setFrom] = useState(local(Date.now() - 86400000)),
    [to, setTo] = useState(local(Date.now())),
    [live, setLive] = useState(true);
  const [client, setClient] = useState(""),
    [provider, setProvider] = useState(""),
    [source, setSource] = useState<"" | "proxy" | "sessions">(""),
    [model, setModel] = useState(""),
    [status, setStatus] = useState(""),
    [page, setPage] = useState(1);
  const [tab, setTab] = useState("requests"),
    [metric, setMetric] = useState<"requests" | "tokens" | "cost">("cost"),
    [expanded, setExpanded] = useState(false);
  const [data, setData] = useState<Dashboard | null>(null),
    [annual, setAnnual] = useState<Point[]>([]),
    [logs, setLogs] = useState<UsagePage>(empty),
    [state, setState] = useState<UsageState | null>(null);
  const [providers, setProviders] = useState<Record<string, string>>({}),
    [selected, setSelected] = useState<UsageRecord | null>(null),
    [sheet, setSheet] = useState(false),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    [heatmapYear, setHeatmapYear] = useState(new Date().getFullYear()),
    [heatBusy, setHeatBusy] = useState(false),
    [timeOpen, setTimeOpen] = useState(false),
    [rowsBusy, setRowsBusy] = useState(false);
  const [rankings, setRankings] = useState<Record<string, RankingView>>({
    providers: initialRanking,
    models: initialRanking,
  });
  const timeButton = useRef<HTMLButtonElement>(null);
  const resetPages = () => {
    setPage(1);
    setRankings((v) => ({
      providers: { ...v.providers, page: 1 },
      models: { ...v.models, page: 1 },
    }));
  };
  const pricingDirty = useRef(false);
  useEffect(() => {
    onDirtyChange?.(sheet || pricingDirty.current);
    return () => onDirtyChange?.(false);
  }, [sheet, onDirtyChange]);
  const priceDirty = useCallback(
    (dirty: boolean) => {
      pricingDirty.current = dirty;
      onDirtyChange?.(dirty || sheet);
    },
    [onDirtyChange, sheet],
  );
  const selectTab = async (next: string) => {
    if (
      next !== tab &&
      pricingDirty.current &&
      !(await confirmAction("离开定价会丢弃未保存的修改。"))
    )
      return;
    setTab(next);
  };
  const [visible, setVisible] = useState(document.visibilityState !== "hidden"),
    [nativeVisible, setNativeVisible] = useState(true);
  const sequence = useRef(0),
    rowsSequence = useRef(0),
    heatSequence = useRef(0);
  const pending = useRef(new Map<string, Promise<unknown>>());
  const request = useCallback(
    <T,>(name: string, args: Record<string, unknown> = {}): Promise<T> => {
      const key = name + JSON.stringify(args);
      let promise = pending.current.get(key);
      if (!promise) {
        promise = command<T>(name, args).finally(() =>
          pending.current.delete(key),
        );
        pending.current.set(key, promise);
      }
      return promise as Promise<T>;
    },
    [],
  );
  const query = useCallback((): UsageFilter => {
    const end = live
      ? Math.floor(Date.now() / 1000) * 1000
      : new Date(to).getTime();
    let start: number | undefined;
    if (range === "today") {
      const d = new Date(end);
      d.setHours(0, 0, 0, 0);
      start = d.getTime();
    } else if (range === "custom") start = new Date(from).getTime();
    else if (range !== "all") start = end - Number(range) * 86400000;
    return {
      start,
      end,
      source: source || undefined,
      client: client || undefined,
      provider: provider || undefined,
      model: model || undefined,
    };
  }, [range, from, to, live, client, provider, model, source]);
  const loadOverview = useCallback(async () => {
    const id = ++sequence.current;
    setBusy(true);
    try {
      const filter = query();
      if (
        filter.start != null &&
        (!Number.isFinite(filter.start) ||
          filter.start > (filter.end ?? Date.now()))
      )
        throw new Error("请选择有效时间范围");
      const overview = await request<Dashboard>("get_usage_dashboard", {
        filter,
      });
      if (id === sequence.current) {
        setData(overview);
        setError("");
      }
    } catch (e) {
      if (id === sequence.current) setError(errorOf(e).message);
    } finally {
      if (id === sequence.current) setBusy(false);
    }
  }, [query, request]);
  const loadRows = useCallback(async () => {
    const id = ++rowsSequence.current;
    setRowsBusy(true);
    try {
      const rows = await request<UsagePage>("get_usage_logs", {
        filter: { ...query(), status: status || undefined, page },
      });
      if (id === rowsSequence.current) setLogs(rows);
    } catch (e) {
      if (id === rowsSequence.current) setError(errorOf(e).message);
    } finally {
      if (id === rowsSequence.current) setRowsBusy(false);
    }
  }, [query, status, page, request]);
  const loadState = useCallback(async () => {
    try {
      setState(await request<UsageState>("get_usage_state"));
    } catch (e) {
      setError(errorOf(e).message);
    }
  }, [request]);
  const load = useCallback(
    () => Promise.all([loadOverview(), loadRows(), loadState()]),
    [loadOverview, loadRows, loadState],
  );
  useEffect(() => {
    if (visible && nativeVisible) void loadOverview();
    return () => {
      sequence.current++;
    };
  }, [loadOverview, visible, nativeVisible]);
  useEffect(() => {
    if (visible && nativeVisible && tab === "requests") void loadRows();
    return () => {
      rowsSequence.current++;
    };
  }, [loadRows, visible, nativeVisible, tab]);
  useEffect(() => {
    void loadState();
    let gone = false;
    let clean = () => {};
    void subscribe("usage-state", () => void loadState()).then((fn) =>
      gone ? fn() : (clean = fn),
    );
    return () => {
      gone = true;
      clean();
    };
  }, [loadState]);
  useEffect(() => {
    let gone = false;
    void Promise.all([
      command<GatewayState>("get_gateway", { clientId: "codex" }),
      command<GatewayState>("get_gateway", { clientId: "claude" }),
    ])
      .then((g) => {
        if (!gone)
          setProviders(
            Object.fromEntries(
              g.flatMap((s) => s.providers.map((p) => [p.id, p.name])),
            ),
          );
      })
      .catch(() => {});
    return () => {
      gone = true;
      sequence.current++;
    };
  }, []);
  useEffect(() => {
    if (range !== "all" || !visible || !nativeVisible) return;
    const id = ++heatSequence.current;
    const filter = query(),
      start = new Date(heatmapYear, 0, 1).getTime();
    setHeatBusy(true);
    void request<Point[]>("get_usage_heatmap", {
      filter: {
        ...filter,
        start,
        end: Math.min(
          Date.now(),
          new Date(heatmapYear + 1, 0, 1).getTime() - 1,
        ),
      },
    })
      .then((v) => {
        if (id === heatSequence.current) {
          setAnnual(v);
          setHeatBusy(false);
        }
      })
      .catch((e) => {
        if (id === heatSequence.current) {
          setError(errorOf(e).message);
          setHeatBusy(false);
        }
      });
    return () => {
      heatSequence.current++;
    };
  }, [
    range,
    heatmapYear,
    query,
    data?.dataVersion,
    visible,
    nativeVisible,
    request,
  ]);
  useEffect(() => {
    const changed = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", changed);
    let clean = () => {},
      disposed = false;
    void subscribe<boolean>("app-visibility", setNativeVisible).then((c) => {
      if (disposed) c();
      else clean = c;
    });
    return () => {
      disposed = true;
      document.removeEventListener("visibilitychange", changed);
      clean();
    };
  }, []);
  useEffect(() => {
    if (!visible || !nativeVisible || !state?.settings.refreshSeconds) return;
    const t = setInterval(
      () => void load(),
      state.settings.refreshSeconds * 1000,
    );
    return () => clearInterval(t);
  }, [visible, nativeVisible, state?.settings.refreshSeconds, load]);
  const choose = <T extends string>(setter: (s: T) => void, value: T) => {
    setter(value);
    resetPages();
  };
  const name = (id: string | null) =>
    id ? providers[id] || `供应商 ${id.slice(0, 8)}` : "本机会话";
  const t = data?.totals,
    cache = t?.tokens.cacheRead,
    inputs =
      (t?.tokens.input ?? 0) +
      (t?.tokens.cacheRead ?? 0) +
      (t?.tokens.cacheWrite ?? 0);
  const open = async (r: UsageRecord) => {
    try {
      setSelected(
        await command<UsageRecord>("get_usage_detail", {
          id: r.id,
          source: source || undefined,
        }),
      );
    } catch (e) {
      setError(errorOf(e).message);
    }
  };
  return (
    <section className="usage-page">
      <header className="usage-heading">
        <h1>用量</h1>
        <div className="usage-actions">
          <span className="usage-sync-status" aria-live="polite">
            {state?.syncing
              ? "同步中…"
              : Object.values(state?.reports ?? {}).some((r) => r.completedAt)
                ? "已同步"
                : ""}
          </span>
          <select
            aria-label="用量刷新频率"
            value={state?.settings.refreshSeconds ?? 30}
            onChange={(e) => {
              if (state)
                void command<UsageState>("set_usage_settings", {
                  settings: {
                    ...state.settings,
                    refreshSeconds: Number(e.target.value),
                  },
                })
                  .then(setState)
                  .catch((e) => setError(errorOf(e).message));
            }}
          >
            {[0, 5, 10, 30, 60].map((n) => (
              <option key={n} value={n}>
                {n ? `${n} 秒刷新` : "手动刷新"}
              </option>
            ))}
          </select>
          <button
            className="icon-button"
            title="数据来源"
            aria-label="数据来源"
            onClick={(e) => {
              e.currentTarget.focus();
              setSheet(true);
            }}
          >
            <Database size={17} />
          </button>
          <button
            className="icon-button"
            aria-label="刷新用量"
            disabled={busy}
            onClick={() => void load()}
          >
            <RefreshCw size={17} />
          </button>
        </div>
      </header>
      <div className="usage-filters">
        <select
          aria-label="用量客户端"
          value={client}
          onChange={(e) => {
            choose(setClient, e.target.value);
            setProvider("");
          }}
        >
          <option value="">全部客户端</option>
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
        </select>
        <select
          aria-label="用量供应商"
          value={provider}
          onChange={(e) => choose(setProvider, e.target.value)}
        >
          <option value="">全部供应商</option>
          {Object.entries(providers).map(([id, n]) => (
            <option key={id} value={id}>
              {n}
            </option>
          ))}
        </select>
        <input
          aria-label="计价模型筛选"
          list="usage-models"
          placeholder="计价模型"
          value={model}
          onChange={(e) => choose(setModel, e.target.value)}
        />
        <datalist id="usage-models">
          {data?.models
            .filter((m) => m.id !== "unknown")
            .map((m) => (
              <option key={m.id} value={m.id} />
            ))}
        </datalist>
        <select
          aria-label="用量来源"
          value={source}
          onChange={(e) => choose(setSource, e.target.value as typeof source)}
        >
          <option value="">全部（去重）</option>
          <option value="proxy">本网关</option>
          <option value="sessions">本机会话</option>
        </select>
        <button
          ref={timeButton}
          className="usage-time-button"
          aria-label="用量时间范围"
          aria-haspopup="dialog"
          aria-expanded={timeOpen}
          onClick={() => setTimeOpen(true)}
        >
          <CalendarDays size={14} />
          {ranges.find(([id]) => id === range)?.[1]}
          <ChevronDown size={13} />
        </button>
      </div>
      {timeOpen && (
        <TimeRange
          range={range}
          from={from}
          to={to}
          live={live}
          anchor={timeButton.current}
          close={() => setTimeOpen(false)}
          apply={(next) => {
            setRange(next.range);
            setFrom(next.from);
            setTo(next.to);
            setLive(next.live);
            resetPages();
            setTimeOpen(false);
          }}
        />
      )}
      {(error || state?.error) && (
        <p role="alert" className="form-error">
          {error || state?.error}
        </p>
      )}
      <div className="usage-metrics">
        <Metric
          label="估算费用"
          value={money(t?.cost)}
          title={t?.cost}
          extra={t?.unpriced ? `${t.unpriced} 条未定价` : undefined}
        />
        <Metric
          label="请求"
          value={compact(t?.requests)}
          title={numeric(t?.requests)}
        />
        <Metric
          label="实际 Token"
          value={t ? compact(tokenTotal(t.tokens)) : "—"}
          title={t ? numeric(tokenTotal(t.tokens)) : undefined}
        />
        <Metric
          label="平均首字"
          value={firstTokenLabel(averageFirstToken(t))}
          title={firstTokenTitle(t)}
        />
      </div>
      <button
        className="usage-more-metrics"
        aria-expanded={expanded}
        onClick={() => setExpanded(!expanded)}
      >
        更多指标
        <ChevronDown size={13} />
      </button>
      {expanded && t && (
        <div className="usage-breakdown">
          <span>
            缓存命中率
            <b>
              {cache != null && inputs
                ? `${((cache / inputs) * 100).toFixed(1)}%`
                : "—"}
            </b>
          </span>
          {[
            ["输入", t.tokens.input],
            ["输出", t.tokens.output],
            ["缓存读取", t.tokens.cacheRead],
            ["缓存写入", t.tokens.cacheWrite],
          ].map(([n, v]) => (
            <span key={n}>
              {n}
              <b>
                <NumberValue value={v as number | null} />
              </b>
            </span>
          ))}
          <span>
            HTTP 成功率
            <b>
              {t.statusKnown
                ? `${((t.success / t.statusKnown) * 100).toFixed(1)}%`
                : "—"}
            </b>
          </span>
          <span>
            会话记录
            <b>
              <NumberValue value={t.sessions} />
            </b>
          </span>
        </div>
      )}
      <div className="usage-chart-card">
        <header>
          <div role="tablist" aria-label="趋势指标">
            {(
              [
                ["cost", "费用"],
                ["tokens", "Token"],
                ["requests", "请求"],
              ] as const
            ).map(([v, n]) => (
              <button
                role="tab"
                aria-selected={metric === v}
                key={v}
                onClick={() => setMetric(v)}
              >
                {n}
              </button>
            ))}
          </div>
          <span>{range === "all" ? "年度用量" : "用量趋势"}</span>
        </header>
        {range === "all" ? (
          <Heatmap
            points={annual}
            year={heatmapYear}
            metric={metric}
            busy={heatBusy}
            firstYear={
              data?.trend[0]
                ? new Date(data.trend[0].time).getFullYear()
                : new Date().getFullYear()
            }
            onYear={setHeatmapYear}
          />
        ) : (
          <Trend
            points={data?.trend ?? []}
            stepMs={data?.trendStepMs ?? 3600000}
            metric={metric}
            onZoom={(start, end) => {
              setRange("custom");
              setFrom(local(start));
              setTo(local(end));
              setLive(false);
              resetPages();
            }}
          />
        )}
      </div>
      <div className="usage-tabs" role="tablist" aria-label="用量明细">
        {[
          ["requests", "请求日志"],
          ["providers", "供应商"],
          ["models", "模型"],
          ["pricing", "定价"],
        ].map(([id, n]) => (
          <button
            role="tab"
            aria-selected={tab === id}
            key={id}
            onClick={() => void selectTab(id)}
          >
            {n}
          </button>
        ))}
        {tab === "requests" && (
          <select
            aria-label="请求状态"
            value={status}
            onChange={(e) => choose(setStatus, e.target.value)}
          >
            <option value="">全部状态</option>
            {["2xx", "4xx", "5xx"].map((v) => (
              <option key={v}>{v}</option>
            ))}
            <option value="none">会话 / 无状态码</option>
          </select>
        )}
      </div>
      {tab === "requests" ? (
        <>
          <div className="usage-table-scroll">
            <table className="usage-table" aria-busy={rowsBusy}>
              <thead>
                <tr>
                  {[
                    "时间",
                    "客户端",
                    "供应商",
                    "模型",
                    "输入",
                    "输出",
                    "缓存",
                    "费用",
                  ].map((v) => (
                    <th key={v}>{v}</th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {logs.rows.map((r) => {
                  const a = r.attempts.at(-1);
                  if (!a) return null;
                  return (
                    <tr
                      key={r.id}
                      tabIndex={0}
                      onClick={(e) => {
                        e.currentTarget.focus();
                        void open(r);
                      }}
                      onKeyDown={(e) => {
                        if (e.key === "Enter") void open(r);
                      }}
                      aria-label={`${date(r.startedAt)} ${name(a.provider)} 请求详情`}
                    >
                      <td title={new Date(r.startedAt).toLocaleString()}>
                        {a.status != null && a.status >= 400 && (
                          <span
                            className="usage-error-dot"
                            title={`HTTP ${a.status}`}
                          />
                        )}
                        <time>{date(r.startedAt)}</time>
                      </td>
                      <td>{clientName(r.client)}</td>
                      <td className="usage-name" title={name(a.provider)}>
                        {name(a.provider)}
                      </td>
                      <td
                        className="usage-model"
                        title={
                          a.requestedModel &&
                          a.responseModel &&
                          a.requestedModel !== a.responseModel
                            ? `${a.requestedModel} → ${a.responseModel}`
                            : (a.responseModel ?? a.requestedModel ?? "未提供")
                        }
                      >
                        <Model a={a} />
                      </td>
                      <td>
                        <NumberValue value={a.tokens.input} />
                      </td>
                      <td>
                        <NumberValue value={a.tokens.output} />
                      </td>
                      <td>
                        <NumberValue value={a.tokens.cacheRead} />
                      </td>
                      <td title={a.price?.cost}>
                        {money(a.price?.cost)}
                        {a.price && Number(a.price.multiplier) !== 1 && (
                          <span className="usage-multiplier">
                            ×{a.price.multiplier}
                          </span>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
            {logs.rows.length === 0 && (
              <div className="usage-empty">暂无请求</div>
            )}
          </div>
          <Pagination page={logs.page} total={logs.total} onChange={setPage} />
        </>
      ) : tab === "pricing" ? (
        <Pricing onDirtyChange={priceDirty} />
      ) : (
        <Ranking
          groups={
            tab === "providers" ? (data?.providers ?? []) : (data?.models ?? [])
          }
          name={(id) =>
            tab === "providers"
              ? name(id === "session" ? null : id)
              : id === "web_search"
                ? "网络搜索"
                : id
          }
          kind={tab === "providers" ? "供应商" : "模型"}
          view={rankings[tab]}
          onChange={(v) =>
            setRankings((previous) => ({ ...previous, [tab]: v }))
          }
        />
      )}
      {selected && (
        <Detail
          record={selected}
          name={name}
          onClose={() => setSelected(null)}
        />
      )}
      {sheet && state && (
        <Sources
          state={state}
          counts={data?.sources ?? {}}
          historyIncomplete={data?.sourceHistoryIncomplete ?? false}
          onClose={() => setSheet(false)}
          onChange={(s) => {
            setState(s);
            void load();
          }}
        />
      )}
    </section>
  );
}
function Metric({
  label,
  value,
  title,
  extra,
}: {
  label: string;
  value: string;
  title?: string;
  extra?: string;
}) {
  const [focused, setFocused] = useState(false);
  return (
    <div className="usage-metric">
      <span>{label}</span>
      <strong
        tabIndex={0}
        title={title}
        aria-label={title}
        onFocus={() => setFocused(true)}
        onBlur={() => setFocused(false)}
        onKeyDown={(event) => {
          if (event.key === "Escape") setFocused(false);
        }}
      >
        {value}
      </strong>
      {focused && title && (
        <span className="usage-exact-value" role="tooltip">
          {title}
        </span>
      )}
      {extra && <em>{extra}</em>}
    </div>
  );
}
function NumberValue({ value }: { value: number | null | undefined }) {
  const [focused, setFocused] = useState(false);
  return (
    <span
      className="usage-number"
      tabIndex={0}
      title={numeric(value)}
      aria-label={numeric(value)}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
    >
      {focused ? numeric(value) : compact(value)}
    </span>
  );
}
function Model({ a }: { a: Attempt }) {
  return (
    <>
      {a.operation === "web_search"
        ? "网络搜索"
        : (a.responseModel ?? a.requestedModel ?? "未提供")}
    </>
  );
}
function Trend({
  points,
  stepMs,
  metric,
  onZoom,
}: {
  points: Point[];
  stepMs: number;
  metric: "requests" | "tokens" | "cost";
  onZoom: (a: number, b: number) => void;
}) {
  const [hover, setHover] = useState<number | null>(null),
    start = useRef<number | null>(null);
  const values = points.map((p) =>
      metric === "cost"
        ? Number(p.totals.cost)
        : metric === "tokens"
          ? (tokenTotal(p.totals.tokens) ?? 0)
          : p.totals.requests,
    ),
    max = Math.max(1, ...values);
  const first = points[0]?.time ?? 0,
    last = points.at(-1)?.time ?? 1;
  const x = (p: Point) =>
    ((p.time - first) / Math.max(1, last - first)) * 960 + 20;
  const path = points
    .map((p, i) => `${i ? "L" : "M"}${x(p)},${142 - (values[i] / max) * 116}`)
    .join(" ");
  const index = (e: React.PointerEvent<SVGSVGElement>) => {
    const rect = e.currentTarget.getBoundingClientRect();
    const time =
      first + ((e.clientX - rect.left) / rect.width) * (last - first);
    return points.reduce(
      (best, p, i) =>
        Math.abs(p.time - time) < Math.abs(points[best].time - time) ? i : best,
      0,
    );
  };
  return (
    <div className="usage-trend">
      <svg
        role="img"
        aria-label="用量趋势，拖动选择时间范围"
        viewBox="0 0 1000 175"
        onPointerMove={(e) => points.length && setHover(index(e))}
        onPointerLeave={() => setHover(null)}
        onPointerDown={(e) => {
          if (points.length) start.current = index(e);
        }}
        onPointerUp={(e) => {
          if (start.current != null && points.length) {
            const finish = index(e);
            if (finish !== start.current)
              onZoom(
                Math.min(points[start.current].time, points[finish].time),
                Math.max(points[start.current].time, points[finish].time) +
                  stepMs,
              );
            start.current = null;
          }
        }}
      >
        <line x1="20" x2="980" y1="142" y2="142" className="usage-grid" />
        <line x1="20" x2="980" y1="80" y2="80" className="usage-grid" />
        {path && <path d={path} className="usage-trend-line" />}
        {points.map((p, i) => (
          <circle
            key={p.time}
            cx={x(p)}
            cy={142 - (values[i] / max) * 116}
            r={hover === i ? 5 : 2}
            className="usage-trend-dot"
          >
            <title>
              {date(p.time)} ·{" "}
              {metric === "cost" ? money(p.totals.cost) : numeric(values[i])}
            </title>
          </circle>
        ))}
        {points.length > 0 && (
          <>
            <text x="20" y="168">
              {date(first)}
            </text>
            <text x="980" y="168" textAnchor="end">
              {date(last)}
            </text>
          </>
        )}
      </svg>
      {hover != null && points[hover] && (
        <div className="usage-chart-tooltip">
          {date(points[hover].time)} ·{" "}
          {metric === "cost"
            ? money(points[hover].totals.cost)
            : numeric(values[hover])}
        </div>
      )}
      {points.length === 0 && (
        <span className="usage-chart-empty">暂无数据</span>
      )}
    </div>
  );
}
function Heatmap({
  points,
  year,
  metric,
  busy,
  firstYear,
  onYear,
}: {
  points: Point[];
  year: number;
  metric: "requests" | "tokens" | "cost";
  busy: boolean;
  firstYear: number;
  onYear: (year: number) => void;
}) {
  const day = (value: number) => local(value).slice(0, 10);
  const value = (point: Point) =>
    metric === "cost"
      ? Number(point.totals.cost)
      : metric === "tokens"
        ? (tokenTotal(point.totals.tokens) ?? 0)
        : point.totals.requests;
  const map = new Map(points.map((p) => [day(p.time), value(p)]));
  const days = Math.round(
    (new Date(year + 1, 0, 1).getTime() - new Date(year, 0, 1).getTime()) /
      86400000,
  );
  const max = Math.max(1, ...map.values());
  return (
    <div className="usage-heatmap" aria-busy={busy}>
      <div className="usage-heatmap-heading">
        <strong>{year} 年</strong>
        <div>
          <button
            className="icon-button"
            aria-label="上一年"
            disabled={year <= firstYear}
            onClick={() => onYear(year - 1)}
          >
            <ChevronLeft size={14} />
          </button>
          <button
            className="icon-button"
            aria-label="下一年"
            disabled={year >= new Date().getFullYear()}
            onClick={() => onYear(year + 1)}
          >
            <ChevronRight size={14} />
          </button>
        </div>
      </div>
      <div
        className="usage-heatmap-grid"
        role="img"
        aria-label={`${year} 年度用量热力图`}
      >
        {Array.from({ length: new Date(year, 0, 1).getDay() }, (_, i) => (
          <i key={`space-${i}`} />
        ))}
        {Array.from({ length: days }, (_, i) => {
          const d = day(new Date(year, 0, i + 1).getTime()),
            n = map.get(d) ?? 0;
          return (
            <span
              key={d}
              tabIndex={0}
              className={n ? "used" : ""}
              style={{ opacity: n ? 0.25 + (0.75 * n) / max : 1 }}
              title={`${d} · ${metric === "cost" ? money(String(n)) : numeric(n)}${metric === "tokens" ? " Token" : metric === "requests" ? " 次请求" : ""}`}
            />
          );
        })}
      </div>
    </div>
  );
}
function TimeRange({
  range,
  from,
  to,
  live,
  anchor,
  close,
  apply,
}: {
  range: string;
  from: string;
  to: string;
  live: boolean;
  anchor: HTMLButtonElement | null;
  close: () => void;
  apply: (value: {
    range: string;
    from: string;
    to: string;
    live: boolean;
  }) => void;
}) {
  const [draft, setDraft] = useState({ range, from, to, live }),
    [error, setError] = useState("");
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const dialog = ref.current;
    dialog?.showModal();
    if (dialog && anchor) {
      const rect = anchor.getBoundingClientRect();
      dialog.style.left = `${Math.max(12, Math.min(rect.right - 330, window.innerWidth - 342))}px`;
      dialog.style.top = `${Math.min(rect.bottom + 6, Math.max(12, window.innerHeight - dialog.offsetHeight - 12))}px`;
    }
    return () => {
      dialog?.close();
      anchor?.focus({ preventScroll: true });
    };
  }, [anchor]);
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (
      draft.range === "custom" &&
      (!Number.isFinite(new Date(draft.from).getTime()) ||
        !Number.isFinite(new Date(draft.to).getTime()) ||
        new Date(draft.from).getTime() >
          (draft.live ? Date.now() : new Date(draft.to).getTime()))
    ) {
      setError("请选择有效时间范围");
      return;
    }
    apply(draft);
  };
  return (
    <dialog
      ref={ref}
      className="usage-range-dialog"
      aria-label="选择时间范围"
      onCancel={(e) => {
        e.preventDefault();
        close();
      }}
      onClick={(e) => {
        if (e.target === ref.current) {
          const r = e.currentTarget.getBoundingClientRect();
          if (
            e.clientX < r.left ||
            e.clientX > r.right ||
            e.clientY < r.top ||
            e.clientY > r.bottom
          )
            close();
        }
      }}
    >
      <form onSubmit={submit}>
        <label className="usage-field">
          时间范围
          <select
            aria-label="用量时间范围"
            value={draft.range}
            onChange={(e) =>
              setDraft({
                ...draft,
                range: e.target.value,
                live: e.target.value === "custom" ? draft.live : true,
              })
            }
          >
            {ranges.map(([v, n]) => (
              <option key={v} value={v}>
                {n}
              </option>
            ))}
          </select>
        </label>
        {draft.range === "custom" && (
          <div className="usage-range-fields">
            <label className="usage-field">
              开始时间
              <input
                type="datetime-local"
                value={draft.from}
                onChange={(e) => setDraft({ ...draft, from: e.target.value })}
              />
            </label>
            <label className="usage-field">
              结束时间
              <input
                type="datetime-local"
                value={draft.to}
                disabled={draft.live}
                onChange={(e) => setDraft({ ...draft, to: e.target.value })}
              />
            </label>
            <label className="usage-range-live">
              <input
                type="checkbox"
                checked={draft.live}
                onChange={(e) => setDraft({ ...draft, live: e.target.checked })}
              />
              跟随当前
            </label>
          </div>
        )}
        {error && (
          <p role="alert" className="form-error">
            {error}
          </p>
        )}
        <div className="usage-form-actions">
          <button type="button" onClick={close}>
            取消
          </button>
          <button className="primary">确定</button>
        </div>
      </form>
    </dialog>
  );
}
export function Pagination({
  page,
  total,
  onChange,
}: {
  page: number;
  total: number;
  onChange: (page: number) => void;
}) {
  const count = Math.max(1, Math.ceil(total / 20));
  const pages = [1, page - 1, page, page + 1, count]
    .filter((n, i, a) => n > 0 && n <= count && a.indexOf(n) === i)
    .sort((a, b) => a - b);
  return (
    <div className="usage-pagination">
      <span title={numeric(total)}>{compact(total)} 条</span>
      <button
        className="icon-button"
        aria-label="上一页"
        disabled={page <= 1}
        onClick={() => onChange(page - 1)}
      >
        <ChevronLeft size={16} />
      </button>
      {pages.map((n, i) => (
        <span key={n}>
          {i > 0 && n - pages[i - 1] > 1 && <span>…</span>}
          <button
            aria-current={n === page ? "page" : undefined}
            onClick={() => onChange(n)}
          >
            {n}
          </button>
        </span>
      ))}
      <button
        className="icon-button"
        aria-label="下一页"
        disabled={page >= count}
        onClick={() => onChange(page + 1)}
      >
        <ChevronRight size={16} />
      </button>
      <input
        aria-label="跳转页码"
        type="number"
        min="1"
        max={count}
        key={`${page}-${count}`}
        defaultValue={page}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            const n = Number(e.currentTarget.value);
            if (Number.isInteger(n) && n > 0 && n <= count) onChange(n);
          }
        }}
      />
    </div>
  );
}
function Ranking({
  groups,
  name,
  kind,
  view,
  onChange,
}: {
  groups: Group[];
  name: (id: string) => string;
  kind: string;
  view: RankingView;
  onChange: (view: RankingView) => void;
}) {
  const sorted = [...groups].sort((a, b) => {
    const value = (g: Group) =>
      view.sort === "cost"
        ? Number(g.totals.cost)
        : view.sort === "tokens"
          ? tokenTotal(g.totals.tokens)
          : view.sort === "firstToken"
            ? averageFirstToken(g.totals)
            : g.totals.requests;
    const av = value(a),
      bv = value(b);
    if (av == null || bv == null)
      return av == null ? (bv == null ? a.id.localeCompare(b.id) : 1) : -1;
    return (av - bv) * (view.asc ? 1 : -1) || a.id.localeCompare(b.id);
  });
  const page = Math.min(view.page, Math.max(1, Math.ceil(sorted.length / 20)));
  return (
    <>
      <div className="usage-table-scroll">
        <table className="usage-table usage-ranking">
          <thead>
            <tr>
              <th>{kind}</th>
              {[
                ["requests", "请求"],
                ["tokens", "Token"],
                ["cost", "费用"],
              ].map(([key, label]) => (
                <th key={key}>
                  <button
                    onClick={() =>
                      onChange({
                        ...view,
                        page: 1,
                        sort: key as RankingView["sort"],
                        asc: view.sort === key ? !view.asc : false,
                      })
                    }
                  >
                    {label}
                    {view.sort === key &&
                      (view.asc ? (
                        <ArrowUp size={12} />
                      ) : (
                        <ArrowDown size={12} />
                      ))}
                  </button>
                </th>
              ))}
              <th>成功率</th>
              <th>
                <button
                  onClick={() =>
                    onChange({
                      ...view,
                      page: 1,
                      sort: "firstToken",
                      asc: view.sort === "firstToken" ? !view.asc : true,
                    })
                  }
                >
                  平均首字
                  {view.sort === "firstToken" &&
                    (view.asc ? (
                      <ArrowUp size={12} />
                    ) : (
                      <ArrowDown size={12} />
                    ))}
                </button>
              </th>
            </tr>
          </thead>
          <tbody>
            {sorted
              .slice((page - 1) * 20, page * 20)
              .map(({ id, totals: t }) => (
                <tr key={id}>
                  <td title={name(id)}>{name(id)}</td>
                  <td>
                    <NumberValue value={t.requests} />
                  </td>
                  <td>
                    <NumberValue value={tokenTotal(t.tokens)} />
                  </td>
                  <td title={t.cost}>{money(t.cost)}</td>
                  <td>
                    {t.statusKnown
                      ? `${((t.success / t.statusKnown) * 100).toFixed(1)}%`
                      : "—"}
                  </td>
                  <td>
                    <FirstTokenValue totals={t} />
                  </td>
                </tr>
              ))}
          </tbody>
        </table>
        {!sorted.length && <div className="usage-empty">暂无数据</div>}
      </div>
      <Pagination
        page={page}
        total={sorted.length}
        onChange={(page) => onChange({ ...view, page })}
      />
    </>
  );
}
function FirstTokenValue({ totals }: { totals: Totals }) {
  const [focused, setFocused] = useState(false);
  return (
    <span
      tabIndex={0}
      className="usage-number"
      title={firstTokenTitle(totals)}
      aria-label={firstTokenTitle(totals)}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
    >
      {focused
        ? firstTokenTitle(totals)
        : firstTokenLabel(averageFirstToken(totals))}
    </span>
  );
}
export function Drawer({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    const focus = document.activeElement as HTMLElement | null;
    const dialog = ref.current;
    dialog?.showModal();
    return () => {
      dialog?.close();
      queueMicrotask(() => {
        if (!dialog?.isConnected && focus?.isConnected)
          focus.focus({ preventScroll: true });
      });
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className="usage-drawer"
      aria-label={title}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) {
          const r = e.currentTarget.getBoundingClientRect();
          if (e.clientX < r.left) onClose();
        }
      }}
    >
      <header>
        <h2>{title}</h2>
        <button className="icon-button" aria-label="关闭详情" onClick={onClose}>
          <X size={18} />
        </button>
      </header>
      <div className="usage-drawer-body">{children}</div>
    </dialog>
  );
}
function Detail({
  record: r,
  name,
  onClose,
}: {
  record: UsageRecord;
  name: (id: string | null) => string;
  onClose: () => void;
}) {
  const a = r.attempts.at(-1)!;
  const total = r.attempts.reduce(
    (sum, a) => sum + Number(a.price?.cost ?? 0),
    0,
  );
  return (
    <Drawer title="请求详情" onClose={onClose}>
      <div className="usage-detail-status">
        <b>
          {a.status == null
            ? r.source === "proxy"
              ? "—"
              : "会话"
            : `HTTP ${a.status}`}
        </b>
        <span>{outcomes[a.outcome] ?? "结果未确认"}</span>
        <span>{clientName(r.client)}</span>
      </div>
      <AttemptDetails
        a={a}
        name={name}
        source={r.source}
        metadata={[
          ["客户端", clientName(r.client)],
          [
            "来源",
            r.source === "proxy" ? "网关" : clientName(r.source) + " 会话",
          ],
          ["请求 ID", r.id],
          [
            "数据归属",
            r.deduplication === "ambiguous"
              ? "关联未确认，未计入合计"
              : r.duplicateOf
                ? "已合并"
                : r.deduplication === "response_id"
                  ? "响应 ID 合并"
                  : r.deduplication === "strict_match"
                    ? "严格匹配合并"
                    : "独立记录",
          ],
        ]}
      />
      <h3>尝试记录</h3>
      <dl className="usage-detail-grid">
        <dt>全部尝试费用</dt>
        <dd>
          {r.attempts.some((a) => !a.price || a.compactedUnpriced)
            ? "含未定价项 · "
            : ""}
          {money(String(total))}
        </dd>
        <dt>实际尝试</dt>
        <dd>
          {numeric(
            r.attempts.reduce((sum, a) => sum + (a.repeatCount ?? 1), 0),
          )}
        </dd>
      </dl>
      {r.attempts.map((attempt, i) => (
        <details key={attempt.id}>
          <summary>
            {i + 1} · {name(attempt.provider)} · {attempt.status ?? "—"}
            {(attempt.repeatCount ?? 1) > 1
              ? ` · 合并 ${compact(attempt.repeatCount)} 次`
              : ""}
          </summary>
          <AttemptDetails a={attempt} name={name} source={r.source} />
        </details>
      ))}
    </Drawer>
  );
}
function rateLabel(key: string) {
  const labels: Record<string, string> = {
    input_cost_per_token: "输入",
    output_cost_per_token: "输出",
    cache_read_input_token_cost: "缓存读取",
    cache_creation_input_token_cost: "缓存写入",
    cache_creation_input_token_cost_above_1hr: "缓存写入（1 小时）",
    input_cost_per_image_token: "图片输入",
    output_cost_per_image_token: "图片输出",
    input_cost_per_audio_token: "音频输入",
    output_cost_per_audio_token: "音频输出",
  };
  const tiers: Record<string, string> = {
    batches: "批处理",
    flex: "弹性",
    priority: "优先",
    ultrafast: "极速",
  };
  const tier = key.match(/_(batches|flex|priority|ultrafast)$/)?.[1];
  const base = tier ? key.slice(0, -tier.length - 1) : key;
  const threshold = base.match(/_above_(100|200|272)k_tokens$/)?.[1];
  const name = threshold
    ? base.replace(/_above_(100|200|272)k_tokens$/, "")
    : base;
  return [
    labels[name] ?? "其他",
    threshold ? `上下文 > ${threshold}K` : null,
    tier ? tiers[tier] : null,
  ]
    .filter(Boolean)
    .join(" · ");
}
function DetailFields({ items }: { items: [string, React.ReactNode][] }) {
  return (
    <dl className="usage-detail-grid">
      {items.map(([label, value]) => (
        <div key={label} className="usage-detail-pair">
          <dt>{label}</dt>
          <dd>{value ?? "未提供"}</dd>
        </div>
      ))}
    </dl>
  );
}
function AttemptDetails({
  a,
  name,
  source,
  metadata = [],
}: {
  a: Attempt;
  name: (id: string | null) => string;
  source: string;
  metadata?: [string, React.ReactNode][];
}) {
  const first =
    source === "proxy" && a.stream && a.operation !== "web_search"
      ? a.firstTokenMs
      : null;
  return (
    <>
      <h3>基本信息</h3>
      <DetailFields
        items={[
          ["供应商", name(a.provider)],
          ["时间", new Date(a.startedAt).toLocaleString()],
          ["请求类型", a.operation === "web_search" ? "网络搜索" : "模型请求"],
          ["请求模型", a.requestedModel],
          ["收到的响应模型", a.responseModel],
          ...metadata,
        ]}
      />
      <h3>Token</h3>
      <DetailFields
        items={[
          ["输入", numeric(a.tokens.input)],
          ["输出", numeric(a.tokens.output)],
          ["缓存读取", numeric(a.tokens.cacheRead)],
          ["缓存写入", numeric(a.tokens.cacheWrite)],
        ]}
      />
      <h3>费用</h3>
      <DetailFields
        items={[
          ["估算费用", money(a.price?.cost)],
          ["倍率", a.price?.multiplier],
          ["计价模型", a.pricingModel],
          [
            "计价依据",
            a.pricingBasis === "provider_mapping"
              ? "供应商精确映射（估算）"
              : a.pricingBasis === "compacted"
                ? "重试消耗汇总（估算）"
                : a.pricingBasis === "request"
                  ? "请求模型（估算）"
                  : a.operation === "web_search"
                    ? "搜索按次"
                    : "收到的响应模型（估算）",
          ],
          ["价格版本", a.price?.version],
          ...(a.mappingRevision
            ? [["映射版本", a.mappingRevision] as [string, string]]
            : []),
        ]}
      />
      {a.price && (
        <details>
          <summary>
            {a.operation === "web_search" ? "按次计费" : "单价 / 百万 Token"}
          </summary>
          {a.operation === "web_search" && (
            <p>
              {a.price.basis?.quantity ?? 1} 次 × $0.01 × {a.price.multiplier}
            </p>
          )}
          <DetailFields
            items={Object.entries(a.price.rates).map(([key, value]) => [
              key === "cost_per_request" ? "每次搜索" : rateLabel(key),
              `$${(Number(value) * (key === "cost_per_request" ? 1 : 1e6)).toLocaleString("zh-CN", { maximumFractionDigits: 8 })}`,
            ])}
          />
        </details>
      )}
      <h3>性能</h3>
      <DetailFields
        items={[
          [
            "总耗时",
            a.durationMs ? `${(a.durationMs / 1000).toFixed(3)}s` : "未提供",
          ],
          ["首字", first != null ? `${(first / 1000).toFixed(3)}s` : "未提供"],
          ["传输", a.transport],
          ["流式", a.stream ? "是" : "否"],
        ]}
      />
    </>
  );
}
function Sources({
  state,
  counts,
  historyIncomplete,
  onClose,
  onChange,
}: {
  state: UsageState;
  counts: Record<string, number>;
  historyIncomplete: boolean;
  onClose: () => void;
  onChange: (v: UsageState) => void;
}) {
  const [draft, setDraft] = useState(state.settings),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const run = async (name: string, args: Record<string, unknown> = {}) => {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      if (name === "set_usage_settings") {
        const current = await command<UsageState>("get_usage_state");
        args = {
          settings: {
            ...current.settings,
            recording: draft.recording,
            autoSync: draft.autoSync,
          },
        };
      }
      onChange(await command<UsageState>(name, args));
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Drawer title="数据来源" onClose={onClose}>
      <div className="usage-source-list">
        {["proxy", "codex", "claude"].map((id) => (
          <div key={id}>
            <b>{id === "proxy" ? "网关" : clientName(id)}</b>
            <span>{numeric(counts[id] ?? 0)} 条</span>
            {state.reports[id] && (
              <span>
                导入 {numeric(state.reports[id].imported)} · 合并{" "}
                {numeric(state.reports[id].merged ?? 0)} · 待核对{" "}
                {numeric(state.reports[id].pending ?? 0)}
                {state.syncing
                  ? ` · ${state.reports[id].files}/${state.reports[id].totalFiles ?? 0}`
                  : ""}
              </span>
            )}
            {!!state.reports[id]?.historicalBefore && (
              <span>历史日汇总已保留</span>
            )}
            {state.reports[id]?.completedAt && (
              <time>{date(state.reports[id].completedAt!)}</time>
            )}
            {!!state.reports[id]?.errors && (
              <span className="form-error">
                {state.reports[id].errors} 个文件未同步
              </span>
            )}
          </div>
        ))}
      </div>
      {historyIncomplete && (
        <details className="usage-price-source">
          <summary>历史数据</summary>
          <p>部分旧日汇总仅支持全部来源查询。</p>
        </details>
      )}
      <div className="usage-actions">
        <button
          disabled={busy || state.syncing}
          onClick={() => void run("sync_usage")}
        >
          同步会话
        </button>
        <button
          disabled={busy || state.syncing}
          onClick={() =>
            void confirmAction("重建 Codex 会话用量？网关记录会保留。").then(
              (ok) => {
                if (ok) return run("sync_usage", { rebuild: "codex" });
              },
            )
          }
        >
          重建 Codex 用量
        </button>
        <button
          disabled={busy || state.syncing}
          onClick={() =>
            void confirmAction(
              "重建 Claude Code 会话用量？网关记录会保留。",
            ).then((ok) => {
              if (ok) return run("sync_usage", { rebuild: "claude" });
            })
          }
        >
          重建 Claude 用量
        </button>
      </div>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void run("set_usage_settings");
        }}
      >
        <label className="usage-setting">
          记录网关用量
          <input
            type="checkbox"
            checked={draft.recording}
            onChange={(e) =>
              setDraft({ ...draft, recording: e.target.checked })
            }
          />
        </label>
        <label className="usage-setting">
          自动同步会话
          <input
            type="checkbox"
            checked={draft.autoSync}
            onChange={(e) => setDraft({ ...draft, autoSync: e.target.checked })}
          />
        </label>
        {error && (
          <p role="alert" className="form-error">
            {error}
          </p>
        )}
        <div className="usage-form-actions">
          <button type="button" onClick={onClose}>
            关闭
          </button>
          <button className="primary" disabled={busy}>
            保存
          </button>
        </div>
      </form>
    </Drawer>
  );
}
