import { useEffect, useMemo, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId, type GatewayState } from "./types";
const phases: Record<string, string> = {
  admission: "等待准入",
  headers: "等待首字节",
  capacity: "容量等待",
  retry: "等待重试",
  ws_wait: "等待重连",
  compaction: "压缩中",
  frozen: "已暂存",
};
export default function QuickWaits({
  gateway,
  clientId,
  visible,
  notify,
  error,
}: {
  gateway: GatewayState;
  clientId: ClientId;
  visible: boolean;
  notify: (s: string) => void;
  error: (s: string) => void;
}) {
  const [now, setNow] = useState(Date.now()),
    [busy, setBusy] = useState<string[]>([]);
  const receivedAt = useMemo(() => Date.now(), [gateway.pendingRequests]);
  const elapsed = (phase: string) =>
    phase === "frozen" ? 0 : Math.max(0, Math.floor((now - receivedAt) / 1000));
  useEffect(() => {
    if (!visible || !gateway.pendingRequests?.some((r) => r.phase !== "frozen"))
      return;
    setNow(Date.now());
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [visible, gateway.pendingRequests]);
  const requests = (gateway.pendingRequests ?? []).filter(
    (r) => r.elapsedSeconds + elapsed(r.phase) >= 60,
  );
  if (!requests.length) return null;
  return (
    <section className="quick-long-waits" aria-label="长时间等待">
      {requests.map((r) => (
        <div className="quick-wait-row" key={r.requestId}>
          <div className="quick-wait-identity">
            <strong
              title={gateway.providers.find((p) => p.id === r.providerId)?.name}
            >
              {gateway.providers.find((p) => p.id === r.providerId)?.name ??
                "等待供应商"}
            </strong>
            <span title={r.model ?? undefined}>{r.model ?? "未指定模型"}</span>
          </div>
          <button
            className="secondary"
            disabled={busy.includes(r.requestId)}
            onClick={async () => {
              setBusy((v) => [...v, r.requestId]);
              try {
                const cancelled = await command("cancel_gateway_wait", {
                  clientId,
                  requestId: r.requestId,
                });
                notify(cancelled ? "已中止等待" : "等待已结束");
              } catch (e) {
                error(errorOf(e).message);
              } finally {
                setBusy((v) => v.filter((id) => id !== r.requestId));
              }
            }}
          >
            {busy.includes(r.requestId) ? "中止中…" : "中止"}
          </button>
          <div className="quick-wait-timing">
            <time>
              {new Date(r.startedAt * 1000).toLocaleTimeString("zh-CN", {
                hour12: false,
              })}
            </time>
            <span>{phases[r.phase] ?? r.phase}</span>
            <span>
              {r.remainingSeconds == null ? (
                `已用 ${r.elapsedSeconds + elapsed(r.phase)} 秒`
              ) : (
                <>
                  剩余 {Math.max(0, r.remainingSeconds - elapsed(r.phase))} 秒
                </>
              )}
            </span>
          </div>
        </div>
      ))}
    </section>
  );
}
