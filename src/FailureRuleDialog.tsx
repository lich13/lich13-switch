import { useEffect, useState } from "react";
import { command } from "./bridge";
import Modal from "./Modal";
import { errorOf, type ClientId, type GatewayState } from "./types";
import type { FailureCategory, FailureRule } from "./failure-policy-types";

export default function FailureRuleDialog({
  clientId,
  initial,
  close,
  saved,
}: {
  clientId: ClientId;
  initial?: Partial<FailureRule>;
  close: () => void;
  saved?: (state: GatewayState) => void;
}) {
  const [rule, setRule] = useState<FailureRule>(() => ({
    id: crypto.randomUUID(),
    matchKind: "code",
    value: "",
    category: "",
    ...initial,
  }));
  const [catalog, setCatalog] = useState<FailureCategory[]>([]);
  const [revision, setRevision] = useState<string | null>(null);
  const [ready, setReady] = useState(false),
    [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  useEffect(() => {
    let active = true;
    void Promise.all([
      command("get_failure_policy_catalog"),
      command("get_gateway", { clientId }),
    ])
      .then(([items, state]) => {
        if (!active) return;
        setCatalog(items);
        setRevision(state.revision);
        setReady(true);
      })
      .catch((e) => {
        if (active) setError(errorOf(e).message);
      });
    return () => {
      active = false;
    };
  }, [clientId]);
  return (
    <Modal
      compact
      title={initial?.id ? "编辑故障规则" : "加入故障规则"}
      close={close}
      busy={busy}
      className="failure-rule-dialog"
    >
      <form
        onSubmit={async (e) => {
          e.preventDefault();
          if (busy) return;
          setBusy(true);
          setError("");
          try {
            const expectedRevision =
              revision ?? (await command("get_gateway", { clientId })).revision;
            const state = await command("update_gateway", {
              clientId,
              expectedRevision,
              edit: {
                op: "upsertFailureRule",
                rule: { ...rule, value: rule.value.trim() },
              },
            });
            saved?.(state);
            close();
          } catch (e) {
            setRevision(null);
            setError(errorOf(e).message);
          } finally {
            setBusy(false);
          }
        }}
      >
        <label>
          匹配方式
          <select
            value={rule.matchKind}
            disabled={busy}
            onChange={(e) =>
              setRule({
                ...rule,
                matchKind: e.target.value as FailureRule["matchKind"],
              })
            }
          >
            <option value="code">错误码精确匹配</option>
            <option value="keyword">消息包含关键词</option>
          </select>
        </label>
        <label>
          {rule.matchKind === "code" ? "错误码" : "关键词"}
          <input
            required
            maxLength={256}
            value={rule.value}
            disabled={busy}
            onChange={(e) => setRule({ ...rule, value: e.target.value })}
          />
        </label>
        <label>
          错误类别
          <select
            required
            value={rule.category}
            disabled={busy}
            onChange={(e) => setRule({ ...rule, category: e.target.value })}
          >
            <option value="">选择类别</option>
            {catalog.map((c) => (
              <option key={c.category} value={c.category}>
                {c.label}
              </option>
            ))}
          </select>
        </label>
        {error && (
          <div role="alert" className="form-error">
            {error}
          </div>
        )}
        <div className="modal-actions">
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={close}
          >
            取消
          </button>
          <button className="primary" disabled={busy || !ready}>
            {busy
              ? "保存中…"
              : revision === null && ready
                ? "重试"
                : "保存规则"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
