import UsageBill from "./UsageBill";
import Detail from "./UsageDetail";
import Sources from "./UsageSources";
import UsageLogTable, { UsageLogControls } from "./UsageLogTable";
export { Drawer } from "./UsageDrawer";
import { subscriptions } from "./subscriptions";
import type {
  AntigravityAuthState,
  CommandName,
  CommandParams,
  CommandResult,
} from "./bridge-contracts";
import {
  useCallback,
  useEffect,
  useEffectEvent,
  useRef,
  useState,
} from "react";
import {
  Download,
  ArrowDown,
  ArrowUp,
  ChevronLeft,
  ChevronRight,
  Database,
  RefreshCw,
  ChevronDown,
  CalendarDays,
} from "lucide-react";
import { command } from "./bridge";
import { errorOf } from "./types";
import { confirmAction } from "./confirmation";
import Pricing from "./UsagePricing";
import UsageAntigravity from "./UsageAntigravity";
import UsageClaudeQuota from "./UsageClaudeQuota";
import {
  type AntigravityUsageStats,
  type ClaudeUsageQuota,
  averageFirstToken,
  firstTokenLabel,
  firstTokenTitle,
  type Totals,
  compact,
  money,
  numeric,
  tokenTotal,
  type Dashboard,
  type Group,
  type UsageFilter,
  type UsagePage,
  type UsageRecord,
  type UsageState,
} from "./usage-types";
import "./usage.css";
export function syncStatus(state: UsageState | null, client: string) {
  if (!state) return "";
  if (state.maintenance) return state.maintenance;
  const reports = client
    ? [state.reports[client]].filter(Boolean)
    : Object.values(state.reports);
  if (
    reports.some((r) =>
      ["syncing", "scanning", "rebuilding"].includes(r.phase ?? ""),
    ) ||
    (!client && state.syncing)
  )
    return "同步中…";
  if (!reports.length) return "";
  if (reports.every((r) => r.phase === "failed")) return "同步失败";
  if (
    reports.some(
      (r) => r.phase === "failed" || r.phase === "partial" || r.errors > 0,
    )
  )
    return "部分未同步";
  return reports.every((r) => r.completedAt != null) ? "已同步" : "";
}
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
export default function Usage({
  onDirtyChange,
}: {
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [billOpen, setBillOpen] = useState(false);
  const [billResult, setBillResult] = useState("");
  const [range, setRange] = useState("today"),
    [from, setFrom] = useState(local(Date.now() - 86400000)),
    [to, setTo] = useState(local(Date.now())),
    [live, setLive] = useState(true);
  const [client, setClient] = useState(""),
    [provider, setProvider] = useState(""),
    [operation, setOperation] = useState(""),
    [sessionId, setSessionId] = useState(""),
    [sessionTitle, setSessionTitle] = useState(""),
    [status, setStatus] = useState(""),
    [page, setPage] = useState(1);
  const [tab, setTab] = useState("requests");
  const independent = tab === "antigravity" || tab === "claude";
  const [claudeQuota, setClaudeQuota] = useState<ClaudeUsageQuota | null>(null);
  const [agStats, setAgStats] = useState<AntigravityUsageStats | null>(null),
    [agBusy, setAgBusy] = useState(false),
    [agCurrentAccount, setAgCurrentAccount] = useState<string | null>(null);
  const [data, setData] = useState<Dashboard | null>(null),
    [logs, setLogs] = useState<UsagePage>(empty),
    [state, setState] = useState<UsageState | null>(null);
  const [providers, setProviders] = useState<Record<string, string>>({}),
    [claudeProviders, setClaudeProviders] = useState<Record<string, string>>(
      {},
    ),
    [agProviders, setAgProviders] = useState<Record<string, string>>({}),
    [selected, setSelected] = useState<UsageRecord | null>(null),
    [sheet, setSheet] = useState(false),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
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
    onDirtyChange?.(sheet || billOpen || pricingDirty.current);
    return () => onDirtyChange?.(false);
  }, [sheet, billOpen, onDirtyChange]);
  const priceDirty = useCallback(
    (dirty: boolean) => {
      pricingDirty.current = dirty;
      onDirtyChange?.(dirty || sheet || billOpen);
    },
    [onDirtyChange, sheet, billOpen],
  );
  const selectTab = async (next: string) => {
    if (
      next !== tab &&
      pricingDirty.current &&
      !(await confirmAction("离开定价会丢弃未保存的修改。"))
    )
      return;
    if ((next === "antigravity" || next === "claude") && next !== client) {
      setClient(next);
      setProvider("");
      setSessionId("");
      setSessionTitle("");
      resetPages();
    }
    if (next !== tab) {
      setAgStats(null);
      setAgBusy(false);
    }
    setTab(next);
  };
  const [visible, setVisible] = useState(document.visibilityState !== "hidden"),
    [nativeVisible, setNativeVisible] = useState(true);
  const sequence = useRef(0),
    rowsSequence = useRef(0),
    agSequence = useRef(0),
    stateSequence = useRef(0),
    detailSequence = useRef(0);
  useEffect(
    () => () => {
      detailSequence.current++;
    },
    [],
  );
  const pending = useRef(new Map<string, Promise<unknown>>());
  const request = useCallback(
    <K extends CommandName>(
      name: K,
      ...params: CommandParams<K>
    ): Promise<CommandResult<K>> => {
      const args = params[0] ?? {};
      const key = name + JSON.stringify(args);
      let promise = pending.current.get(key);
      if (!promise) {
        promise = command(name, ...params).finally(() => {
          if (pending.current.get(key) === promise) pending.current.delete(key);
        });
        pending.current.set(key, promise);
      }
      return promise as Promise<CommandResult<K>>;
    },
    [],
  );
  useEffect(() => {
    if (!visible || !nativeVisible) pending.current.clear();
  }, [visible, nativeVisible]);
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
      client: client || undefined,
      provider: provider || undefined,
      operation: operation || undefined,
      sessionId: sessionId || undefined,
    };
  }, [range, from, to, live, client, provider, operation, sessionId]);
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
      const overview = await request("get_usage_dashboard", {
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
      const rows = await request("get_usage_logs", {
        filter: { ...query(), status: status || undefined, page },
      });
      if (id === rowsSequence.current) setLogs(rows);
    } catch (e) {
      if (id === rowsSequence.current) setError(errorOf(e).message);
    } finally {
      if (id === rowsSequence.current) setRowsBusy(false);
    }
  }, [query, status, page, request]);
  const loadClientStats = useCallback(async () => {
    const id = ++agSequence.current;
    setAgBusy(true);
    try {
      const filter = query();
      if (
        filter.start != null &&
        (!Number.isFinite(filter.start) ||
          filter.start > (filter.end ?? Date.now()))
      )
        throw new Error("请选择有效时间范围");
      const stats = await request(
        tab === "claude"
          ? "get_claude_usage_stats"
          : "get_antigravity_usage_stats",
        { filter },
      );
      if (id === agSequence.current) {
        setAgStats(stats);
        setError("");
      }
    } catch (e) {
      if (id === agSequence.current) setError(errorOf(e).message);
    } finally {
      if (id === agSequence.current) setAgBusy(false);
    }
  }, [query, request, tab]);
  const loadClaudeQuota = useCallback(
    async (refresh = false) => {
      try {
        setClaudeQuota(await request("get_claude_usage_quota", { refresh }));
      } catch (error) {
        setClaudeQuota({
          account: null,
          plan: null,
          windows: [],
          fetchedAt: null,
          retryAt: null,
          error: errorOf(error).message,
        });
      }
    },
    [request],
  );
  const loadState = useCallback(
    async (fresh = false) => {
      if (fresh) pending.current.delete("get_usage_state{}");
      const id = ++stateSequence.current;
      try {
        const value = await request("get_usage_state");
        if (id === stateSequence.current) setState(value);
      } catch (e) {
        if (id === stateSequence.current) setError(errorOf(e).message);
      }
    },
    [request],
  );
  const loadData = useCallback(
    (fresh = false, refreshQuota = false) => {
      if (fresh) pending.current.clear();
      return Promise.all([
        ...(!independent ? [loadOverview()] : []),
        ...(tab === "requests" ? [loadRows()] : []),
        ...(independent ? [loadClientStats()] : []),
        ...(tab === "claude" ? [loadClaudeQuota(refreshQuota)] : []),
      ]);
    },
    [
      loadOverview,
      loadRows,
      loadClientStats,
      loadClaudeQuota,
      independent,
      tab,
    ],
  );
  const load = useCallback(
    (fresh = false) => Promise.all([loadData(fresh, fresh), loadState()]),
    [loadData, loadState],
  );
  useEffect(() => {
    if (visible && nativeVisible && !independent) void loadOverview();
    return () => {
      sequence.current++;
    };
  }, [loadOverview, visible, nativeVisible, independent]);
  useEffect(() => {
    if (visible && nativeVisible && tab === "requests") void loadRows();
    return () => {
      rowsSequence.current++;
    };
  }, [loadRows, visible, nativeVisible, tab]);
  useEffect(() => {
    if (!visible || !nativeVisible || !independent) return;
    void loadClientStats();
    return () => {
      agSequence.current++;
    };
  }, [loadClientStats, visible, nativeVisible, independent]);
  useEffect(() => {
    if (!visible || !nativeVisible || tab !== "claude") return;
    void loadClaudeQuota();
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("claude-login-state", (value) => {
      if (value.phase === "complete") void loadClaudeQuota(true);
    });
    return () => events.dispose();
  }, [visible, nativeVisible, tab, loadClaudeQuota]);
  const reloadAccountStats = useEffectEvent(() => {
    pending.current.clear();
    if (tab === "antigravity") void loadClientStats();
  });
  useEffect(() => {
    if (!visible || !nativeVisible) return;
    void loadState();
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("usage-state", () => void loadState(true));
    return () => {
      stateSequence.current++;
      events.dispose();
    };
  }, [loadState, visible, nativeVisible]);
  useEffect(() => {
    if (!visible || !nativeVisible) return;
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("pricing-state", () => void loadData(true));
    return () => events.dispose();
  }, [loadData, visible, nativeVisible]);
  useEffect(() => {
    if (!visible || !nativeVisible) return;
    let gone = false;
    const updateAccounts = (ag: AntigravityAuthState | null) => {
      const kind =
        ag?.mode === "vertex_key"
          ? "vertex_key"
          : ag?.enableAdc
            ? "adc"
            : "oauth";
      setAgCurrentAccount(
        ag?.accounts.find((a) => a.current && a.kind === kind)?.id ?? null,
      );
      const names = Object.fromEntries(
        (ag?.accounts ?? []).map((a) => [
          a.id,
          `${a.name} (${a.kind === "oauth" ? "OAuth" : a.kind === "adc" ? "ADC" : "Vertex Key"})`,
        ]),
      );
      setAgProviders(names);
      return names;
    };
    void Promise.all([
      command("get_gateway", { clientId: "codex" }),
      command("get_gateway", { clientId: "claude" }),
      command("get_gateway", { clientId: "grok" }),
      command("get_gateway", { clientId: "dsh" }),
      command("get_antigravity_auth_state").catch(() => null),
    ])
      .then(([codex, claude, grok, dsh, ag]) => {
        if (gone) return;
        const gw = Object.fromEntries(
          [codex, claude, grok, dsh].flatMap((s) =>
            s.providers.map((p) => [p.id, p.name]),
          ),
        );
        setClaudeProviders(
          Object.fromEntries(claude.providers.map((p) => [p.id, p.name])),
        );
        const agMap = updateAccounts(ag);
        setProviders({ ...gw, ...agMap });
      })
      .catch(() => {});
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("antigravity-auth-state", (ag) => {
      if (gone) return;
      const agMap = updateAccounts(ag);
      setProviders((prev) => ({ ...prev, ...agMap }));
      reloadAccountStats();
    });
    return () => {
      gone = true;
      sequence.current++;
      events.dispose();
    };
  }, [visible, nativeVisible]);
  useEffect(() => {
    const changed = () => setVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", changed);
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("app-visibility", setNativeVisible);
    return () => {
      document.removeEventListener("visibilitychange", changed);
      events.dispose();
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
  const t = data?.totals;
  const cacheRate = t?.cacheInputEligible
    ? (100 * (t.cacheReadEligible ?? 0)) / t.cacheInputEligible
    : null;
  const open = async (r: UsageRecord) => {
    const id = ++detailSequence.current;
    try {
      const detail = await command("get_usage_detail", {
        id: r.id,
      });
      if (id === detailSequence.current) setSelected(detail);
    } catch (e) {
      if (id === detailSequence.current) setError(errorOf(e).message);
    }
  };
  return (
    <section className="usage-page">
      <header className="usage-heading">
        <h1>用量</h1>
        <div className="usage-actions">
          <span className="usage-sync-status" aria-live="polite">
            {syncStatus(state, client)}
          </span>
          <select
            aria-label="用量刷新频率"
            value={state?.settings.refreshSeconds ?? 30}
            onChange={(e) => {
              if (state)
                void command("set_usage_settings", {
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
            disabled={busy || state?.syncing}
            onClick={() => {
              setBillResult("");
              setBillOpen(true);
            }}
          >
            <Download size={16} />
            导出账单
          </button>
          <button
            className="icon-button"
            title="数据来源"
            aria-label="数据来源"
            onClick={(e) => {
              e.currentTarget.focus();
              setSheet(true);
              if (independent) void loadOverview();
            }}
          >
            <Database size={17} />
          </button>
          <button
            className="icon-button"
            aria-label="刷新用量"
            disabled={independent ? agBusy : busy}
            onClick={() => void load(true)}
          >
            <RefreshCw size={17} />
          </button>
        </div>
      </header>
      {billResult && (
        <p role="status" className="usage-bill-result">
          {billResult}
        </p>
      )}
      {!independent && (
        <div className="usage-metrics">
          <Metric label="估算费用" value={money(t?.cost)} title={t?.cost} />
          <Metric
            label="已报告 Token"
            value={t ? compact(tokenTotal(t.tokens)) : "—"}
            title={t ? numeric(tokenTotal(t.tokens)) : undefined}
          />
          <Metric
            label="缓存命中率"
            value={cacheRate == null ? "—" : `${cacheRate.toFixed(1)}%`}
            title={
              cacheRate == null
                ? "无有效缓存样本"
                : `${numeric(t?.cacheReadEligible)} / ${numeric(t?.cacheInputEligible)}`
            }
          />
          <Metric
            label="平均首字"
            value={firstTokenLabel(averageFirstToken(t))}
            title={firstTokenTitle(t)}
          />
        </div>
      )}
      <div className="usage-filters">
        <select
          aria-label="用量客户端"
          value={client}
          disabled={independent}
          onChange={(e) => {
            const next = e.target.value;
            choose(setClient, next);
            setProvider("");
            setSessionId("");
            setSessionTitle("");
          }}
        >
          <option value="">全部客户端</option>
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
          <option value="grok">Grok Build</option>
          <option value="dsh">DeepSeek Harness</option>
          <option value="antigravity">Antigravity app</option>
        </select>
        <select
          aria-label="用量供应商"
          value={provider}
          onChange={(e) => choose(setProvider, e.target.value)}
        >
          <option value="">全部供应商</option>
          {Object.entries(
            client === "antigravity" || tab === "antigravity"
              ? agProviders
              : client === "claude"
                ? claudeProviders
                : providers,
          ).map(([id, n]) => (
            <option key={id} value={id}>
              {n}
            </option>
          ))}
          {(client === "antigravity" ||
            tab === "antigravity" ||
            client === "claude" ||
            provider === "session") && (
            <option value="session">本机会话（未归属）</option>
          )}
        </select>
        <select
          aria-label="请求类型"
          value={operation}
          onChange={(e) => choose(setOperation, e.target.value)}
        >
          <option value="">全部类型</option>
          <option value="model">生成</option>
          <option value="compaction">压缩</option>
          <option value="web_search">搜索</option>
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
        {sessionId && (
          <button
            type="button"
            className="usage-session-chip"
            aria-label={`清除会话筛选 ${sessionTitle}`}
            onClick={() => {
              setSessionId("");
              resetPages();
            }}
          >
            <span>{sessionTitle || "未命名会话"}</span>
            <span aria-hidden="true">×</span>
          </button>
        )}
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
      <div className="usage-tabs" role="tablist" aria-label="用量明细">
        {[
          ["requests", "请求日志"],
          ["providers", "供应商"],
          ["models", "模型"],
          ["antigravity", "Antigravity"],
          ["claude", "Claude Code"],
          ["pricing", "定价"],
        ].map(([id, n]) => (
          <button
            role="tab"
            data-tab={id}
            aria-selected={tab === id}
            key={id}
            onClick={() => void selectTab(id)}
          >
            {n}
          </button>
        ))}
        {tab === "requests" && (
          <div className="usage-toolbar" role="group" aria-label="请求日志工具">
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
            <UsageLogControls
              settings={state?.settings}
              save={(settings) => {
                void command("set_usage_settings", { settings })
                  .then(setState)
                  .catch((e) => setError(errorOf(e).message));
              }}
            />
          </div>
        )}
      </div>
      {tab === "requests" ? (
        <>
          <UsageLogTable
            rows={logs.rows}
            busy={rowsBusy}
            settings={state?.settings}
            name={name}
            open={(r) => void open(r)}
          />
          <Pagination page={logs.page} total={logs.total} onChange={setPage} />
        </>
      ) : independent ? (
        <>
          {tab === "claude" && <UsageClaudeQuota quota={claudeQuota} />}
          <UsageAntigravity
            client={tab as "claude" | "antigravity"}
            stats={agStats}
            busy={agBusy}
            currentAccountId={agCurrentAccount}
            selectedProvider={provider}
            providerName={name}
            onSelectProvider={(nextProvider) => {
              if (nextProvider) setClient(tab);
              setProvider(nextProvider);
              resetPages();
            }}
            onSelectSession={(nextSessionId, title) => {
              setSessionTitle(title);
              setClient(tab);
              setSessionId(nextSessionId);
              resetPages();
              setTab("requests");
            }}
          />
        </>
      ) : tab === "pricing" ? (
        <Pricing onDirtyChange={priceDirty} />
      ) : (
        <Ranking
          groups={
            tab === "providers"
              ? (data?.providers ?? [])
              : (data?.models ?? []).filter(
                  ({ totals }) =>
                    totals.success > 0 ||
                    Object.values(totals.tokens).some(
                      (value) => (value ?? 0) > 0,
                    ) ||
                    Number(totals.cost) !== 0,
                )
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
          onClose={() => {
            detailSequence.current++;
            setSelected(null);
          }}
        />
      )}
      {billOpen && (
        <UsageBill
          start={query().start ?? (data?.detailSince || undefined)}
          end={query().end}
          close={() => setBillOpen(false)}
          saved={(result) => {
            setBillOpen(false);
            setBillResult(
              `已导出 ${numeric(result.requests)} 个请求、${numeric(result.attempts)} 次尝试，清理 ${numeric(result.cleaned)} 条已导出记录与对应统计${result.retained ? `，保留 ${numeric(result.retained)} 条更新记录` : ""}。${result.cleanupError ?? ""}`,
            );
            void load(true);
          }}
        />
      )}
      {sheet && state && (
        <Sources
          state={state}
          counts={data?.sources ?? {}}
          historyIncomplete={data?.sourceHistoryIncomplete ?? false}
          onClose={() => setSheet(false)}
          onChange={(s) => {
            stateSequence.current++;
            setState(s);
            void loadData(true);
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
function NumberValue({
  value,
  missing = "历史缺失",
}: {
  value: number | null | undefined;
  missing?: string;
}) {
  const [focused, setFocused] = useState(false);
  return (
    <span
      className="usage-number"
      tabIndex={0}
      title={value == null ? missing : numeric(value)}
      aria-label={value == null ? missing : numeric(value)}
      onFocus={() => setFocused(true)}
      onBlur={() => setFocused(false)}
    >
      {value == null ? "—" : focused ? numeric(value) : compact(value)}
    </span>
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
  const metricOf = (g: Group) =>
    view.sort === "cost"
      ? Number(g.totals.cost)
      : view.sort === "tokens"
        ? (tokenTotal(g.totals.tokens) ?? 0)
        : view.sort === "firstToken"
          ? (averageFirstToken(g.totals) ?? 0)
          : g.totals.requests;
  const maxMetric = sorted.reduce(
    (max, g) => Math.max(max, metricOf(g) || 0),
    0,
  );
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
            {sorted.slice((page - 1) * 20, page * 20).map((group) => {
              const { id, totals: t } = group;
              const ratio =
                maxMetric > 0
                  ? Math.min(
                      100,
                      Math.max(2, (metricOf(group) / maxMetric) * 100),
                    )
                  : 0;
              return (
                <tr key={id}>
                  <td title={name(id)}>
                    <div className="usage-rank-cell">
                      <span>{name(id)}</span>
                      <i className="usage-rank-bar" aria-hidden="true">
                        <b style={{ width: `${ratio}%` }} />
                      </i>
                    </div>
                  </td>
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
              );
            })}
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
