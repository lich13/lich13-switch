import { command } from "./bridge";
import { Pencil, Plus, Trash2 } from "lucide-react";
import FailureRuleDialog from "./FailureRuleDialog";
import { failureCategories, type FailureRule } from "./failure-policy-types";
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
  ];
  const groups: {
    name: string;
    tone: "rate" | "quota" | "service";
    fields: [Exclude<keyof GatewaySettings, "customFailureRules">, string][];
  }[] = [
    {
      name: "限流与容量",
      tone: "rate",
      fields: [
        ["rateLimitSeconds", "限流基础等待 / 秒"],
        ["capacityRetrySeconds", "容量错误等待 / 秒"],
      ],
    },
    {
      name: "额度与认证",
      tone: "quota",
      fields: [["hardFailureCooldownSeconds", "额度／认证冷却 / 秒"]],
    },
    {
      name: "服务与网络故障",
      tone: "service",
      fields: [
        ["maxRetries", "最大重试次数"],
        ["retryBudgetSeconds", "等待与重试预算 / 秒"],
        ["failureThreshold", "连续失败阈值"],
        ["transientFailureThreshold", "5xx 连续失败阈值"],
        ["successThreshold", "恢复成功次数"],
        ["cooldownSeconds", "熔断等待 / 秒"],
        ["queueSeconds", "排队等待 / 秒"],
        ["maxWaiting", "最多等待请求"],
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
    <div className="policy-parameters">
      {items.map(([key, label]) => (
        <label key={key} className="policy-parameter">
          <span>{label.split(" / ")[0]}</span>
          <input
            aria-label={label}
            form={formId}
            type="number"
            disabled={
              busy ||
              (key === "port" &&
                (!!listener?.runningClients.length ||
                  !!listener?.antigravityActive))
            }
            required
            min={
              key === "maxRetries" ? 0 : key === "endpointProbeSeconds" ? 10 : 1
            }
            max={
              key === "maxRetries"
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
            step={1}
            value={
              draft[key] ?? (key === "transientFailureThreshold" ? 6 : 300)
            }
            onInvalid={(e) => {
              let section = e.currentTarget.closest("details");
              while (section) {
                section.open = true;
                section = section.parentElement?.closest("details") ?? null;
              }
            }}
            onChange={(e) => {
              if (!editing) baseRevision.current = revision;
              setEditing(true);
              onDirtyChange?.(true);
              setDraft({ ...draft, [key]: Number(e.target.value) });
            }}
          />
          <span className="policy-unit">
            {label.includes(" / 秒")
              ? "秒"
              : key === "maxWaiting"
                ? "个"
                : [
                      "maxRetries",
                      "failureThreshold",
                      "transientFailureThreshold",
                      "successThreshold",
                    ].includes(key)
                  ? "次"
                  : ""}
          </span>
        </label>
      ))}
    </div>
  );
  return (
    <section className="gateway-settings">
      <details className="advanced">
        <summary>故障策略</summary>
        <FailureRules
          clientId={clientId}
          rules={settings.customFailureRules ?? []}
          revision={revision}
        />
        {groups.map((group) => (
          <section
            className="failure-policy-group"
            data-policy-group={group.tone}
            key={group.name}
          >
            <h3>{group.name}</h3>
            {fields(group.fields)}
          </section>
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

function FailureRules({
  clientId,
  rules,
  revision,
}: {
  clientId: ClientId;
  rules: FailureRule[];
  revision: string;
}) {
  const [editing, setEditing] = useState<Partial<FailureRule> | null>(null),
    [deleting, setDeleting] = useState(""),
    [error, setError] = useState("");
  return (
    <div className="failure-catalog">
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      <h3>自定义规则</h3>
      {rules.map((rule) => (
        <div className="custom-failure-rule" key={rule.id}>
          <span>{rule.matchKind === "code" ? "错误码" : "关键词"}</span>
          <code>{rule.value}</code>
          <span>{categoryLabel(rule.category)}</span>
          <button
            className="icon-button"
            aria-label={`编辑 ${rule.value}`}
            onClick={() => setEditing(rule)}
          >
            <Pencil size={14} />
          </button>
          <button
            className="icon-button"
            disabled={deleting !== ""}
            aria-label={`删除 ${rule.value}`}
            onClick={async () => {
              setDeleting(rule.id);
              setError("");
              try {
                await command("update_gateway", {
                  clientId,
                  expectedRevision: revision,
                  edit: { op: "deleteFailureRule", id: rule.id },
                });
              } catch (e) {
                setError(errorOf(e).message);
              } finally {
                setDeleting("");
              }
            }}
          >
            <Trash2 size={14} />
          </button>
        </div>
      ))}
      <button
        type="button"
        className="text-button failure-add"
        onClick={() => setEditing({})}
      >
        <Plus size={14} />
        添加匹配条件
      </button>
      {editing && (
        <FailureRuleDialog
          clientId={clientId}
          initial={editing}
          close={() => setEditing(null)}
        />
      )}
    </div>
  );
}

function categoryLabel(category: string): string {
  return (
    failureCategories.find((c) => c.category === category)?.label ?? category
  );
}
