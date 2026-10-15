import type { GatewayEdit } from "./bridge-contracts";
import { useEffect, useRef, useState } from "react";
import { ArrowUpDown, Plus, RefreshCw, Trash2 } from "lucide-react";
import { command } from "./bridge";
import { confirmAction } from "./confirmation";
import type { EditRevision } from "./gateway-edit";
import Modal from "./Modal";
import { errorOf, type ClientId, type GatewayState, type Provider } from "./types";

export default function ProviderEndpoints({ provider, runtime, clientId, revision, save, update, close, onDirtyChange }: {
  provider: Provider; runtime: Provider; clientId: ClientId; revision: string;
  save: (edit: GatewayEdit, revision: EditRevision) => Promise<void>;
  update: (state: GatewayState) => void; close: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [urls, setUrls] = useState(provider.extraBaseUrls ?? []);
  const [saved, setSaved] = useState(JSON.stringify(provider.extraBaseUrls ?? []));
  const [busy, setBusy] = useState(false);
  const [probing, setProbing] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [failedSwap, setFailedSwap] = useState<string | null>(null);
  const baseline = useRef<EditRevision>(revision);
  const mounted = useRef(true);
  const dirty = JSON.stringify(urls) !== saved;
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  useEffect(() => { onDirtyChange?.(dirty); }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  useEffect(() => {
    if (!dirty && !busy && !error) {
      const values = runtime.extraBaseUrls ?? [];
      setUrls(values); setSaved(JSON.stringify(values)); baseline.current = revision;
    }
  }, [JSON.stringify(runtime.extraBaseUrls), runtime.baseUrl, revision, dirty, busy, error]);
  const leave = async () => {
    if (!busy && (!dirty || await confirmAction("放弃未保存的端点修改？"))) close();
  };
  const submit = async (edit: GatewayEdit, closing: boolean) => {
    if (busy) return;
    setBusy(true); setError(""); setNotice("");
    try {
      await save(edit, baseline.current);
      setFailedSwap(null);
      if (closing) { setSaved(JSON.stringify(urls)); close(); }
      else setNotice("主副端点已互换");
    } catch (e) { baseline.current = null; setFailedSwap(edit.op === "swapEndpoint" ? String(edit.baseUrl) : null); setError(errorOf(e).message); }
    finally { if (mounted.current) setBusy(false); }
  };
  const measure = async () => {
    setProbing(true); setError("");
    try {
      const state = await command("probe_provider_endpoints", { clientId, providerId: provider.id });
      if (mounted.current) update(state);
    } catch (e) { if (mounted.current) setError(errorOf(e).message); }
    finally { if (mounted.current) setProbing(false); }
  };
  const result = (url: string) => {
    const row = runtime.endpointMeasurements?.find((v) => v.baseUrl.replace(/\/$/, "") === url.replace(/\/$/, ""));
    if (!row) return <span className="endpoint-result">—</span>;
    const errors: Record<string, string> = { PROBE_TIMEOUT: "测速超时", CONNECT_TIMEOUT: "连接超时", TLS_HANDSHAKE_FAILED: "TLS 失败", GATEWAY_LOOP: "网关回环", CONNECTION_FAILED: "连接失败" };
    return <span className="endpoint-result" title={`${row.status ? `HTTP ${row.status}` : row.error ?? ""} · ${new Date(row.checkedAt).toLocaleTimeString()}`}>
      {row.latencyMs === null ? errors[row.error ?? ""] ?? "测速失败" : `${row.latencyMs} ms`}
      {row.status !== null && <span>HTTP {row.status}</span>}
    </span>;
  };
  return <Modal compact title={`${provider.name} · 端点`} close={() => void leave()} busy={busy}>
    <form className="gateway-form endpoint-form" data-provider-draft={dirty || undefined} onSubmit={(e) => {
      e.preventDefault();
      if (failedSwap !== null && !dirty) {
        void submit({ op: "swapEndpoint", id: provider.id, baseUrl: failedSwap }, false); return;
      }
      const values = urls.map((v) => v.trim());
      try {
        values.forEach((value) => {
          const parsed = new URL(value);
          if (!/^https?:$/.test(parsed.protocol) || parsed.username || parsed.password || parsed.search || parsed.hash || /[\x00-\x1f\x7f]/.test(value) || value.length > 2048) throw new Error();
        });
      } catch { setError("请输入有效的 HTTP 或 HTTPS base_url，不含凭据、查询或片段"); return; }
      void submit({ op: "endpointsProvider", id: provider.id, baseUrls: values }, true);
    }}>
      <div className="endpoint-toolbar"><strong>主 base_url</strong><button type="button" className="text-button" disabled={probing || busy || dirty} onClick={() => void measure()}><RefreshCw size={14} className={probing ? "spin" : ""} />{probing ? "测速中" : "测速"}</button></div>
      <div className="endpoint-primary"><span title={runtime.baseUrl}>{runtime.baseUrl}</span>{result(runtime.baseUrl)}</div>
      <div className="endpoint-toolbar"><strong>辅助 base_url</strong><button type="button" className="text-button" disabled={busy || urls.length >= 8} onClick={() => setUrls([...urls, ""])}><Plus size={14} />添加</button></div>
      <div className="endpoint-list">{urls.map((url, index) => <div className="endpoint-row" key={index}>
        <input type="url" required aria-label={`辅助 base_url ${index + 1}`} placeholder="https://api.example.invalid/v1" value={url} disabled={busy} onChange={(e) => setUrls(urls.map((v, i) => i === index ? e.target.value : v))} />
        <div className="endpoint-row-actions">{result(url)}<button type="button" className="icon-button compact" aria-label={`辅助端点 ${index + 1} 设为主端点`} title="与主端点互换" disabled={busy || dirty} onClick={() => void submit({ op: "swapEndpoint", id: provider.id, baseUrl: url }, false)}><ArrowUpDown size={15} /></button><button type="button" className="icon-button compact" aria-label={`删除辅助端点 ${index + 1}`} title="删除" disabled={busy} onClick={() => setUrls(urls.filter((_, i) => i !== index))}><Trash2 size={15} /></button></div>
      </div>)}</div>
      {runtime.preferredBaseUrl && runtime.endpointMeasurements?.length ? <div className="endpoint-preferred"><span>新连接优选</span><span title={runtime.preferredBaseUrl}>{runtime.preferredBaseUrl}</span></div> : null}
      {error && <div className="form-error" role="alert">{error}</div>}
      {notice && <div role="status">{notice}</div>}
      <div className="dialog-actions"><button type="button" className="secondary" disabled={busy} onClick={() => void leave()}>取消</button><button className="primary" disabled={busy}>{busy ? "保存中…" : baseline.current === null ? "重试" : "保存"}</button></div>
    </form>
  </Modal>;
}
