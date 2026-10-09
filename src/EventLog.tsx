import { useCallback, useEffect, useRef, useState } from "react";
import { ChevronLeft, ChevronRight, RefreshCw, Trash2, X } from "lucide-react";
import { command, subscribe } from "./bridge";
import { confirmAction } from "./confirmation";
import { errorOf, type ClientId, type GatewayState } from "./types";

export const reasons = {
  model_unavailable: "模型不支持",
  authentication: "认证失败",
  upstream_service: "上游服务异常",
  network: "连接失败",
  rate_limit: "上游限流",
  capacity: "容量不足",
  failover_exhausted: "换商失败",
  failover: "自动换商",
  circuit_open: "供应商熔断",
  recovered: "供应商恢复",
  config_conflict: "配置冲突",
  startup_recovery: "启动恢复失败",
  account_sync: "账号同步异常",
  protocol_error: "返回了应用错误",
} as const;

export const errorCodes: Record<keyof typeof reasons, string> = {
  model_unavailable: "MODEL_UNAVAILABLE",
  authentication: "AUTHENTICATION_FAILED",
  upstream_service: "UPSTREAM_SERVICE_ERROR",
  network: "NETWORK_ERROR",
  rate_limit: "RATE_LIMITED",
  capacity: "CAPACITY_LIMITED",
  failover_exhausted: "FAILOVER_EXHAUSTED",
  failover: "FAILOVER",
  circuit_open: "CIRCUIT_OPEN",
  recovered: "RECOVERED",
  config_conflict: "CONFIG_CONFLICT",
  startup_recovery: "STARTUP_RECOVERY_FAILED",
  account_sync: "ACCOUNT_SYNC_ERROR",
  protocol_error: "PROTOCOL_ERROR",
};

const actions = {
  trying_next: "尝试下一供应商",
  returned: "已反馈客户端",
  waiting: "等待后重试",
  stopped: "需要处理",
  routed: "已切换供应商",
  recovered: "已恢复可用",
  reconnecting: "等待后重新连接",
  not_retried: "已输出，未重试",
} as const;

export type EventRecord = {
  id: string;
  firstAt: number;
  lastAt: number;
  count: number;
  clientId: ClientId | null;
  providerId: string | null;
  model: string | null;
  reason: keyof typeof reasons;
  errorCode?: string;
  action: keyof typeof actions;
  level: "warning" | "error" | "info";
  status: number | null;
  attempt: number | null;
  details?: {
    upstreamCode?: string | null;
    upstreamType?: string | null;
    parameter?: string | null;
    message?: string | null;
    phase?: string | null;
    wsCloseCode?: number | null;
    countedFailure?: boolean | null;
    waitSeconds?: number | null;
    causeId?: string | null;
    circuit?: {
      failures: number;
      failureThreshold: number;
      failedRequests: number;
      requests: number;
      errorRate: number;
      minRequests: number;
      trigger: string;
    } | null;
  };
};

type EventPage = {
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

const client = (id: ClientId | null) =>
  id === "codex" ? "Codex" : id === "claude" ? "Claude Code" : "应用";

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
    !/(@|:\/\/|sk-|eyJ|Bearer|[\\/]|[\u0000-\u001f]|\b(?:\d{1,3}\.){3}\d{1,3}\b|\b[a-z0-9-]+\.(com|net|org|xyz|cn|io|dev)\b)/i.test(
      name,
    )
    ? name
    : "供应商 " + id.slice(0, 8);
}

function statusText(status: number | null) {
  return status === null ? "—" : "HTTP " + status;
}

function statusTone(status: number | null) {
  if (status === null) return "none";
  if (status >= 200 && status < 300) return "success";
  if (status >= 400 && status < 500) return "client";
  if (status >= 500) return "server";
  return "other";
}

function normalizedCode(record: EventRecord) {
  return record.errorCode || errorCodes[record.reason] || "UNKNOWN_ERROR";
}

const phases: Record<string, string> = {
  connect: "建立连接",
  headers: "等待响应头",
  response: "读取响应",
  stream: "流式响应",
  ws_handshake: "WebSocket 握手",
  ws_send: "WebSocket 发送",
  ws_receive: "WebSocket 接收",
  ws_wait: "等待重连",
};
function summary(r: EventRecord) {
  return [r.details?.upstreamCode, r.details?.message || reasons[r.reason]]
    .filter(Boolean)
    .join(" · ");
}
function actionText(r: EventRecord) {
  return `${r.details?.countedFailure === false ? "未计入熔断，" : ""}${r.details?.waitSeconds ? `等待 ${r.details.waitSeconds} 秒${r.action === "reconnecting" ? "重连" : "重试"}` : actions[r.action]}`;
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

export default function EventLog() {
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
    reason: "",
    statusGroup: "" as StatusGroup,
    from: "",
    to: "",
  });
  const [page, setPage] = useState(1);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [providers, setProviders] = useState<
    Record<string, { name: string; clientId: ClientId }>
  >({});
  const [selected, setSelected] = useState<EventRecord | null>(null);
  const [documentVisible, setDocumentVisible] = useState(
    document.visibilityState !== "hidden",
  );
  const [nativeVisible, setNativeVisible] = useState(true);
  const visible = documentVisible && nativeVisible;
  const sequence = useRef(0);

  const load = useCallback(async () => {
    const request = ++sequence.current;
    try {
      const filter = Object.fromEntries(
        Object.entries(filters).filter(([, value]) => value),
      );
      const result = await command<EventPage>("get_app_events", {
        filter: {
          ...filter,
          page,
          from: filters.from
            ? Math.floor(new Date(filters.from).getTime() / 1000)
            : null,
          to: filters.to
            ? Math.floor(new Date(filters.to).getTime() / 1000)
            : null,
        },
      });
      if (request === sequence.current) {
        setData(result);
        setError("");
      }
    } catch (e) {
      if (request === sequence.current) setError(errorOf(e).message);
    }
  }, [filters, page]);

  useEffect(() => {
    if (visible) void load();
    return () => {
      sequence.current++;
    };
  }, [visible, load]);

  useEffect(() => {
    let disposed = false;
    const clean: (() => void)[] = [];
    let timer: number;

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

    for (const id of ["codex", "claude"] as const)
      void command<GatewayState>("get_gateway", { clientId: id })
        .then((s) => {
          if (!disposed) accept(s);
        })
        .catch(() => {});

    const watch = <T,>(name: string, fn: (value: T) => void) =>
      void subscribe<T>(name, (value) => {
        if (!disposed) fn(value);
      }).then((c) => (disposed ? c() : clean.push(c)));

    watch<GatewayState>("gateway-state", accept);
    watch<boolean>("app-visibility", setNativeVisible);
    watch("app-event", () => {
      if (visible) {
        window.clearTimeout(timer);
        timer = window.setTimeout(() => void load(), 250);
      }
    });

    const visibility = () =>
      setDocumentVisible(document.visibilityState !== "hidden");
    document.addEventListener("visibilitychange", visibility);

    return () => {
      disposed = true;
      window.clearTimeout(timer);
      clean.forEach((c) => c());
      document.removeEventListener("visibilitychange", visibility);
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
    if (!(await confirmAction("清空全部异常日志？"))) return;
    setBusy(true);
    try {
      await command("clear_app_events");
      setSelected(null);
      setPage(1);
      await load();
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
            onClick={() => void load()}
          >
            <RefreshCw size={16} />
          </button>
          <button
            className="icon-button"
            aria-label="清空日志"
            disabled={busy}
            onClick={() => void clear()}
          >
            <Trash2 size={16} />
          </button>
        </div>
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
          onChange={(e) => change("clientId", e.target.value)}
        >
          <option value="">全部客户端</option>
          <option value="codex">Codex</option>
          <option value="claude">Claude Code</option>
        </select>
        <select
          aria-label="供应商"
          value={filters.providerId}
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
          {Object.entries(reasons)
            .filter(([key]) => key !== "recovered")
            .map(([key, label]) => (
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
            {data.items
              .filter((r) => r.reason !== "recovered")
              .map((r) => (
                <tr key={r.id}>
                  <td>
                    <time title={new Date(r.lastAt * 1000).toISOString()}>
                      {time(r.lastAt)}
                    </time>
                  </td>
                  <td>
                    <span>{client(r.clientId)}</span>
                    <strong title={name(r)}>{name(r)}</strong>
                  </td>
                  <td>
                    <span
                      className={"event-status " + statusTone(r.status)}
                      title={wireStatus(r)}
                    >
                      {r.status != null && <span>{statusText(r.status)}</span>}
                      {r.details?.wsCloseCode != null && <span>WS {r.details.wsCloseCode}</span>}
                      {r.status == null && r.details?.wsCloseCode == null && "—"}
                    </span>
                  </td>
                  <td>
                    <button
                      className={"event-code " + r.level}
                      aria-label={reasons[r.reason]}
                      onClick={(e) => {
                        e.currentTarget.focus({ preventScroll: true });
                        void command<EventRecord | null>("get_app_event", {
                          id: r.id,
                        })
                          .then((value) => setSelected(value || r))
                          .catch((e) => setError(errorOf(e).message));
                      }}
                    >
                      {summary(r)}
                      {r.count > 1 && (
                        <span className="event-count">
                          ×{r.count.toLocaleString()}
                        </span>
                      )}
                    </button>
                  </td>
                  <td>{actionText(r)}</td>
                </tr>
              ))}
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
          onClose={() => setSelected(null)}
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

  return (
    <dialog
      ref={ref}
      className="event-drawer"
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
        <header>
          <h2 ref={heading} id="event-title" tabIndex={-1}>
            {reasons[r.reason]}
          </h2>
          <button
            className="icon-button"
            aria-label="关闭详情"
            onClick={onClose}
          >
            <X size={18} />
          </button>
        </header>
        <dl>
          {[
            ["状态码", wireStatus(r)],
            ["上游错误码", r.details?.upstreamCode ?? "未提供"],
            ["上游错误类型", r.details?.upstreamType ?? "未提供"],
            ["错误摘要", r.details?.message ?? reasons[r.reason]],
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
            ["错误码", normalizedCode(r)],
            ["客户端", client(r.clientId)],
            ["供应商", provider],
            ["处理结果", actionText(r)],
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
                  [
                    "触发条件",
                    (
                      {
                        consecutive_failures: "连续失败达到阈值",
                        error_rate: "错误率达到阈值",
                        probe_failed: "恢复探测失败",
                      } as Record<string, string>
                    )[r.details.circuit.trigger],
                  ],
                  [
                    "错误率阈值",
                    `${r.details.circuit.errorRate * 100}% / 最少 ${r.details.circuit.minRequests} 次`,
                  ],
                ]
              : []),
            ...(r.details?.causeId ? [["触发事件", r.details.causeId]] : []),
            ["模型", r.model || "未提供"],
            ["尝试次数", r.attempt ?? "—"],
            ["首次发生", time(r.firstAt)],
            ["最近发生", time(r.lastAt)],
            ["合并次数", r.count],
          ].map(([key, value]) => (
            <div key={key}>
              <dt>{key}</dt>
              <dd>{value}</dd>
            </div>
          ))}
        </dl>
      </div>
    </dialog>
  );
}
