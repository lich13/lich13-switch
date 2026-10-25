import { subscriptions } from "./subscriptions";
import { useCallback, useEffect, useState } from "react";
import { command } from "./bridge";
import { errorOf, type ClientId, type GrokProtocol } from "./types";
import { clients, clientName } from "./ClientSelection";
import { clientProtocols, protocolNames } from "./client-protocols";
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
    [clientId, setClientId] = useState<ClientId | "">(""),
    [protocol, setProtocol] = useState<GrokProtocol | "">(""),
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
      setClientId(queue[0].provider?.clientId ?? "");
      const provider = queue[0].provider;
      const choices = provider?.clientId
        ? clientProtocols[provider.clientId]
        : [];
      setProtocol(
        provider?.protocol &&
          (!choices.length || choices.includes(provider.protocol))
          ? provider.protocol
          : (choices[0] ?? ""),
      );
      setActive(queue[0]);
    }
  }, [queue, active, available, blocked]);
  useEffect(() => {
    setRevision("");
    if (active?.provider && clientId) {
      let disposed = false;
      void command("get_gateway", { clientId })
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
  }, [active, clientId]);
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
            <dt>
              <label htmlFor="import-client">平台</label>
            </dt>
            <dd>
              <select
                id="import-client"
                value={clientId}
                disabled={busy}
                onChange={(e) => {
                  const next = e.target.value as ClientId;
                  setClientId(next);
                  setRevision("");
                  setError("");
                  if (!protocol || !clientProtocols[next].includes(protocol))
                    setProtocol(clientProtocols[next][0]);
                }}
              >
                <option value="" disabled>
                  选择平台
                </option>
                {clients.map((id) => (
                  <option key={id} value={id}>
                    {clientName(id)}
                  </option>
                ))}
              </select>
            </dd>
          </div>
          <div>
            <dt>
              <label htmlFor="import-protocol">协议</label>
            </dt>
            <dd>
              <select
                id="import-protocol"
                value={protocol}
                disabled={busy || !clientId}
                onChange={(e) => setProtocol(e.target.value as GrokProtocol)}
              >
                <option value="" disabled>
                  选择协议
                </option>
                {protocol &&
                  clientId &&
                  !clientProtocols[clientId].includes(protocol) && (
                    <option value={protocol} disabled>
                      {protocolNames[protocol]}（不兼容）
                    </option>
                  )}
                {(clientId ? clientProtocols[clientId] : []).map((p) => (
                  <option key={p} value={p}>
                    {protocolNames[p]}
                  </option>
                ))}
              </select>
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
          disabled={
            busy ||
            !active.provider ||
            !revision ||
            !clientId ||
            !protocol ||
            !clientProtocols[clientId].includes(protocol)
          }
          onClick={() => {
            if (busy || !clientId || !protocol) return;
            setBusy(true);
            setError("");
            void command("confirm_provider_import", {
              id: active.id,
              clientId,
              protocol,
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
                    clientId,
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
