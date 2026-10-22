import { useCallback, useEffect, useRef, useState } from "react";
import { Eye, EyeOff, LoaderCircle } from "lucide-react";
import { subscriptions } from "./subscriptions";
import { command } from "./bridge";
import { errorOf, type ClientId, type Provider } from "./types";
import type { GatewayEdit } from "./bridge-contracts";
import type { EditRevision } from "./gateway-edit";
import Modal from "./Modal";

export default function GatewayDialog({
  clientId,
  connectionMode,
  dialog,
  revision,
  close,
  save,
  onDirtyChange,
}: {
  clientId: ClientId;
  connectionMode?: "bearer" | "apiKey";
  dialog: { kind: "provider"; item?: Provider };
  close: () => void;
  revision: string;
  save: (p: GatewayEdit, revision: EditRevision) => Promise<void>;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const baseline = useRef<EditRevision>(revision);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const [name, setName] = useState(dialog.item ? dialog.item.name : ""),
    [baseUrl, setBaseUrl] = useState(dialog.item?.baseUrl ?? ""),
    [token, setToken] = useState("");
  const [protocol, setProtocol] = useState(
    clientId === "dsh"
      ? "chat_completions"
      : (dialog.item?.protocol ?? "responses"),
  );
  const [showToken, setShowToken] = useState(false);
  const [revealed, setRevealed] = useState<{
    value: string;
    revision: string;
  } | null>(null);
  const [revealing, setRevealing] = useState(false);
  const revealSequence = useRef(0);
  const currentRevision = useRef(revision);
  currentRevision.current = revision;
  const hideToken = useCallback(() => {
    revealSequence.current += 1;
    setShowToken(false);
    setRevealed(null);
    setRevealing(false);
  }, []);
  useEffect(() => {
    hideToken();
    const events = subscriptions((e) => setError(errorOf(e).message));
    const visibility = () => {
      if (document.visibilityState === "hidden") hideToken();
    };
    document.addEventListener("visibilitychange", visibility);
    events.watch("app-visibility", (visible) => {
      if (!visible) hideToken();
    });
    return () => {
      revealSequence.current += 1;
      events.dispose();
      document.removeEventListener("visibilitychange", visibility);
    };
  }, [clientId, dialog.item?.id, revision, hideToken]);
  const toggleToken = async () => {
    if (showToken) {
      hideToken();
      return;
    }
    setError("");
    setShowToken(true);
    if (token || !dialog.item) return;
    const sequence = ++revealSequence.current;
    setRevealing(true);
    try {
      const value = await command("reveal_provider_token", {
        clientId,
        providerId: dialog.item.id,
        expectedRevision: revision,
      });
      if (
        sequence === revealSequence.current &&
        currentRevision.current === revision
      ) {
        setRevealed({ value, revision });
      }
    } catch (e) {
      if (
        sequence === revealSequence.current &&
        currentRevision.current === revision
      ) {
        setShowToken(false);
        setError(errorOf(e).message);
      }
    } finally {
      if (sequence === revealSequence.current) setRevealing(false);
    }
  };
  const initialDraft = useRef(JSON.stringify([name, baseUrl, token, protocol]));
  useEffect(() => {
    onDirtyChange?.(
      JSON.stringify([name, baseUrl, token, protocol]) !== initialDraft.current,
    );
  }, [name, baseUrl, token, protocol, onDirtyChange]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const title = `${dialog.item ? "编辑" : "添加"}供应商`;
  return (
    <Modal title={title} close={close} busy={busy}>
      <form
        className="gateway-form"
        onSubmit={(e) => {
          e.preventDefault();
          setError("");
          setBusy(true);
          const draft = {
            id: dialog.item?.id ?? null,
            baseUrl,
            token,
            ...(!dialog.item ? { name } : {}),
          };
          const payload: GatewayEdit =
            clientId === "grok" || clientId === "dsh"
              ? {
                  op:
                    clientId === "dsh" ? "saveDshProvider" : "saveGrokProvider",
                  ...draft,
                  protocol,
                }
              : { op: "saveProvider", ...draft };
          void save(payload, baseline.current)
            .catch((e) => {
              baseline.current = null;
              setError(errorOf(e).message);
            })
            .finally(() => setBusy(false));
        }}
      >
        <>
          {!dialog.item && (
            <label>
              名称
              <input
                type="text"
                maxLength={120}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="可选"
              />
            </label>
          )}
          {clientId === "grok" && (
            <label>
              API 协议
              <select
                value={protocol}
                onChange={(e) => setProtocol(e.target.value as typeof protocol)}
              >
                <option value="responses">Responses</option>
                <option value="chat_completions">Chat Completions</option>
                <option value="messages">Messages</option>
              </select>
            </label>
          )}
          <label>
            {clientId === "claude" ? "ANTHROPIC_BASE_URL" : "base_url"}
            <input
              required
              type="url"
              autoFocus
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder={
                clientId === "claude"
                  ? "https://api.example.com"
                  : "https://api.example.com/v1"
              }
            />
          </label>
          <label>
            {clientId === "claude"
              ? "ANTHROPIC_AUTH_TOKEN"
              : clientId === "grok" || clientId === "dsh"
                ? "api_key"
                : connectionMode === "apiKey"
                  ? "OPENAI_API_KEY"
                  : "experimental_bearer_token"}
            <span className="provider-token-input">
              <input
                required={!dialog.item}
                type={showToken ? "text" : "password"}
                autoComplete="new-password"
                spellCheck={false}
                value={
                  showToken && revealed?.revision === revision
                    ? revealed.value
                    : token
                }
                onChange={(e) => {
                  revealSequence.current += 1;
                  setRevealed(null);
                  setRevealing(false);
                  setToken(e.target.value);
                }}
                placeholder={dialog.item ? "留空保留当前 Token" : "输入 Token"}
              />
              <button
                type="button"
                className="icon-button"
                aria-label={showToken ? "隐藏 Token" : "显示 Token"}
                aria-pressed={showToken}
                aria-busy={revealing}
                onClick={() => void toggleToken()}
              >
                {revealing ? (
                  <LoaderCircle size={16} className="spin" aria-hidden="true" />
                ) : showToken ? (
                  <EyeOff size={16} aria-hidden="true" />
                ) : (
                  <Eye size={16} aria-hidden="true" />
                )}
              </button>
            </span>
          </label>
        </>
        {error && (
          <div className="banner error" role="alert">
            {error}
          </div>
        )}
        <div className="form-actions">
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={close}
          >
            取消
          </button>
          <button className="primary" disabled={busy}>
            {busy && <LoaderCircle size={15} className="spin" />}
            {baseline.current === null ? "重试" : "保存"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
