import FailureRuleDialog from "./FailureRuleDialog";
import type { ErrorResponse } from "./failure-policy-types";
import {
  limitLabel,
  rpmWaitLabel,
  quotaCheckLabel,
  quotaBasisLabel,
  type QuotaCheck,
} from "./limit-labels";
import type { RoutingDetails } from "./usage-types";
import {
  routingReason,
  routingInput,
  routingIdentity,
  routingBoundary,
  routingRestriction,
  routingOutcome,
  routingFailure,
} from "./routing-labels";
import { subscriptions } from "./subscriptions";
import { useVisible } from "./visibility";
import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, RefreshCw, Trash2, X } from "lucide-react";
import { command } from "./bridge";
import { confirmAction } from "./confirmation";
import { errorOf, type ClientId, type GatewayState } from "./types";

export const reasons = {
  model_unavailable: "模型不支持",
  authentication: "认证失败",
  upstream_service: "上游服务异常",
  network: "连接失败",
  rate_limit: "上游限流",
  capacity: "容量不足",
  failover_exhausted: "请求未完成",
  failover: "自动换商",
  circuit_open: "供应商熔断",
  thread_handoff: "线程已接管",
  thread_routing: "线程路由结果",
  config_conflict: "配置冲突",
  startup_recovery: "启动恢复失败",
  account_sync: "账号同步异常",
  protocol_error: "返回了应用错误",
  request_interrupted: "请求交付提前结束",
} as const;

/** Events the summary names by what happened; their records may carry a narrower code. */
const eventCodes: Partial<Record<keyof typeof reasons, string>> = {
  failover_exhausted: "FAILOVER_EXHAUSTED",
  failover: "FAILOVER",
  circuit_open: "CIRCUIT_OPEN",
  thread_handoff: "THREAD_HANDOFF",
  thread_routing: "THREAD_ROUTING",
};

const actions = {
  trying_next: "尝试下一供应商",
  returned: "已反馈客户端",
  waiting: "等待后重试",
  stopped: "需要处理",
  routed: "已切换供应商",
  reconnecting: "等待后重新连接",
  retrying_same: "原供应商等待重试",
  not_retried: "响应已开始，未重试",
} as const;

const categories = {
  ordinary_error: "普通错误",
  circuit_error: "熔断错误",
  status: "状态记录",
} as const;

export type EventOrigin = "upstream" | "gateway" | "antigravity";

export type EventRecord = {
  id: string;
  origin: EventOrigin;
  firstAt: number;
  lastAt: number;
  count: number;
  clientId: ClientId | "agy" | "antigravity" | null;
  providerId: string | null;
  model: string | null;
  reason: keyof typeof reasons;
  errorCode: string;
  action: keyof typeof actions;
  level: "warning" | "error" | "info";
  category: keyof typeof categories;
  status: number | null;
  attempt: number | null;
  details?: {
    rawError?: {
      id: string;
      digest: string;
      incomplete?: string | null;
    } | null;
    customRuleId?: string | null;
    ruleConflict?: boolean | null;
    routing?: RoutingDetails | null;
    errorBasis?: string | null;
    quotaCheck?: QuotaCheck | null;
    rpmWaitReason?: string | null;
    healthScope?: "provider" | "model" | "request" | null;
    upstreamCode?: string | null;
    upstreamReason?: string | null;
    localCode?: string | null;
    upstreamType?: string | null;
    parameter?: string | null;
    message?: string | null;
    phase?: string | null;
    wsCloseCode?: number | null;
    countedFailure?: boolean | null;
    waitSeconds?: number | null;
    causeId?: string | null;
    timeoutPhase?: string | null;
    timeoutSeconds?: number | null;
    lastProgressMs?: number | null;
    delivery?: {
      durationMs: number;
      upstreamBytes: number;
      handedBytes: number;
      terminalReceived: boolean;
      terminalHandedOff: boolean;
      requestHash: string | null;
      responseHash: string | null;
    } | null;
    circuit?: {
      causeReason?: keyof typeof reasons | null;
      retryAt?: number | null;
      failures: number;
      failureThreshold: number;
      failedRequests: number;
      requests: number;
      trigger: string;
    } | null;
  };
};

export type EventPage = {
  items: EventRecord[];
  total: number;
  page: number;
  error: string | null;
};

type StatusGroup =
  | ""
  | "success"
  | "client_error"
  | "server_error"
  | "no_status";

const client = (id: ClientId | "agy" | "antigravity" | null) =>
  id === "antigravity"
    ? "Antigravity app"
    : id === "agy"
      ? "Antigravity CLI"
      : id === "codex"
        ? "Codex"
        : id === "claude"
          ? "Claude Code"
          : id === "dsh"
            ? "DSH"
            : id === "grok"
              ? "Grok Build"
              : "应用";

const time = (seconds: number) =>
  new Date(seconds * 1000).toLocaleString("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
    hour12: false,
  });

function displayName(name: string, id: string) {
  return name.length <= 120 &&
    !/(@|:\/\/|sk-|eyJ|Bearer|[\\/]|[\u0000-\u001f]|\b(?:\d{1,3}\.){3}\d{1,3}\b)/i.test(
      name,
    )
    ? name
    : "供应商 " + id.slice(0, 8);
}

function statusText(status: number | null) {
  return status === null ? "—" : String(status);
}

function statusTone(status: number | null) {
  if (status === null) return "none";
  if (status >= 200 && status < 300) return "success";
  if (status >= 400 && status < 500) return "client";
  if (status >= 500) return "server";
  return "other";
}

const phases: Record<string, string> = {
  admission: "准入",
  connect: "建立连接",
  headers: "等待响应头",
  response: "读取响应",
  stream: "流式响应",
  ws_handshake: "WebSocket 握手",
  ws_send: "WebSocket 发送",
  ws_receive: "WebSocket 接收",
};
const timeoutPhases: Record<string, string> = {
  response_headers: "等待响应头",
  first_byte: "等待首字节",
  first_event_idle: "首事件接收静默",
  error_body_idle: "错误响应接收静默",
  stream_idle: "上游流静默",
  downstream_write: "下游发送无进展",
  ws_idle: "WebSocket 接收静默",
  ws_first_byte: "WebSocket 等待首字节",
  ws_handshake: "WebSocket 握手",
};
function reasonText(r: EventRecord) {
  if (
    r.details?.delivery?.terminalReceived &&
    !r.details.delivery.terminalHandedOff &&
    r.reason === "request_interrupted"
  )
    return "上游已结束，下游未完成交付";
  if (r.reason === "circuit_open") {
    if (r.details?.healthScope === "model") return "模型熔断";
    return r.details?.healthScope === "provider" ? "供应商熔断" : "熔断";
  }
  if (r.origin === "upstream" || r.details?.quotaCheck) {
    const limit = limitLabel(r.details);
    if (limit) return limit;
  }
  return reasons[r.reason];
}
function circuitCause(r: EventRecord) {
  return (
    [
      r.status != null ? statusText(r.status) : null,
      r.details?.upstreamCode ?? r.details?.localCode,
      r.details?.upstreamReason,
      r.details?.message ||
        (r.details?.circuit?.causeReason
          ? reasons[r.details.circuit.causeReason]
          : null),
    ]
      .filter(
        (value, index, values) =>
          Boolean(value) && values.indexOf(value) === index,
      )
      .join(" · ") || "原因未记录"
  );
}
function circuitTrigger(r: EventRecord) {
  const c = r.details?.circuit;
  if (!c) return "触发条件未记录";
  switch (c.trigger) {
    case "transient_failures":
    case "consecutive_failures":
      return `连续失败 ${c.failures}/${c.failureThreshold}`;
    case "probe_failed":
      return "恢复探测失败";
    default:
      return "触发条件未记录";
  }
}
function primaryCode(r: EventRecord) {
  if (r.origin === "gateway")
    return eventCodes[r.reason] || r.details?.localCode || r.errorCode;
  return (
    r.details?.upstreamCode ??
    r.details?.upstreamReason ??
    r.details?.upstreamType ??
    r.details?.localCode ??
    r.errorCode
  );
}
function summaryMessage(r: EventRecord) {
  if (r.reason === "circuit_open")
    return [reasonText(r), circuitTrigger(r)].filter(Boolean).join(" · ");
  if (r.reason === "thread_routing" && r.details?.routing)
    return [reasonText(r), routingOutcome(r.details.routing.outcome)]
      .filter(Boolean)
      .join(" · ");
  if (r.origin === "gateway") return reasonText(r);
  return [
    r.details?.message || reasonText(r),
    r.details?.upstreamReason !== primaryCode(r)
      ? r.details?.upstreamReason
      : null,
    r.details?.quotaCheck ? quotaCheckLabel(r.details.quotaCheck) : null,
  ]
    .filter((v, i, all) => v && all.indexOf(v) === i)
    .join(" · ");
}
function summary(r: EventRecord) {
  return [primaryCode(r), summaryMessage(r)]
    .filter((v, i, all) => v && all.indexOf(v) === i)
    .join(" · ");
}
function actionText(r: EventRecord) {
  if (r.reason === "circuit_open")
    return r.details?.circuit?.retryAt != null && r.details.waitSeconds != null
      ? `等待 ${r.details.waitSeconds} 秒后可探测`
      : "等待时间未记录";
  const prefix = r.details?.countedFailure === false ? "未计入熔断，" : "";
  const wait = r.details?.waitSeconds;
  if (wait && ["trying_next", "returned", "stopped"].includes(r.action))
    return `${prefix}${actions[r.action]} · 当前${r.details?.healthScope === "model" ? "模型" : "供应商"}冷却 ${wait} 秒`;
  return `${prefix}${wait ? `${r.action === "retrying_same" ? "原供应商" : ""}等待 ${wait} 秒${r.action === "reconnecting" ? "重连" : "重试"}` : actions[r.action]}`;
}
function wireStatus(r: EventRecord) {
  return (
    [
      r.status == null ? null : statusText(r.status),
      r.details?.wsCloseCode ? `WS ${r.details.wsCloseCode}` : null,
    ]
      .filter(Boolean)
      .join(" · ") || "—"
  );
}

export default function EventLog({
  focus,
  consumed,
}: {
  focus?: {
    id: string;
    clientId: ClientId;
    providerId: string | null;
    sequence: number;
  } | null;
  consumed?: () => void;
} = {}) {
  const [data, setData] = useState<EventPage>({
    items: [],
    total: 0,
    page: 1,
    error: null,
  });
  const [filters, setFilters] = useState({
    clientId: "",
    providerId: "",
    level: "",
    category: "",
    reason: "",
    statusGroup: "" as StatusGroup,
    from: "",
    to: "",
  });
  const [origin, setOrigin] = useState<EventOrigin>("upstream");
  const [page, setPage] = useState(1);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [providers, setProviders] = useState<
    Record<string, { name: string; clientId: ClientId }>
  >({});
  const [selected, setSelected] = useState<EventRecord | null>(null);
  const visible = useVisible("app-visibility", (message) => setError(message));
  const sequence = useRef(0);
  const flight = useRef<{ key: string; promise: Promise<EventPage> } | null>(
    null,
  );
  const detailSequence = useRef(0);
  useEffect(
    () => () => {
      detailSequence.current++;
    },
    [],
  );

  useEffect(() => {
    if (!focus) return;
    const current = focus;
    const request = ++detailSequence.current;
    consumed?.();
    setFilters((old) => ({
      ...old,
      clientId: current.clientId,
      providerId: current.providerId ?? "",
      reason: "",
      category: "",
      level: "",
      statusGroup: "",
    }));
    setPage(1);
    void command("get_app_event", { id: current.id })
      .then((record) => {
        if (record && request === detailSequence.current) {
          setOrigin(record.origin);
          setSelected(record);
        }
      })
      .catch((e) => {
        if (request === detailSequence.current) setError(errorOf(e).message);
      });
  }, [focus?.sequence]);

  const load = useCallback(async (fresh = false) => {
    if (fresh) flight.current = null;
    const request = ++sequence.current;
    try {
      const filter = Object.fromEntries(
        Object.entries(filters).filter(([, value]) => value),
      );
      const args = {
        filter: {
          ...filter,
          origin,
          page,
          from: filters.from
            ? Math.floor(new Date(filters.from).getTime() / 1000)
            : null,
          to: filters.to
            ? Math.floor(new Date(filters.to).getTime() / 1000)
            : null,
        },
      };
      const key = JSON.stringify(args);
      const promise =
        flight.current?.key === key
          ? flight.current.promise
          : command("get_app_events", args);
      flight.current = { key, promise };
      const result = await promise.finally(() => {
        if (flight.current?.promise === promise) flight.current = null;
      });
      if (request === sequence.current) {
        setData(result);
        setError("");
      }
    } catch (e) {
      if (request === sequence.current) setError(errorOf(e).message);
    }
  }, [filters, page, origin]);

  useEffect(() => {
    if (visible) void load(true);
    else flight.current = null;
    return () => {
      sequence.current++;
    };
  }, [visible, load]);

  useEffect(() => {
    if (!visible || origin !== "antigravity") return;
    const timer = window.setInterval(() => void load(), 5_000);
    return () => window.clearInterval(timer);
  }, [visible, origin, load]);

  useEffect(() => {
    let disposed = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    const accept = (s: GatewayState) =>
      setProviders((old) => {
        const next = { ...old };
        for (const key of Object.keys(next))
          if (next[key].clientId === s.clientId) delete next[key];
        for (const p of s.providers)
          next[s.clientId + ":" + p.id] = {
            name: displayName(p.name, p.id),
            clientId: s.clientId,
          };
        return next;
      });

    const received = new Set<ClientId>();
    for (const id of ["codex", "claude", "grok", "dsh"] as const)
      void command("get_gateway", { clientId: id })
        .then((s) => {
          if (!disposed && !received.has(id)) accept(s);
        })
        .catch(() => {});

    const watch = events.watch;

    watch("gateway-state", (s) => {
      received.add(s.clientId);
      accept(s);
    });

    return () => {
      disposed = true;
      events.dispose();
    };
  }, []);

  useEffect(() => {
    const events = subscriptions((e) => setError(errorOf(e).message));
    let timer: number | undefined;
    events.watch("app-event", () => {
      if (visible && timer == null) {
        timer = window.setTimeout(() => {
          timer = undefined;
          void load(true);
        }, 250);
      }
    });

    return () => {
      window.clearTimeout(timer);
      events.dispose();
    };
  }, [visible, load]);

  const change = (key: keyof typeof filters, value: string) => {
    setFilters((f) => ({
      ...f,
      [key]: value,
      ...(key === "clientId" ? { providerId: "" } : {}),
    }));
    setPage(1);
  };

  const name = (r: EventRecord) =>
    r.providerId
      ? providers[r.clientId + ":" + r.providerId]?.name ||
        "供应商 " + r.providerId.slice(0, 8)
      : "系统";

  const providerOptions = Object.entries(providers)
    .filter(
      ([, provider]) =>
        !filters.clientId || provider.clientId === filters.clientId,
    )
    .reduce<{ id: string; name: string }[]>((items, [key, provider]) => {
      const id = key.split(":").slice(1).join(":");
      if (!items.some((item) => item.id === id))
        items.push({ id, name: provider.name });
      return items;
    }, []);

  const knownProviderIds = new Set(
    providerOptions.map((provider) => provider.id),
  );
  const unknownProviderIds = [
    ...new Set(
      data.items
        .map((record) => record.providerId)
        .filter((id): id is string => id !== null)
        .filter((id) => !knownProviderIds.has(id)),
    ),
  ];

  const clear = async () => {
    if (!(await confirmAction("清空全部日志（上游报错与网关内部）？"))) return;
    setBusy(true);
    try {
      await command("clear_app_events");
      flight.current = null;
      sequence.current++;
      detailSequence.current++;
      setSelected(null);
      if (page !== 1) setPage(1);
      else await load(true);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section className="event-page">
      <div className="page-heading">
        <h1>日志</h1>
        <div className="event-actions">
          <button
            className="icon-button"
            aria-label="刷新日志"
            onClick={() => void load(true)}
          >
            <RefreshCw size={16} />
          </button>
          <button
            className="icon-button"
            aria-label="清空全部日志"
            disabled={busy}
            onClick={() => void clear()}
          >
            <Trash2 size={16} />
          </button>
        </div>
      </div>
      <div className="policy-tabs" role="tablist" aria-label="日志来源">
        {(["upstream", "gateway", "antigravity"] as const).map((value) => (
          <button
            type="button"
            role="tab"
            key={value}
            data-origin={value}
            aria-selected={origin === value}
            onClick={() => {
              setOrigin(value);
              setPage(1);
              setFilters((old) => ({
                ...old,
                ...(value === "antigravity" ? { clientId: "", providerId: "" } : {}),
                reason: "",
                category: "",
                level: "",
                statusGroup: "",
              }));
              detailSequence.current++;
              setSelected(null);
            }}
          >
            {value === "upstream"
              ? "上游报错"
              : value === "gateway"
                ? "网关内部"
                : "Antigravity app"}
          </button>
        ))}
      </div>
      <div className="event-filters">
        <label>
          开始
          <input
            type="datetime-local"
            aria-label="开始时间"
            value={filters.from}
            onChange={(e) => change("from", e.target.value)}
          />
        </label>
        <label>
          结束
          <input
            type="datetime-local"
            aria-label="结束时间"
            value={filters.to}
            onChange={(e) => change("to", e.target.value)}
          />
        </label>
        <select
          aria-label="客户端"
          value={filters.clientId}
          disabled={origin === "antigravity"}
          onChange={(e) => change("clientId", e.target.value)}
        >
          <option value="">全部客户端</option>
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
          <option value="grok">Grok Build</option>
          <option value="dsh">DeepSeek Harness</option>
        </select>
        <select
          aria-label="供应商"
          value={filters.providerId}
          disabled={origin === "antigravity"}
          onChange={(e) => change("providerId", e.target.value)}
        >
          <option value="">全部供应商</option>
          {providerOptions.map((provider) => (
            <option key={provider.id} value={provider.id}>
              {provider.name}
            </option>
          ))}
          {unknownProviderIds.map((id) => (
            <option key={id} value={id}>
              供应商 {id.slice(0, 8)}
            </option>
          ))}
        </select>
        <select
          aria-label="状态码"
          value={filters.statusGroup}
          onChange={(e) => change("statusGroup", e.target.value)}
        >
          <option value="">全部状态</option>
          <option value="success">2xx</option>
          <option value="client_error">4xx</option>
          <option value="server_error">5xx</option>
          <option value="no_status">无状态码</option>
        </select>
        <select
          aria-label="类别"
          value={filters.category}
          onChange={(e) => change("category", e.target.value)}
        >
          <option value="">全部类别</option>
          {Object.entries(categories).map(([key, label]) => (
            <option key={key} value={key}>
              {label}
            </option>
          ))}
        </select>
        <select
          aria-label="级别"
          value={filters.level}
          onChange={(e) => change("level", e.target.value)}
        >
          <option value="">全部级别</option>
          <option value="warning">警告</option>
          <option value="error">错误</option>
          <option value="info">状态</option>
        </select>
        <select
          aria-label="原因"
          value={filters.reason}
          onChange={(e) => change("reason", e.target.value)}
        >
          <option value="">全部原因</option>
          {Object.entries(reasons).map(([key, label]) => (
            <option key={key} value={key}>
              {label}
            </option>
          ))}
        </select>
      </div>
      {(error || data.error) && (
        <div className="form-error" role="alert">
          {error || data.error}
        </div>
      )}
      <div className="event-table-wrap">
        <table className="event-table">
          <thead>
            <tr>
              <th>时间</th>
              <th>客户端 / 供应商</th>
              <th>状态</th>
              <th>错误摘要</th>
              <th>处理结果</th>
            </tr>
          </thead>
          <tbody>
            {data.items.map((r) => {
              const cat = r.category;
              return (
                <tr key={r.id} data-category={cat}>
                  <td>
                    <time title={new Date(r.lastAt * 1000).toISOString()}>
                      {time(r.lastAt)
                        .split(" ")
                        .map((part, i) => (
                          <span key={i}>{part}</span>
                        ))}
                    </time>
                  </td>
                  <td>
                    <span data-client={r.clientId ?? undefined}>
                      {client(r.clientId)}
                    </span>
                    <strong title={name(r)}>{name(r)}</strong>
                  </td>
                  <td>
                    <span
                      className={
                        "event-status " +
                        (r.details?.errorBasis || r.level !== "info"
                          ? r.level === "error"
                            ? "server"
                            : "client"
                          : statusTone(r.status))
                      }
                      data-ws={r.details?.wsCloseCode != null ? "true" : undefined}
                      title={wireStatus(r)}
                    >
                      {r.status != null && <span>{statusText(r.status)}</span>}
                      {r.details?.wsCloseCode != null && (
                        <span>WS {r.details.wsCloseCode}</span>
                      )}
                      {r.status == null &&
                        r.details?.wsCloseCode == null &&
                        "—"}
                    </span>
                  </td>
                  <td>
                    <div className="event-error-cell">
                      <button
                        className={"event-code " + r.level}
                        data-category={cat}
                        aria-label={reasonText(r)}
                        title={summary(r)}
                        onClick={(e) => {
                          e.currentTarget.focus({ preventScroll: true });
                          const request = ++detailSequence.current;
                          void command("get_app_event", {
                            id: r.id,
                          })
                            .then((value) => {
                              if (request === detailSequence.current)
                                setSelected(value || r);
                            })
                            .catch((e) => {
                              if (request === detailSequence.current)
                                setError(errorOf(e).message);
                            });
                        }}
                      >
                        <span className="event-summary-text">
                          <code className="event-primary-code">
                            {primaryCode(r)}
                          </code>
                          <span className="event-message-text">
                            {summaryMessage(r)}
                          </span>
                        </span>
                        {r.count > 1 && (
                          <span className="event-count">
                            ×{r.count.toLocaleString()}
                          </span>
                        )}
                      </button>
                    </div>
                  </td>
                  <td>
                    <span className="event-action-text" title={actionText(r)}>
                      {actionText(r)}
                    </span>
                  </td>
                </tr>
                );
              })}
          </tbody>
        </table>
        {data.items.length === 0 && <div className="event-empty">暂无日志</div>}
      </div>
      <div className="event-pagination">
        <span>{data.total.toLocaleString()} 条</span>
        <button
          className="icon-button"
          aria-label="上一页"
          disabled={data.page <= 1}
          onClick={() => setPage(data.page - 1)}
        >
          <ChevronLeft size={16} />
        </button>
        <span>
          {data.page} / {Math.max(1, Math.ceil(data.total / 50))}
        </span>
        <button
          className="icon-button"
          aria-label="下一页"
          disabled={data.page * 50 >= data.total}
          onClick={() => setPage(data.page + 1)}
        >
          <ChevronRight size={16} />
        </button>
      </div>
      {selected && (
        <EventDetail
          event={selected}
          provider={name(selected)}
          onClose={() => {
            detailSequence.current++;
            setSelected(null);
          }}
        />
      )}
    </section>
  );
}

function EventDetail({
  event: r,
  provider,
  onClose,
}: {
  event: EventRecord;
  provider: string;
  onClose: () => void;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const [raw, setRaw] = useState<ErrorResponse | null>(null);
  const [rawError, setRawError] = useState("");
  useEffect(() => {
    let active = true;
    setRaw(null);
    setRawError("");
    if (r.details?.rawError)
      void command("get_app_event_raw", { id: r.id })
        .then((value) => {
          if (active) setRaw(value);
        })
        .catch((error) => {
          if (active) setRawError(errorOf(error).message);
        });
    return () => {
      active = false;
    };
  }, [r.id, r.details?.rawError]);
  const [addingRule, setAddingRule] = useState(false);
  useEffect(() => {
    const focus = document.activeElement as HTMLElement | null;
    const dialog = ref.current;
    dialog?.showModal();
    heading.current?.focus({ preventScroll: true });
    return () => {
      dialog?.close();
      queueMicrotask(() => {
        if (!dialog?.isConnected && focus?.isConnected)
          focus.focus({ preventScroll: true });
      });
    };
  }, []);

  const eventCategory = r.category;
  return (
    <dialog
      ref={ref}
      className="event-drawer"
      data-category={eventCategory}
      aria-labelledby="event-title"
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === ref.current) onClose();
      }}
    >
      <div className="event-drawer-inner">
        {addingRule &&
          r.clientId &&
          r.clientId !== "agy" &&
          r.clientId !== "antigravity" && (
          <FailureRuleDialog
            clientId={r.clientId}
            initial={{
              value: r.details?.upstreamCode ?? "",
              matchKind: r.details?.upstreamCode ? "code" : "keyword",
              category: r.details?.errorBasis ?? "",
            }}
            close={() => setAddingRule(false)}
          />
        )}
        <header>
          <h2 ref={heading} id="event-title" tabIndex={-1}>
            {reasonText(r)}
          </h2>
          <button
            className="icon-button"
            aria-label="关闭详情"
            onClick={onClose}
          >
            <X size={18} />
          </button>
        </header>
        <div className="event-rule-actions">
          {r.origin === "upstream" &&
            r.clientId &&
            (r.details?.upstreamCode ||
              r.details?.message ||
              r.details?.upstreamType) && (
              <button
                className="secondary event-add-rule"
                onClick={() => setAddingRule(true)}
              >
                加入故障规则
              </button>
            )}
        </div>
        {rawError && (
          <div role="alert" className="form-error">
            {rawError}
          </div>
        )}
        {r.details?.rawError?.incomplete && (
          <div role="alert">{r.details.rawError.incomplete}</div>
        )}
        {raw && (
          <section aria-label="完整错误响应" className="event-raw">
            <h3>完整错误响应{raw.base64 ? "（Base64）" : ""}</h3>
            {raw.incomplete && <div role="alert">{raw.incomplete}</div>}
            <button
              type="button"
              className="secondary"
              onClick={() => {
                const headers = raw.headers
                  .map((header) => `${header.name}: ${header.value}`)
                  .join("\n");
                void navigator.clipboard
                  .writeText(`${headers}${headers ? "\n\n" : ""}${raw.body}`)
                  .catch((error) => setRawError(errorOf(error).message));
              }}
            >
              复制完整错误
            </button>
            {!!raw.headers.length && (
              <details>
                <summary>响应头</summary>
                <pre>
                  {raw.headers
                    .map((header) => `${header.name}: ${header.value}`)
                    .join("\n")}
                </pre>
              </details>
            )}
            {raw.event && <p>事件：{raw.event}</p>}
            <pre>{raw.body}</pre>
          </section>
        )}
        <dl>
          {[
            ["类别", categories[r.category]],
            ["HTTP 状态", wireStatus(r)],
            [
              "影响范围",
              r.details?.healthScope === "model"
                ? `当前模型 · ${r.model ?? "—"}`
                : r.details?.healthScope === "provider"
                  ? "整个供应商"
                  : r.details?.healthScope === "request"
                    ? "仅此请求"
                    : "历史未记录",
            ],
            ["上游错误码", r.details?.upstreamCode ?? "—"],
            ["上游原因", r.details?.upstreamReason ?? "未提供"],
            [
              "网关分类",
              limitLabel(r.details) ||
                (r.details?.errorBasis ? "未识别分类" : reasonText(r)),
            ],
            ...(r.details?.rpmWaitReason
              ? [["RPM 等待原因", rpmWaitLabel(r.details.rpmWaitReason)]]
              : []),
            ...(r.details?.quotaCheck
              ? [
                  ["额度核验", quotaCheckLabel(r.details.quotaCheck)],
                  [
                    "查询来源",
                    (
                      { sub2_api: "Sub2API", new_api: "new-api" } as Record<
                        string,
                        string
                      >
                    )[r.details.quotaCheck.source] ?? "未识别来源",
                  ],
                  ["查询依据", quotaBasisLabel(r.details.quotaCheck.basis)],
                  ["检查时间", time(r.details.quotaCheck.checkedAt)],
                ]
              : []),
            ["网关诊断码", r.details?.localCode ?? r.errorCode],
            ["上游错误类型", r.details?.upstreamType ?? "未提供"],
            ["错误摘要", r.details?.message ?? reasonText(r)],
            ["参数", r.details?.parameter ?? "未提供"],
            ["阶段", phases[r.details?.phase ?? ""] ?? "未提供"],
            [
              "计入熔断",
              r.details?.countedFailure == null
                ? "历史未记录"
                : r.details.countedFailure
                  ? "是"
                  : "否",
            ],
            ["客户端", client(r.clientId)],
            ["供应商", provider],
            ["处理结果", actionText(r)],
            ...(r.details?.customRuleId ? [["分类依据", "自定义规则"]] : []),
            ...(r.details?.ruleConflict
              ? [["规则冲突", "多个规则指向不同类别，保留原分类"]]
              : []),
            ...(r.reason === "circuit_open"
              ? [
                  ["熔断原因", circuitCause(r)],
                  ["触发条件", circuitTrigger(r)],
                  [
                    "发生时等待",
                    r.details?.circuit?.retryAt != null &&
                    r.details.waitSeconds != null
                      ? `${r.details.waitSeconds} 秒`
                      : "未记录",
                  ],
                  [
                    "预计探测时间",
                    r.details?.circuit?.retryAt != null
                      ? time(r.details.circuit.retryAt)
                      : "未记录",
                  ],
                  ["触发事件", r.details?.causeId ?? "未记录"],
                ]
              : []),
            ...(r.details?.timeoutPhase
              ? [
                  [
                    "超时阶段",
                    timeoutPhases[r.details.timeoutPhase] ??
                      r.details.timeoutPhase,
                  ],
                ]
              : []),
            ...(r.details?.timeoutSeconds != null
              ? [["配置阈值", `${r.details.timeoutSeconds} 秒`]]
              : []),
            ...(r.details?.lastProgressMs != null
              ? [["最后上游进展距结束", `${r.details.lastProgressMs} 毫秒`]]
              : []),
            ...(r.details?.circuit
              ? [
                  [
                    "连续失败",
                    `${r.details.circuit.failures} / ${r.details.circuit.failureThreshold}`,
                  ],
                  [
                    "失败请求",
                    `${r.details.circuit.failedRequests} / ${r.details.circuit.requests}`,
                  ],
                ]
              : []),
            ...(r.reason !== "circuit_open" && r.details?.causeId
              ? [["触发事件", r.details.causeId]]
              : []),
            ["模型", r.model || "未提供"],
            ["尝试次数", r.attempt ?? "—"],
            ...(r.details?.routing
              ? [
                  ["线程调度", routingReason(r.details.routing.decision)],
                  ["路由结果", routingOutcome(r.details.routing.outcome)],
                  ["输入证据", routingInput(r.details.routing.evidence.input)],
                  ...(r.details.routing.evidence.identity
                    ? [
                        [
                          "线程身份",
                          routingIdentity(r.details.routing.evidence.identity),
                        ],
                      ]
                    : []),
                  ...(r.details.routing.evidence.boundary
                    ? [
                        [
                          "边界状态",
                          routingBoundary(r.details.routing.evidence.boundary),
                        ],
                      ]
                    : []),
                  ...(r.details.routing.evidence.restriction
                    ? [
                        [
                          "准入限制",
                          routingRestriction(
                            r.details.routing.evidence.restriction,
                          ),
                        ],
                      ]
                    : []),
                  ...(r.details.routing.failure
                    ? [
                        [
                          "故障转移依据",
                          routingFailure(r.details.routing.failure),
                        ],
                      ]
                    : []),
                  ...(r.details.routing.attemptId
                    ? [["关联尝试", r.details.routing.attemptId]]
                    : []),
                ]
              : []),
            ...(r.details?.delivery
              ? [
                  ["请求持续", `${r.details.delivery.durationMs} 毫秒`],
                  ["上游已读取", `${r.details.delivery.upstreamBytes} 字节`],
                  [
                    "交给传输层",
                    `${r.details.delivery.handedBytes} 字节（不代表客户端已收到）`,
                  ],
                  [
                    "已观察上游终态",
                    r.details.delivery.terminalReceived ? "是" : "否",
                  ],
                  [
                    "终态已交给传输层",
                    r.details.delivery.terminalHandedOff ? "是" : "否",
                  ],
                  ["请求标识摘要", r.details.delivery.requestHash ?? "未记录"],
                  ["响应标识摘要", r.details.delivery.responseHash ?? "未记录"],
                ]
              : []),
            ["首次发生", time(r.firstAt)],
            ["最近发生", time(r.lastAt)],
            ["合并次数", r.count],
          ].map(([key, value]) => (
            <div
              key={key}
              data-field={key === "类别" ? "category" : undefined}
            >
              <dt>{key}</dt>
              <dd>{value}</dd>
            </div>
          ))}
        </dl>
      </div>
    </dialog>
  );
}
