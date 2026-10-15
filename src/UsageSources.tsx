import { useState } from "react";
import { Drawer } from "./UsageDrawer";
import { command } from "./bridge";
import { errorOf } from "./types";
import { confirmAction } from "./confirmation";
import { date } from "./usage-format";
import { clientName, numeric, type UsageState } from "./usage-types";

export default function Sources({
  state,
  counts,
  historyIncomplete,
  onClose,
  onChange,
}: {
  state: UsageState;
  counts: Record<string, number>;
  historyIncomplete: boolean;
  onClose: () => void;
  onChange: (v: UsageState) => void;
}) {
  const [draft, setDraft] = useState(state.settings),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const run = async (
    name: "set_usage_settings" | "sync_usage",
    args: { rebuild?: string } = {},
  ) => {
    if (busy) return;
    setBusy(true);
    setError("");
    try {
      if (name === "set_usage_settings") {
        const current = await command("get_usage_state");
        onChange(
          await command(name, {
            settings: {
              ...current.settings,
              recording: draft.recording,
              autoSync: draft.autoSync,
            },
          }),
        );
      } else {
        onChange(await command(name, args));
      }
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <Drawer title="数据来源" onClose={onClose}>
      <div className="usage-source-list">
        {["proxy", "codex", "claude", "grok"].map((id) => (
          <div key={id}>
            <b>{id === "proxy" ? "网关" : clientName(id)}</b>
            <span>{numeric(counts[id] ?? 0)} 条</span>
            {state.reports[id] && (
              <span>
                导入 {numeric(state.reports[id].imported)} · 合并{" "}
                {numeric(state.reports[id].merged ?? 0)} · 待核对{" "}
                {numeric(state.reports[id].pending ?? 0)}
                {state.syncing
                  ? ` · ${state.reports[id].files}/${state.reports[id].totalFiles ?? 0}`
                  : ""}
              </span>
            )}
            {!!state.reports[id]?.historicalBefore && (
              <span>历史日汇总已保留</span>
            )}
            {state.reports[id]?.completedAt && (
              <time>{date(state.reports[id].completedAt!)}</time>
            )}
            {!!state.reports[id]?.errors && (
              <span className="form-error">
                {state.reports[id].errors} 个文件未同步
              </span>
            )}
          </div>
        ))}
      </div>
      {historyIncomplete && (
        <details className="usage-price-source">
          <summary>历史数据</summary>
          <p>部分旧日汇总仅支持全部来源查询。</p>
        </details>
      )}
      <div className="usage-actions">
        <button
          disabled={busy || state.syncing}
          onClick={() => void run("sync_usage")}
        >
          同步会话
        </button>
        <button
          disabled={busy || state.syncing}
          onClick={() =>
            void confirmAction("重建 Codex 会话用量？网关记录会保留。").then(
              (ok) => {
                if (ok) return run("sync_usage", { rebuild: "codex" });
              },
            )
          }
        >
          重建 Codex 用量
        </button>
        <button
          disabled={busy || state.syncing}
          onClick={() =>
            void confirmAction(
              "重建 Claude Code 会话用量？网关记录会保留。",
            ).then((ok) => {
              if (ok) return run("sync_usage", { rebuild: "claude" });
            })
          }
        >
          重建 Claude 用量
        </button>
        <button
          disabled={busy || state.syncing}
          onClick={() =>
            void confirmAction(
              "重建 Grok Build 轮次汇总？网关记录会保留。",
            ).then((ok) => {
              if (ok) return run("sync_usage", { rebuild: "grok" });
            })
          }
        >
          重建 Grok 用量
        </button>
      </div>
      <form
        onSubmit={(e) => {
          e.preventDefault();
          void run("set_usage_settings");
        }}
      >
        <label className="usage-setting">
          记录网关用量
          <input
            type="checkbox"
            checked={draft.recording}
            onChange={(e) =>
              setDraft({ ...draft, recording: e.target.checked })
            }
          />
        </label>
        <label className="usage-setting">
          自动同步会话
          <input
            type="checkbox"
            checked={draft.autoSync}
            onChange={(e) => setDraft({ ...draft, autoSync: e.target.checked })}
          />
        </label>
        {error && (
          <p role="alert" className="form-error">
            {error}
          </p>
        )}
        <div className="usage-form-actions">
          <button type="button" onClick={onClose}>
            关闭
          </button>
          <button className="primary" disabled={busy}>
            保存
          </button>
        </div>
      </form>
    </Drawer>
  );
}
