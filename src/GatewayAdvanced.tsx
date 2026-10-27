import { command } from "./bridge";
import FailurePolicyCatalog from "./FailurePolicyCatalog";
import FailurePolicySearch from "./FailurePolicySearch";
import type {
  FailureFocus,
  FailureCategory,
  FailureOrigin,
  FailureParameter,
} from "./failure-policy-types";
import {
  useCallback,
  useEffect,
  useId,
  useRef,
  useState,
  type ReactNode,
} from "react";
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
  focusFailure,
  onFailureHandled,
}: {
  children?: ReactNode;
  focusFailure?: FailureFocus | null;
  onFailureHandled?: () => void;
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
  const [allOpen, setAllOpen] = useState(false);
  const [catalog, setCatalog] = useState<FailureCategory[]>([]);
  const [catalogError, setCatalogError] = useState("");
  const [origin, setOrigin] = useState<FailureOrigin>("upstream");
  const [highlight, setHighlight] = useState<FailureParameter[]>([]);
  const selectedPolicy = useCallback(
    (next: FailureOrigin, keys: FailureParameter[]) => {
      setOrigin(next);
      setHighlight(keys);
      setAllOpen(true);
    },
    [],
  );
  useEffect(() => {
    let active = true;
    void command("get_failure_policy_catalog")
      .then((items) => {
        if (active) setCatalog(items ?? []);
      })
      .catch((e) => {
        if (active) setCatalogError(errorOf(e).message);
      });
    return () => {
      active = false;
    };
  }, []);
  const policySection = useRef<HTMLDetailsElement>(null);
  useEffect(() => {
    if (focusFailure && policySection.current) {
      policySection.current.open = true;
      setAllOpen(false);
      policySection.current.scrollIntoView?.({ block: "start" });
    }
  }, [focusFailure]);
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
        <label
          key={key}
          className={
            highlight.includes(key)
              ? "policy-parameter is-relevant"
              : "policy-parameter"
          }
        >
          <span>{label.split(" / ")[0]}</span>
          <input
            aria-label={label}
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
                      "minRequests",
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
      <details className="advanced" ref={policySection}>
        <summary>故障策略</summary>
        <FailurePolicySearch
          clientId={clientId}
          rules={settings.customFailureRules ?? []}
          revision={revision}
          focus={focusFailure}
          consumed={onFailureHandled}
          catalog={catalog}
          onSelection={selectedPolicy}
        />
        <details
          className="failure-all"
          open={allOpen}
          onToggle={(e) => setAllOpen(e.currentTarget.open)}
        >
          <summary>全部规则与参数</summary>
          {catalogError && (
            <div role="alert" className="form-error">
              {catalogError}
            </div>
          )}
          <div className="policy-tabs" role="tablist" aria-label="故障策略来源">
            {(["upstream", "gateway"] as const).map((value) => (
              <button
                type="button"
                role="tab"
                key={value}
                aria-selected={origin === value}
                onClick={() => setOrigin(value)}
              >
                {value === "upstream" ? "上游报错策略" : "网关内部策略"}
              </button>
            ))}
          </div>
          {allOpen && (
            <>
              <div
                role="tabpanel"
                aria-label={
                  origin === "upstream" ? "上游报错策略" : "网关内部策略"
                }
              >
                {origin === "upstream" ? (
                  groups.map((group) => (
                    <section className="failure-policy-group" key={group.name}>
                      <h3>
                        {group.name === "服务与网络故障"
                          ? "服务与请求错误"
                          : group.name}
                      </h3>
                      <FailurePolicyCatalog
                        group={group.name}
                        catalog={catalog.filter((c) => c.origin !== "gateway")}
                        clientId={clientId}
                        rules={settings.customFailureRules ?? []}
                        revision={revision}
                      />
                      {group.name !== "服务与网络故障" && fields(group.fields)}
                    </section>
                  ))
                ) : (
                  <section className="failure-policy-group">
                    <FailurePolicyCatalog
                      catalog={catalog.filter((c) => c.origin === "gateway")}
                      clientId={clientId}
                      rules={[]}
                      revision={revision}
                    />
                    {fields(
                      groups
                        .flatMap((g) => g.fields)
                        .filter(([key]) =>
                          [
                            "queueSeconds",
                            "maxWaiting",
                            "connectSeconds",
                            "firstByteSeconds",
                            "idleSeconds",
                            "websocketRetrySeconds",
                          ].includes(key),
                        ),
                    )}
                  </section>
                )}
              </div>
              <section
                className="failure-policy-group policy-shared"
                aria-label="共享重试与熔断参数"
              >
                <h3>共享重试与熔断参数</h3>
                {fields(
                  groups
                    .flatMap((g) => g.fields)
                    .filter(([key]) =>
                      [
                        "maxRetries",
                        "retryBudgetSeconds",
                        "failureThreshold",
                        "transientFailureThreshold",
                        "successThreshold",
                        "cooldownSeconds",
                        "errorRate",
                        "minRequests",
                      ].includes(key),
                    ),
                )}
              </section>
            </>
          )}
        </details>
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
