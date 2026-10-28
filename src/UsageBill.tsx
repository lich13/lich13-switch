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
export function billTime(value: string) {
  const at = new Date(value).getTime();
  return Number.isFinite(at) && localSecond(at) === value ? at : null;
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
  const first = billTime(from),
    last = billTime(to);
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
                    ? `${e.target.value}:00`
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
            所选客户端截至 {display(to)} 的全部明细，包含开始时间以前的记录。
          </dd>
        </dl>
        <p className="usage-bill-rule">
          文件保存成功后清理明细，统计汇总保留。仍在变化的请求留待下次处理。
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
