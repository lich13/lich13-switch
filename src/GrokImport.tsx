import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import type { GrokImportPreview } from "./bridge-contracts";
import { errorOf, type GatewayState } from "./types";
import Modal from "./Modal";

export default function GrokImport({
  close,
  update,
  notify,
}: {
  close: () => void;
  update: (state: GatewayState) => void;
  notify: (text: string) => void;
}) {
  const [preview, setPreview] = useState<GrokImportPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const generation = useRef(0),
    token = useRef<string | null>(null);
  async function load(selected?: string) {
    const current = ++generation.current;
    const previous = token.current;
    token.current = null;
    setPreview(null);
    setBusy(true);
    setError("");
    if (previous)
      void command("cancel_grok_import", { token: previous }).catch(() => {});
    try {
      const next = await command("preview_grok_import", {
        selected,
        useEnvironment: true,
      });
      if (current !== generation.current) {
        if (next.token)
          await command("cancel_grok_import", { token: next.token });
        return;
      }
      token.current = next.token;
      setPreview(next);
    } catch (e) {
      if (current === generation.current) setError(errorOf(e).message);
    } finally {
      if (current === generation.current) setBusy(false);
    }
  }
  useEffect(() => {
    void load();
    return () => {
      ++generation.current;
      if (token.current)
        void command("cancel_grok_import", { token: token.current }).catch(
          () => {},
        );
    };
  }, []);
  const model = preview?.models.find((m) => m.key === preview.selected);
  return (
    <Modal title="接入 Grok 当前配置" close={close} busy={busy} compact>
      <label>
        模型
        <select
          disabled={busy}
          value={preview?.selected ?? ""}
          onChange={(e) => void load(e.target.value)}
        >
          {!preview?.selected && <option value="">请选择模型</option>}
          {preview?.models.map((m) => (
            <option key={m.key} value={m.key}>
              {m.key}
            </option>
          ))}
        </select>
      </label>
      {model && (
        <dl className="import-summary">
          <dt>地址</dt>
          <dd>{model.address || "未读取到"}</dd>
          <dt>API Key</dt>
          <dd>{model.credentialAvailable ? "••••••••" : "未读取到"}</dd>
        </dl>
      )}
      <details>
        <summary>配置来源</summary>
        <dl className="import-summary">
          <dt>读取层</dt>
          <dd>{preview?.layers?.join(" → ") || "用户配置"}</dd>
          <dt>模型来源</dt>
          <dd>{preview?.modelSource || "未知"}</dd>
          <dt>协议</dt>
          <dd>{model?.protocol || "未识别"}</dd>
        </dl>
        {preview?.unknown?.map((message) => (
          <p key={message}>{message}</p>
        ))}
      </details>
      {(error || preview?.error || model?.error) && (
        <p className="form-error" role="alert">
          {error || preview?.error || model?.error}
        </p>
      )}
      <footer className="modal-actions">
        <button disabled={busy} onClick={close}>
          取消
        </button>
        <button
          className="primary"
          disabled={busy || !preview?.token || !!preview?.error}
          onClick={() => {
            if (!preview?.token) return;
            setBusy(true);
            setError("");
            void command("commit_grok_import", {
              token: preview.token,
              expectedRevision: preview.revision,
            })
              .then((state) => {
                token.current = null;
                update(state);
                notify("Grok 已导入并接入");
                close();
              })
              .catch((e) => setError(errorOf(e).message))
              .finally(() => setBusy(false));
          }}
        >
          导入并接入
        </button>
      </footer>
    </Modal>
  );
}
