import { useEffect, useRef, useState } from "react";
import { errorOf, type ClientId, type GatewaySettings, type GatewayListener } from "./types";
import type { EditRevision } from "./gateway-edit";

export default function Advanced({
  clientId,
  onDirtyChange,
  settings,
  listener,
  revision,
  busy,
  save,
}: {
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
  const fields: [keyof GatewaySettings, string][] = [
    ["port", "共享本地端口"],
    ["maxRetries", "最大重试次数"],
    ["failureThreshold", "连续失败阈值"],
    ["transientFailureThreshold", "5xx 连续失败阈值"],
    ["successThreshold", "恢复成功次数"],
    ["cooldownSeconds", "熔断等待 / 秒"],
    ["rateLimitSeconds", "429 默认冷却 / 秒"],
    ...(clientId === "codex"
      ? [
          ["websocketRetrySeconds", "WebSocket 断开等待 / 秒"] as [
            keyof GatewaySettings,
            string,
          ],
          ["threadIdleReleaseSeconds", "线程空闲释放 / 秒"] as [
            keyof GatewaySettings,
            string,
          ],
          ["capacityRetrySeconds", "容量错误等待 / 秒"] as [
            keyof GatewaySettings,
            string,
          ],
        ]
      : []),
    ["errorRate", "错误率阈值 (0–1)"],
    ["minRequests", "最小请求数"],
    ["firstByteSeconds", "首字节 / 秒"],
    ["idleSeconds", "流静默 / 秒"],
    ["retryBudgetSeconds", "等待与重试预算 / 秒"],
    ["connectSeconds", "连接超时 / 秒"],
    ["endpointProbeSeconds", "端点测速间隔 / 秒"],
    ["queueSeconds", "排队等待 / 秒"],
    ["maxWaiting", "最多等待请求"],
  ];
  return (
    <form
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
      <div className="settings-grid">
        {fields.map(([key, label]) => (
          <label key={key}>
            {label}
            <input
              type="number"
              disabled={key === "port" && !!listener?.runningClients.length}
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
                key === "capacityRetrySeconds" ||
                key === "websocketRetrySeconds" ||
                key === "threadIdleReleaseSeconds" ||
                key === "endpointProbeSeconds"
                  ? 86400
                  : key === "transientFailureThreshold"
                    ? 1000
                    : undefined
              }
              step={key === "errorRate" ? 0.01 : 1}
              value={
                draft[key] ??
                (key === "transientFailureThreshold"
                  ? 6
                  : key === "threadIdleReleaseSeconds" ||
                      key === "endpointProbeSeconds"
                    ? 300
                    : "")
              }
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
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      <button disabled={busy} className="secondary">
        {baseRevision.current === null ? "重试" : "保存参数"}
      </button>
    </form>
  );
}
