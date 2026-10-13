import { useEffect, useRef, useState } from "react";
import { Download, Upload, RefreshCw } from "lucide-react";
import Modal from "./Modal";
import { command } from "./bridge";
import { errorOf, type ClientId, type GatewayState } from "./types";
import TemplateChoice, { type PreparedTemplate } from "./TemplateChoice";

type Preview = {
  token: string;
  revision: string;
  unsupportedProxies: number;
  template?: PreparedTemplate | null;
  rows: {
    id: number;
    name: string;
    baseUrl: string | null;
    concurrency: number | null;
    error: string | null;
  }[];
};
export default function ProviderExchange({
  mode,
  state,
  clientId,
  close,
  update,
  notify,
  onDirtyChange,
}: {
  mode: "import" | "export";
  state: GatewayState;
  clientId: ClientId;
  close: () => void;
  update: (s: GatewayState) => void;
  notify: (s: string) => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [preview, setPreview] = useState<Preview | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [revision, setRevision] = useState(state.revision);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [template, setTemplate] = useState<PreparedTemplate | null>(null);
  const templateToken = useRef<string | null>(null);
  const token = useRef<string | null>(null);
  const started = useRef(false);
  const mounted = useRef(true);
  const choose = async () => {
    setBusy(true);
    setError("");
    try {
      if (mode === "import") {
        const next = await command<Preview | null>("preview_sub2api_import", {
          clientId,
        });
        if (!mounted.current) {
          if (next?.template?.token)
            void command("cancel_sub2api_import", { clientId, token: next.template.token });
          if (next)
            void command("cancel_sub2api_import", {
              clientId,
              token: next.token,
            });
          return;
        }
        if (next) {
          token.current = next.token;
          setPreview(next);
          setRevision(next.revision);
          setSelected(
            next.rows.filter((r) => !r.error).map((r) => String(r.id)),
          );
          setTemplate(next.template ?? null);
          templateToken.current = next.template?.token ?? null;
          setReady(!next.template?.token);
        }
      } else {
        const ok = await command<PreparedTemplate | null>("prepare_sub2api_export", {
          clientId,
        });
        if (!mounted.current && ok?.token) {
          void command("cancel_sub2api_import", { clientId, token: ok.token });
          return;
        }
        if (mounted.current && ok) {
          setTemplate(ok);
          templateToken.current = ok.token;
          setReady(ok.ready);
          setSelected(state.providers.map((p) => p.id));
        }
      }
    } catch (e) {
      if (mounted.current) setError(errorOf(e).message);
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  useEffect(() => {
    mounted.current = true;
    if (!started.current) {
      started.current = true;
      void choose();
    }
    return () => {
      mounted.current = false;
      if (token.current)
        void command("cancel_sub2api_import", {
          clientId,
          token: token.current,
        });
      if (templateToken.current)
        void command("cancel_sub2api_import", { clientId, token: templateToken.current });
      onDirtyChange?.(false);
    };
  }, []);
  useEffect(() => {
    onDirtyChange?.(busy || selected.length > 0);
  }, [busy, selected, onDirtyChange]);
  const rows =
    mode === "import"
      ? (preview?.rows ?? []).map((r) => ({ ...r, id: String(r.id) }))
      : state.providers.map((p) => ({
          id: p.id,
          name: p.name,
          baseUrl: p.baseUrl,
          concurrency: p.maxConcurrency,
          error: null,
        }));
  const available = rows.filter((r) => !r.error).map((r) => r.id);
  const save = async () => {
    if (busy || !selected.length) return;
    setBusy(true);
    setError("");
    try {
      if (mode === "import") {
        const next = await command<GatewayState>("commit_sub2api_import", {
          clientId,
          token: preview?.token,
          ids: selected.map(Number),
          expectedRevision: revision,
        });
        token.current = null;
        update(next);
        notify(`已导入 ${selected.length} 个供应商`);
        close();
      } else {
        const saved = await command<boolean>("export_sub2api_providers", {
          clientId,
          ids: selected,
          expectedRevision: revision,
        });
        if (saved) {
          notify(`已导出 ${selected.length} 个供应商`);
          close();
        }
      }
    } catch (e) {
      const failure = errorOf(e);
      setError(failure.message);
      if (failure.code === "CONFLICT") {
        try {
          const latest = await command<GatewayState>("get_gateway", {
            clientId,
          });
          update(latest);
          setRevision(latest.revision);
        } catch {
          /* Keep the original actionable conflict and selection. */
        }
      }
    } finally {
      if (mounted.current) setBusy(false);
    }
  };
  return (
    <Modal
      title={mode === "import" ? "导入 Sub2API" : "导出 Sub2API"}
      close={close}
      busy={busy}
    >
      {error && (
        <div className="inline-error" role="alert">
          {error}
        </div>
      )}
      {mode === "import" && Boolean(preview?.unsupportedProxies) && (
        <div role="status" className="inline-error">
          {preview?.unsupportedProxies} 条代理记录不支持导入
        </div>
      )}
      {template?.token && <TemplateChoice key={template.token} template={template} busy={busy} select={id => {
        setBusy(true);
        setError("");
        void command("select_sub2api_template", { clientId, token: template.token, id })
          .then(() => { templateToken.current = null; setTemplate(null); setReady(true); })
          .catch(e => setError(errorOf(e).message)).finally(() => setBusy(false));
      }} />}
      {ready && (
        <>
          <label className="exchange-select-all">
            <input
              type="checkbox"
              disabled={busy || !available.length}
              checked={
                available.length > 0 &&
                available.every((id) => selected.includes(id))
              }
              onChange={(e) => setSelected(e.target.checked ? available : [])}
            />
            全选
            <span>
              {selected.length} / {available.length}
            </span>
          </label>
          <div className="exchange-list">
            {rows.map((row) => (
              <label className="exchange-row" key={row.id}>
                <input
                  type="checkbox"
                  disabled={busy || Boolean(row.error)}
                  checked={selected.includes(row.id)}
                  onChange={(e) =>
                    setSelected((s) =>
                      e.target.checked
                        ? [...s, row.id]
                        : s.filter((id) => id !== row.id),
                    )
                  }
                />
                <span>
                  <strong title={row.name}>{row.name}</strong>
                  <span title={row.baseUrl ?? undefined}>
                    {row.error ?? row.baseUrl}
                  </span>
                </span>
                {!row.error && (
                  <span className="mono">并发 {row.concurrency || "∞"}</span>
                )}
              </label>
            ))}
          </div>
        </>
      )}
      <div className="form-actions exchange-actions">
        {mode === "import" || !ready ? (
          <button disabled={busy} onClick={() => void choose()}>
            <RefreshCw size={14} />
            选择{mode === "import" ? "文件" : "模板"}
          </button>
        ) : (
          <button
            disabled={busy}
            onClick={() => {
              setBusy(true);
              void command<PreparedTemplate | null>("prepare_sub2api_export", {
                clientId,
                replaceTemplate: true,
              })
                .then(next => {
                  if (!mounted.current) { if (next?.token) void command("cancel_sub2api_import", { clientId, token: next.token }); return; }
                  if (next) { setTemplate(next); templateToken.current = next.token; setReady(next.ready); }
                })
                .catch((e) => setError(errorOf(e).message))
                .finally(() => setBusy(false));
            }}
          >
            更换模板
          </button>
        )}
        <button onClick={close} disabled={busy}>
          取消
        </button>
        <button
          className="primary"
          disabled={busy || !ready || !selected.length}
          onClick={() => void save()}
        >
          {mode === "import" ? <Download size={14} /> : <Upload size={14} />}
          {error ? "重试" : mode === "import" ? "导入所选" : "导出所选"}
        </button>
      </div>
    </Modal>
  );
}
