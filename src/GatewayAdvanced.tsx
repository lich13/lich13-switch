import FailurePolicyCatalog from "./FailurePolicyCatalog";
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
  const basic: [
    Exclude<keyof GatewaySettings, "customFailureRules">,
    string,
  ][] = [
    ["port", "共享本地端口"],
    ["endpointProbeSeconds", "端点测速间隔 / 秒"],
    ["queueSeconds", "排队等待 / 秒"],
    ["maxWaiting", "最多等待请求"],
    ...(clientId === "codex"
      ? [
          ["threadIdleReleaseSeconds", "线程空闲释放 / 秒"] as [
            Exclude<keyof GatewaySettings, "customFailureRules">,
            string,
          ],
        ]
      : []),
  ];
  const groups: {
    name: string;
    fields: [Exclude<keyof GatewaySettings, "customFailureRules">, string][];
  }[] = [
    {
      name: "限流与容量",
      fields: [
        ["rateLimitSeconds", "限流基础等待 / 秒"],
        ["capacityRetrySeconds", "容量错误等待 / 秒"],
      ],
    },
    {
      name: "额度与认证",
      fields: [["hardFailureCooldownSeconds", "额度／认证冷却 / 秒"]],
    },
    {
      name: "服务与网络故障",
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
                Exclude<keyof GatewaySettings, "customFailureRules">,
                string,
              ],
            ]
          : []),
      ],
    },
  ];
  const fields = (
    items: [Exclude<keyof GatewaySettings, "customFailureRules">, string][],
  ) => (
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
            <FailurePolicyCatalog
              group={group.name}
              clientId={clientId}
              rules={settings.customFailureRules ?? []}
              revision={revision}
            />
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
          void save(
            { ...draft, customFailureRules: settings.customFailureRules ?? [] },
            baseRevision.current,
          )
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
