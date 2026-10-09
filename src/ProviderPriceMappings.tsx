import { useEffect, useRef, useState } from "react";
import { Plus, Trash2 } from "lucide-react";
import { command } from "./bridge";
import { Drawer } from "./Usage";
import { errorOf, type ClientId, type GatewayState } from "./types";
import type { PricingConfig, PricingView, ProviderPriceMapping } from "./usage-types";
import { confirmAction } from "./confirmation";
export default function ProviderPriceMappings({ view, busy, save, close }: {
  view: PricingView; busy: boolean;
  save: (config: PricingConfig, revision?: string) => Promise<void>; close: () => void;
}) {
  const [draft, setDraft] = useState<ProviderPriceMapping[]>(() => structuredClone(view.config.providerMappings ?? []));
  const original = useRef(JSON.stringify(draft));
  const baseline = useRef(view.revision);
  const [retry, setRetry] = useState(false), [error, setError] = useState("");
  const [providers, setProviders] = useState<Record<ClientId, { id: string; name: string }[]>>({ codex: [], claude: [] });
  useEffect(() => {
    let disposed = false;
    void Promise.all((["codex", "claude"] as const).map(async (clientId) => {
      const state = await command<GatewayState>("get_gateway", { clientId });
      return [clientId, state.providers.map(({ id, name }) => ({ id, name }))] as const;
    })).then((values) => { if (!disposed) setProviders(Object.fromEntries(values) as typeof providers); })
      .catch((e) => { if (!disposed) setError(errorOf(e).message); });
    return () => { disposed = true; };
  }, []);
  const change = (index: number, patch: Partial<ProviderPriceMapping>) => setDraft(draft.map((rule, i) => i === index ? { ...rule, ...patch } : rule));
  const leave = async () => { if (!busy && (JSON.stringify(draft) === original.current || await confirmAction("放弃未保存的计价映射？"))) close(); };
  return <Drawer title="供应商计价映射" onClose={() => void leave()}>
    <form onSubmit={(e) => {
      e.preventDefault(); setError("");
      const rules = draft.map((r) => ({ ...r, fromModel: r.fromModel.trim(), toModel: r.toModel.trim() }));
      if (rules.some((r) => !r.provider || !r.fromModel || !r.toModel)) { setError("请填写供应商和完整模型 ID"); return; }
      void save({ ...view.config, providerMappings: rules }, retry ? view.revision : baseline.current)
        .then(close).catch((e) => { setError(errorOf(e).message); setRetry(true); });
    }}>
      <div className="price-mapping-list">
        {draft.map((rule, index) => <fieldset key={index} disabled={busy} className="price-mapping-rule">
          <div className="price-mapping-heading"><label><input type="checkbox" aria-label={`启用映射 ${index + 1}`} checked={rule.enabled} onChange={(e) => change(index, { enabled: e.target.checked })} />启用</label>
            <button type="button" className="icon-button" aria-label={`删除映射 ${index + 1}`} onClick={() => setDraft(draft.filter((_, i) => i !== index))}><Trash2 size={14} /></button></div>
          <div className="usage-price-fields"><label className="usage-field">客户端<select value={rule.client} onChange={(e) => change(index, { client: e.target.value as ClientId, provider: "" })}><option value="codex">Codex</option><option value="claude">Claude Code</option></select></label>
            <label className="usage-field">供应商<select value={rule.provider} onChange={(e) => change(index, { provider: e.target.value })}><option value="">选择供应商</option>
              {!providers[rule.client].some((p) => p.id === rule.provider) && rule.provider && <option value={rule.provider}>已删除供应商</option>}
              {providers[rule.client].map((p) => <option key={p.id} value={p.id}>{p.name}</option>)}</select></label></div>
          <label className="usage-field">匹配字段<select value={rule.matchOn} onChange={(e) => change(index, { matchOn: e.target.value as "request" | "response" })}><option value="request">请求模型</option><option value="response">收到的响应模型</option></select></label>
          <label className="usage-field">精确模型 ID<input required maxLength={256} value={rule.fromModel} onChange={(e) => change(index, { fromModel: e.target.value })} /></label>
          <label className="usage-field">计价模型<input required maxLength={256} list={`billing-model-${index}`} value={rule.toModel} onChange={(e) => change(index, { toModel: e.target.value })} />
            <datalist id={`billing-model-${index}`}>{Object.keys(view.models).filter((id) => id.toLowerCase().includes(rule.toModel.toLowerCase())).slice(0, 100).map((id) => <option key={id} value={id} />)}</datalist></label>
        </fieldset>)}
      </div>
      <button type="button" disabled={busy} onClick={() => setDraft([...draft, { client: "codex", provider: "", enabled: false, matchOn: "request", fromModel: "", toModel: "" }])}><Plus size={14} />添加映射</button>
      {error && <p role="alert" className="form-error">{error}</p>}
      <div className="usage-form-actions"><button type="button" disabled={busy} onClick={() => void leave()}>取消</button><button className="primary" disabled={busy}>{busy ? "保存中…" : retry ? "重试" : "保存"}</button></div>
    </form>
  </Drawer>;
}
