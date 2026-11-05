import { useMemo, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId, type GatewayState } from "./types";
const phases: Record<string, string> = {
  receiving: "接收中",
  admission: "等待准入",
  headers: "等待首字节",
  capacity: "容量等待",
  retry: "等待重试",
  ws_wait: "等待重连",
  compaction: "压缩中",
  frozen: "已暂存",
  streaming: "输出中",
  finishing: "收尾中",
};
const rate = new Intl.NumberFormat("zh-CN", { maximumFractionDigits: 1 });
export default function QuickWaits({
  gateway,
  clientId,
  notify,
  error,
}: {
  gateway: GatewayState;
  clientId: ClientId;
  notify: (s: string) => void;
  error: (s: string) => void;
}) {
  const [busy, setBusy] = useState<string[]>([]);
  const receivedAt = useMemo(() => Date.now(), [gateway.pendingRequests]);
  const requests = gateway.activeRequests ?? [];
  if (!requests.length) return null;
  return (
    <section className="gateway-requests" aria-label="进行中的请求">
      {requests.map((r) => {
        const pending = gateway.pendingRequests?.find(
          (p) => p.requestId === r.waitId,
        );
        const provider =
          gateway.providers.find((p) => p.id === r.providerId)?.name ??
          "等待供应商";
        const elapsed =
          r.phase === "frozen"
            ? 0
            : Math.max(0, Math.floor((Date.now() - receivedAt) / 1000));
        return (
          <div className="gateway-request" key={r.requestId}>
            <div className="gateway-request-identity">
              <strong title={provider}>{provider}</strong>
              <span title={r.model ?? undefined}>
                {r.model ?? "未指定模型"}
              </span>
            </div>
            <span className="gateway-request-rate">
              {r.tokensPerSecond == null
                ? "—"
                : `≈${rate.format(r.tokensPerSecond)} tok/s`}
            </span>
            {pending && (
              <button
                className="secondary"
                disabled={busy.includes(pending.requestId)}
                onClick={async () => {
                  const requestId = pending.requestId;
                  setBusy((v) => [...v, requestId]);
                  try {
                    const cancelled = await command("cancel_gateway_wait", {
                      clientId,
                      requestId,
                    });
                    notify(cancelled ? "已中止等待" : "等待已结束");
                  } catch (e) {
                    error(errorOf(e).message);
                  } finally {
                    setBusy((v) => v.filter((id) => id !== requestId));
                  }
                }}
              >
                {busy.includes(pending.requestId) ? "中止中…" : "中止"}
              </button>
            )}
            <div className="gateway-request-timing">
              <time>
                {new Date(r.startedAt * 1000).toLocaleTimeString("zh-CN", {
                  hour12: false,
                })}
              </time>
              <span>{phases[r.phase] ?? r.phase}</span>
              <span>已用 {r.elapsedSeconds} 秒</span>
              {pending?.remainingSeconds != null && r.phase !== "frozen" && (
                <span>
                  剩余 {Math.max(0, pending.remainingSeconds - elapsed)} 秒
                </span>
              )}
            </div>
          </div>
        );
      })}
    </section>
  );
}
