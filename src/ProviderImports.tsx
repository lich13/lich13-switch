import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf } from "./types";
import Modal from "./Modal";
import type {
  LinkHandlers,
  PendingImport as Pending,
} from "./bridge-contracts";
const SELF = "com.lich13.gpt-switch";

export default function ProviderImports({
  notify,
  blocked = false,
}: {
  notify: (message: string) => void;
  blocked?: boolean;
}) {
  const [queue, setQueue] = useState<Pending[]>([]),
    [active, setActive] = useState<Pending | null>(null),
    [handlers, setHandlers] = useState<LinkHandlers | null>(null),
    [choose, setChoose] = useState(true),
    [appId, setAppId] = useState(""),
    [remember, setRemember] = useState(false),
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
      let disposed = false;
      const pending = queue[0];
      void command("get_link_handler_state")
        .then((state) => {
          if (disposed) return;
          const ask =
            state.remembered !== SELF ||
            !!pending.dispatchError ||
            !pending.provider;
          setHandlers(state);
          setChoose(ask);
          setAppId(ask ? "" : SELF);
          setRemember(false);
          setRevision("");
          setError(pending.dispatchError ?? "");
          setActive(pending);
        })
        .catch((e) => {
          if (!disposed) setError(errorOf(e).message);
        });
      return () => {
        disposed = true;
      };
    }
  }, [queue, active, available, blocked]);
  useEffect(() => {
    setRevision("");
    if (active?.provider && appId === SELF) {
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
  }, [active, appId]);
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
  const local = appId === SELF;
  return (
    <Modal
      title={local ? "导入供应商" : "选择接收应用"}
      close={cancel}
      busy={busy}
      compact
    >
      {choose && (
        <label className="field">
          接收应用
          <select
            aria-label="接收应用"
            disabled={busy}
            value={appId}
            onChange={(e) => {
              setAppId(e.target.value);
              setError("");
            }}
          >
            <option value="" disabled>
              请选择接收应用
            </option>
            {handlers?.apps.map((a) => (
              <option key={a.id} value={a.id}>
                {a.name}
              </option>
            ))}
          </select>
        </label>
      )}
      {local && active.provider && (
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
      {choose && (
        <label className="setting-row">
          <span>下次不再提示</span>
          <input
            type="checkbox"
            checked={remember}
            disabled={busy || !appId}
            onChange={(e) => setRemember(e.target.checked)}
          />
        </label>
      )}
      {(error || (local && active.importError)) && (
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
          disabled={
            busy || !appId || (local && (!active.provider || !revision))
          }
          onClick={() => {
            if (busy) return;
            setBusy(true);
            setError("");
            void command("confirm_provider_import", {
              id: active.id,
              appId,
              remember,
              expectedRevision: local ? revision : null,
            })
              .then((receipt) => {
                dismiss();
                notify(
                  receipt.warning ??
                    (local ? "已添加供应商" : "已交给接收应用"),
                );
              })
              .catch(async (e) => {
                setError(errorOf(e).message);
                if (local && active.provider) {
                  const state = await command("get_gateway", {
                    clientId: active.provider.clientId,
                  }).catch(() => null);
                  if (state) setRevision(state.revision);
                }
              })
              .finally(() => setBusy(false));
          }}
        >
          {busy ? "正在处理…" : local ? "导入" : "打开"}
        </button>
      </div>
    </Modal>
  );
}
