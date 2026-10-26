import { useEffect, useId, useRef, useState, type ReactNode } from "react";
import {
  errorOf,
  type ClientId,
  type GatewaySettings,
  type GatewayListener,
} from "./types";
import type { EditRevision } from "./gateway-edit";

export default function Advanced({
  clientId,
  onDirtyChange,
  settings,
  listener,
  revision,
  busy,
  save,
  children,
}: {
  children?: ReactNode;
  clientId: ClientId;
  onDirtyChange?: (dirty: boolean) => void;
  settings: GatewaySettings;
  listener?: GatewayListener;
  revision: string;
  busy: boolean;
  save: (s: GatewaySettings, revision: EditRevision) => Promise<void>;
}) {
  const [draft, setDraft] = useState(settings),
    [error, setError] = useState("");
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const [editing, setEditing] = useState(false);
  const baseRevision = useRef<EditRevision>(revision);
  useEffect(() => {
    if (!editing) {
      setDraft(settings);
      baseRevision.current = revision;
    }
  }, [settings, revision, editing]);
  const formId = useId();
  const basic: [keyof GatewaySettings, string][] = [
    ["port", "共享本地端口"],
    ["endpointProbeSeconds", "端点测速间隔 / 秒"],
    ["queueSeconds", "排队等待 / 秒"],
    ["maxWaiting", "最多等待请求"],
    ...(clientId === "codex"
      ? [
          ["threadIdleReleaseSeconds", "线程空闲释放 / 秒"] as [
            keyof GatewaySettings,
            string,
          ],
        ]
      : []),
  ];
  const groups: {
    name: string;
    rules: [string, string][];
    fields: [keyof GatewaySettings, string][];
  }[] = [
    {
      name: "限流与容量",
      rules: [
        ["RPM / RPS、Token 限流", "按对应窗口等待"],
        ["并发、突发、短时消费限制", "退避后重新准入"],
        ["容量不足、未分类 429", "已有会话优先原供应商"],
      ],
      fields: [
        ["rateLimitSeconds", "限流基础等待 / 秒"],
        ["capacityRetrySeconds", "容量错误等待 / 秒"],
      ],
    },
    {
      name: "额度与认证",
      rules: [
        ["周期额度耗尽", "到重置时间按需探测"],
        ["余额、硬额度、认证失败", "跳过当前候选，冷却后探测"],
        ["模型无权使用", "仅隔离对应模型"],
      ],
      fields: [["hardFailureCooldownSeconds", "额度／认证冷却 / 秒"]],
    },
    {
      name: "服务与网络故障",
      rules: [
        ["服务异常、连接失败", "有限重试与熔断"],
        ["参数错误、单次超限、安全拒绝", "返回原因"],
        ["请求已发出的首字超时、已输出内容", "不自动重放"],
      ],
      fields: [
        ["maxRetries", "最大重试次数"],
        ["retryBudgetSeconds", "等待与重试预算 / 秒"],
        ["failureThreshold", "连续失败阈值"],
        ["transientFailureThreshold", "5xx 连续失败阈值"],
        ["successThreshold", "恢复成功次数"],
        ["cooldownSeconds", "熔断等待 / 秒"],
        ["errorRate", "错误率阈值 (0–1)"],
        ["minRequests", "最小请求数"],
        ["connectSeconds", "连接超时 / 秒"],
        ["firstByteSeconds", "首字节 / 秒"],
        ["idleSeconds", "流静默 / 秒"],
        ...(clientId === "codex"
          ? [
              ["websocketRetrySeconds", "WebSocket 断开等待 / 秒"] as [
                keyof GatewaySettings,
                string,
              ],
            ]
          : []),
      ],
    },
  ];
  const fields = (items: [keyof GatewaySettings, string][]) => (
    <div className="settings-grid">
      {items.map(([key, label]) => (
        <label key={key}>
          {label}
          <input
            form={formId}
            type="number"
            disabled={
              busy || (key === "port" && !!listener?.runningClients.length)
            }
            required
            min={
              key === "maxRetries"
                ? 0
                : key === "errorRate"
                  ? 0.01
                  : key === "endpointProbeSeconds"
                    ? 10
                    : 1
            }
            max={
              key === "errorRate"
                ? 1
                : key === "maxRetries"
                  ? 15
                  : key === "transientFailureThreshold"
                    ? 1000
                    : key === "maxWaiting"
                      ? 10000
                      : key === "port"
                        ? 65535
                        : key.endsWith("Seconds")
                          ? 86400
                          : undefined
            }
            step={key === "errorRate" ? 0.01 : 1}
            value={
              draft[key] ?? (key === "transientFailureThreshold" ? 6 : 300)
            }
            onInvalid={(e) => {
              const section = e.currentTarget.closest("details");
              if (section) section.open = true;
            }}
            onChange={(e) => {
              if (!editing) baseRevision.current = revision;
              setEditing(true);
              onDirtyChange?.(true);
              setDraft({ ...draft, [key]: Number(e.target.value) });
            }}
          />
        </label>
      ))}
    </div>
  );
  return (
    <section className="gateway-settings">
      <details className="advanced">
        <summary>故障策略</summary>
        {groups.map((group) => (
          <fieldset className="failure-policy-group" key={group.name}>
            <legend>{group.name}</legend>
            <dl className="failure-policy-rules">
              {group.rules.map(([kind, action]) => (
                <div key={kind}>
                  <dt>{kind}</dt>
                  <dd>{action}</dd>
                </div>
              ))}
            </dl>
            {fields(group.fields)}
          </fieldset>
        ))}
      </details>
      <details className="advanced">
        <summary>高级设置</summary>
        {children}
        {fields(basic)}
      </details>
      <form
        id={formId}
        onSubmit={(e) => {
          e.preventDefault();
          setError("");
          void save(draft, baseRevision.current)
            .then(() => {
              setEditing(false);
              onDirtyChange?.(false);
            })
            .catch((e) => {
              baseRevision.current = null;
              setError(errorOf(e).message);
            });
        }}
      >
        {error && (
          <div role="alert" className="form-error">
            {error}
          </div>
        )}
        <button disabled={busy} className="secondary">
          {baseRevision.current === null ? "重试" : "保存参数"}
        </button>
      </form>
    </section>
  );
}
