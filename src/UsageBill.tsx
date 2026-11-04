import { useState } from "react";
import Modal from "./Modal";
import { command } from "./bridge";
import { errorOf } from "./types";
import type { BillExport } from "./usage-types";

export const localSecond = (at: number) => {
  const date = new Date(at);
  return new Date(at - date.getTimezoneOffset() * 60_000)
    .toISOString()
    .slice(0, 19);
};
export function billTime(value: string, bound: "start" | "end" = "start") {
  const trimmed = value.trim().replace(" ", "T");
  const normalized = /^\d{4}-\d{2}-\d{2}$/.test(trimmed)
    ? `${trimmed}T${bound === "end" ? "23:59:59" : "00:00:00"}`
    : /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}$/.test(trimmed)
      ? `${trimmed}:${bound === "end" ? "59" : "00"}`
      : trimmed;
  const at = new Date(normalized).getTime();
  return Number.isFinite(at) && localSecond(at) === normalized ? at : null;
}
export default function UsageBill({
  start,
  end,
  close,
  saved,
}: {
  start?: number;
  end?: number;
  close: () => void;
  saved: (result: BillExport) => void;
}) {
  const [clients, setClients] = useState("both");
  const [from, setFrom] = useState(localSecond(start ?? 0));
  const [to, setTo] = useState(localSecond(end ?? Date.now()));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const zone = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const first = billTime(from, "start"),
    last = billTime(to, "end");
  const valid = first !== null && last !== null && first >= 0 && last >= first;
  const display = (value: string) => value.replace("T", " ");
  return (
    <Modal
      title="导出账单"
      close={close}
      busy={busy}
      className="usage-bill-modal"
    >
      <form
        className="usage-bill-form"
        onSubmit={async (event) => {
          event.preventDefault();
          if (!valid || busy) return;
          setBusy(true);
          setError("");
          try {
            const result = await command("export_usage_bill", {
              request: {
                clients:
                  clients === "both" ? ["claude", "antigravity"] : [clients],
                start: first!,
                end: last!,
                timezone: zone,
              },
            });
            if (result) saved(result);
          } catch (e) {
            setError(errorOf(e).message);
          } finally {
            setBusy(false);
          }
        }}
      >
        <label>
          客户端
          <select
            value={clients}
            disabled={busy}
            onChange={(e) => setClients(e.target.value)}
          >
            <option value="both">Claude Code 与 Antigravity app</option>
            <option value="claude">Claude Code</option>
            <option value="antigravity">Antigravity app</option>
          </select>
        </label>
        <div className="usage-bill-times">
          <label>
            开始
            <input
              type="datetime-local"
              step="1"
              value={from}
              disabled={busy}
              onChange={(e) =>
                setFrom(
                  e.target.value.length === 16
                    ? `${e.target.value}:00`
                    : e.target.value,
                )
              }
              required
            />
          </label>
          <label>
            结束
            <input
              type="datetime-local"
              step="1"
              value={to}
              disabled={busy}
              onChange={(e) =>
                setTo(
                  e.target.value.length === 16
                    ? `${e.target.value}:59`
                    : e.target.value,
                )
              }
              required
            />
          </label>
        </div>
        <dl className="usage-bill-scope">
          <dt>时区</dt>
          <dd>{zone}</dd>
          <dt>导出范围</dt>
          <dd>
            {display(from)} 至 {display(to)}
          </dd>
          <dt>清理范围</dt>
          <dd>
            所选客户端 {display(from)} 至 {display(to)} 的已导出明细与统计汇总。
          </dd>
        </dl>
        <p className="usage-bill-rule">
          文件保存成功后同步清理该范围的明细与统计汇总，仍在变化的请求留待下次处理。
        </p>
        {error && (
          <p role="alert" className="form-error">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button type="button" disabled={busy} onClick={close}>
            取消
          </button>
          <button className="primary" disabled={busy || !valid}>
            {busy ? "正在导出…" : "导出并清理"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
