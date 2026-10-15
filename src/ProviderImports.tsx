import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId, type GatewayState } from "./types";
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
    let disposed = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    refresh();
    events.watch("provider-imports", refresh);
    return () => {
      disposed = true;
      events.dispose();
    };
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
      let disposed = false;
      void command("get_gateway", {
        clientId: queue[0]?.clientId ?? "codex",
      })
        .then((s) => {
          if (!disposed) {
            setRevision(s.revision);
            setError("");
            setActive(queue[0]);
          }
        })
        .catch((e) => setError(errorOf(e).message));
      return () => {
        disposed = true;
      };
    }
  }, [queue, active, available, blocked]);
  const cancel = () => {
    if (!active) return;
    setBusy(true);
    void command("cancel_provider_import", { id: active.id })
      .then(() => {
        setQueue((q) => q.filter((p) => p.id !== active.id));
        setActive(null);
        setError("");
      })
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
      <dl className="import-preview">
        <div>
          <dt>客户端</dt>
          <dd>{active.clientId === "claude" ? "Claude Code" : "Codex"}</dd>
        </div>
        <div>
          <dt>名称</dt>
          <dd>{active.name}</dd>
        </div>
        <div>
          <dt>地址</dt>
          <dd>{active.baseUrl}</dd>
        </div>
        <div>
          <dt>API Key</dt>
          <dd>••••••••</dd>
        </div>
      </dl>
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      <div className="modal-actions">
        <button className="secondary" disabled={busy} onClick={cancel}>
          取消
        </button>
        <button
          className="primary"
          disabled={busy}
          onClick={() => {
            setBusy(true);
            setError("");
            void command("confirm_provider_import", {
              id: active.id,
              expectedRevision: revision,
            })
              .then(() => {
                setQueue((q) => q.filter((p) => p.id !== active.id));
                setActive(null);
                notify("已添加供应商");
              })
              .catch(async (e) => {
                setError(errorOf(e).message);
                const state = await command("get_gateway", {
                  clientId: active.clientId ?? "codex",
                }).catch(() => null);
                if (state) setRevision(state.revision);
              })
              .finally(() => setBusy(false));
          }}
        >
          {busy ? "正在处理…" : "导入"}
        </button>
      </div>
    </Modal>
  );
}
