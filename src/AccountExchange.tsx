import { useEffect, useRef, useState } from "react";
import Modal from "./Modal";
import TemplateChoice, { type PreparedTemplate } from "./TemplateChoice";
import { command } from "./bridge";
import { errorOf, type ViewState } from "./types";

type Row = { id: number; name: string; format: string; identityHint: string | null; duplicate: boolean; error: string | null };
type Preview = { token: string; revision: string; rows: Row[] };
type ExportPreview = { revision: string; rows: { id: string; name: string; error: string | null }[] };
const formatName = (format: string) => ({ codex: "Codex", sub2api: "Sub2API", cliproxyapi: "CLIProxyAPI" }[format] ?? format);

export default function AccountExchange({ mode, close, update, notify }: {
  mode: "import" | "export"; close: () => void; update: (state: ViewState) => void; notify: (message: string) => void;
}) {
  const [preview, setPreview] = useState<Preview | null>(null);
  const [exportPreview, setExportPreview] = useState<ExportPreview | null>(null);
  const [selected, setSelected] = useState<string[]>([]);
  const [format, setFormat] = useState("sub2api");
  const [template, setTemplate] = useState<PreparedTemplate | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [stale, setStale] = useState(false);
  const token = useRef<string | null>(null), templateToken = useRef<string | null>(null);
  const mounted = useRef(true), started = useRef(false), submitting = useRef(false);
  const perform = async (work: () => Promise<void>) => {
    if (submitting.current) return;
    submitting.current = true;
    setBusy(true); setError("");
    try { await work(); } catch (e) { if (mounted.current) { const failure = errorOf(e); setError(failure.message); if (failure.code === "CONFLICT") setStale(true); } }
    finally { submitting.current = false; if (mounted.current) setBusy(false); }
  };
  const choose = () => perform(async () => {
    if (mode === "import") {
      const next = await command<Preview | null>("preview_account_import");
      if (!next) return;
      if (!mounted.current) { void command("cancel_account_exchange", { token: next.token }); return; }
      token.current = next.token;
      setPreview(next);
      setStale(false);
      setSelected(next.rows.filter(r => !r.error && !r.duplicate).map(r => String(r.id)));
    } else {
      const next = await command<ExportPreview>("preview_account_export");
      if (!mounted.current) return;
      setExportPreview(next);
      setStale(false);
      setSelected(next.rows.filter(r => !r.error).map(r => r.id));
    }
  });
  useEffect(() => {
    mounted.current = true;
    if (!started.current) { started.current = true; void choose(); }
    return () => {
      mounted.current = false;
      for (const value of [token.current, templateToken.current])
        if (value) void command("cancel_account_exchange", { token: value });
    };
  }, []);
  const rows = mode === "import" ? (preview?.rows ?? []).map(r => ({ ...r, id: String(r.id), description:
    r.error ?? `${formatName(r.format)} · ${r.identityHint}${r.duplicate ? " · 勾选将更新已存在账号" : ""}` }))
    : (exportPreview?.rows ?? []).map(r => ({ ...r, duplicate: false, description: r.error ?? "ChatGPT / Codex" }));
  const available = rows.filter(r => !r.error && !r.duplicate).map(r => r.id);
  const prepare = async (replaceTemplate = false) => {
    const next = await command<PreparedTemplate | null>("prepare_account_template", { ids: selected, replaceTemplate });
    if (!mounted.current) {
      if (next?.token) void command("cancel_account_exchange", { token: next.token });
      return false;
    }
    if (next) { setTemplate(next); templateToken.current = next.token; }
    return next?.ready === true;
  };
  const save = () => perform(async () => {
    if (!selected.length) return;
    if (mode === "import" && preview) {
      const next = await command<ViewState>("commit_account_import", { token: preview.token, expectedRevision: preview.revision,
        selected: selected.map(id => ({ id: Number(id), update: preview.rows.find(r => r.id === Number(id))?.duplicate === true })) });
      token.current = null; update(next); notify(`已保存 ${selected.length} 个账号`); close();
    } else if (exportPreview) {
      if (format === "sub2api" && !(await prepare())) return;
      if (await command<boolean>("export_accounts", { ids: selected, expectedRevision: exportPreview.revision, format })) {
        notify(`已导出 ${selected.length} 个账号`); close();
      }
    }
  });
  const changeFormat = (next: string) => perform(async () => {
    if (templateToken.current) await command("cancel_account_exchange", { token: templateToken.current });
    templateToken.current = null;
    setTemplate(null);
    setFormat(next);
  });
  return <Modal title={mode === "import" ? "导入账号" : "导出账号"} close={close} busy={busy}>
    {error && <div className="inline-error" role="alert">{error}</div>}
    {mode === "export" && <label className="field">导出格式
      <select value={format} disabled={busy} onChange={e => void changeFormat(e.target.value)}>
        <option value="sub2api">Sub2API OAuth · 单文件</option>
        <option value="cliproxyapi">CLIProxyAPI Codex · 每账号一文件</option>
      </select>
    </label>}
    {template?.token ? <TemplateChoice key={template.token} template={template} busy={busy} select={id => void perform(async () => {
      await command("select_account_template", { token: template.token, id });
      templateToken.current = null; setTemplate(null);
    })} /> : <>
      {mode === "import" && preview?.rows.some(r => r.duplicate) && <p className="muted">重复账号默认跳过，勾选后更新其凭据。</p>}
      <label className="exchange-select-all"><input type="checkbox" disabled={busy || !available.length}
        checked={available.length > 0 && available.every(id => selected.includes(id))}
        onChange={e => setSelected(e.target.checked ? available : [])} />
        {mode === "import" ? "全选新账号" : "全选"}<span>已选 {selected.length}</span>
      </label>
      <div className="exchange-list">{rows.map(row => <label key={row.id} className="exchange-row">
        <input type="checkbox" disabled={busy || !!row.error} checked={selected.includes(row.id)}
          onChange={e => setSelected(s => e.target.checked ? [...s, row.id] : s.filter(id => id !== row.id))} />
        <span><strong title={row.name}>{row.name}</strong><span className={row.error ? "exchange-error" : undefined} title={row.description}>{row.description}</span></span>
      </label>)}</div>
    </>}
    <div className="form-actions exchange-actions">
      <button disabled={busy} onClick={() => void choose()}>{mode === "import" ? "选择文件" : "刷新账号列表"}</button>
      {mode === "export" && format === "sub2api" && <button disabled={busy} onClick={() => void perform(async () => { await prepare(true); })}>更换 OAuth 模板</button>}
      <button disabled={busy} onClick={close}>取消</button>
      <button className="primary" disabled={busy || stale || !selected.length || !!template?.token} onClick={() => void save()}>
        {mode === "import" ? "保存所选账号" : "导出所选"}
      </button>
    </div>
  </Modal>;
}
