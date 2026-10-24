import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf } from "./types";
import Modal from "./Modal";
import type { PendingImport as Pending } from "./bridge-contracts";

export default function ProviderImports({
  notify,
  blocked = false,
}: {
  notify: (message: string) => void;
  blocked?: boolean;
}) {
  const [queue, setQueue] = useState<Pending[]>([]),
    [active, setActive] = useState<Pending | null>(null),
    [revision, setRevision] = useState(""),
    [busy, setBusy] = useState(false),
    [error, setError] = useState(""),
    [available, setAvailable] = useState(false);
  const refresh = useCallback(() => {
    void command("get_provider_imports")
      .then((q) => setQueue(q ?? []))
      .catch((e) => setError(errorOf(e).message));
  }, []);
  useEffect(() => {
    const events = subscriptions((e) => setError(errorOf(e).message));
    refresh();
    events.watch("provider-imports", refresh);
    return () => events.dispose();
  }, [refresh]);
  useEffect(() => {
    const check = () => setAvailable(!document.querySelector("dialog[open]"));
    const observer = new MutationObserver(check);
    observer.observe(document.body, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["open"],
    });
    check();
    return () => observer.disconnect();
  }, []);
  useEffect(() => {
    if (!active && available && !blocked && queue[0]) {
      setRevision("");
      setError(queue[0].error ?? "");
      setActive(queue[0]);
    }
  }, [queue, active, available, blocked]);
  useEffect(() => {
    setRevision("");
    if (active?.provider) {
      let disposed = false;
      void command("get_gateway", { clientId: active.provider.clientId })
        .then((s) => {
          if (!disposed) setRevision(s.revision);
        })
        .catch((e) => {
          if (!disposed) setError(errorOf(e).message);
        });
      return () => {
        disposed = true;
      };
    }
  }, [active]);
  const dismiss = () => {
    if (!active) return;
    setQueue((q) => q.filter((p) => p.id !== active.id));
    setActive(null);
    setError("");
  };
  const cancel = () => {
    if (!active || busy) return;
    setBusy(true);
    void command("cancel_provider_import", { id: active.id })
      .then(dismiss)
      .catch((e) => setError(errorOf(e).message))
      .finally(() => setBusy(false));
  };
  if (!active)
    return error ? (
      <div className="toast error" role="alert">
        {error}
      </div>
    ) : null;
  return (
    <Modal title="导入供应商" close={cancel} busy={busy} compact>
      {active.provider && (
        <dl className="import-preview">
          <div>
            <dt>客户端</dt>
            <dd>
              {active.provider.clientId === "claude" ? "Claude Code" : "Codex"}
            </dd>
          </div>
          <div>
            <dt>名称</dt>
            <dd>{active.provider.name}</dd>
          </div>
          <div>
            <dt>地址</dt>
            <dd>{active.provider.baseUrl}</dd>
          </div>
          <div>
            <dt>API Key</dt>
            <dd>••••••••</dd>
          </div>
        </dl>
      )}
      {(error || active.importError) && (
        <div role="alert" className="form-error">
          {error || active.importError}
        </div>
      )}
      <div className="modal-actions">
        <button className="secondary" disabled={busy} onClick={cancel}>
          取消
        </button>
        <button
          className="primary"
          disabled={busy || !active.provider || !revision}
          onClick={() => {
            if (busy) return;
            setBusy(true);
            setError("");
            void command("confirm_provider_import", {
              id: active.id,
              expectedRevision: revision,
            })
              .then(() => {
                dismiss();
                notify("已添加供应商");
              })
              .catch(async (e) => {
                setError(errorOf(e).message);
                if (active.provider) {
                  const state = await command("get_gateway", {
                    clientId: active.provider.clientId,
                  }).catch(() => null);
                  if (state) setRevision(state.revision);
                }
              })
              .finally(() => setBusy(false));
          }}
        >
          {busy ? "正在导入…" : "导入"}
        </button>
      </div>
    </Modal>
  );
}
