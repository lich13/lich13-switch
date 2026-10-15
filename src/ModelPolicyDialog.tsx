import type { EditRevision } from "./gateway-edit";
import type { ClientId } from "./types";
import { confirmAction } from "./confirmation";
import { useEffect, useRef, useState } from "react";
import { RefreshCw, Search, X } from "lucide-react";
import { command } from "./bridge";
import { errorOf, type ModelCatalog, type Provider } from "./types";

export default function ModelPolicyDialog({
  clientId = "codex",
  provider,
  version,
  revision,
  close,
  save,
  onDirtyChange,
}: {
  clientId?: ClientId;
  provider: Provider;
  version: string;
  revision: string;
  close: () => void;
  save: (models: string[] | null, revision: EditRevision) => Promise<void>;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [limited, setLimited] = useState(provider.allowedModels != null);
  const [selected, setSelected] = useState(provider.allowedModels ?? []);
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null);
  const [query, setQuery] = useState("");
  const [custom, setCustom] = useState("");
  const [error, setError] = useState("");
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const generation = useRef(0);
  const dialog = useRef<HTMLDialogElement>(null);
  const baseline = useRef<EditRevision>(revision);
  useEffect(() => {
    dialog.current?.showModal();
  }, []);
  const initial = useRef(JSON.stringify(provider.allowedModels ?? null));
  const dirty =
    Boolean(custom) ||
    JSON.stringify(limited ? selected : null) !== initial.current;
  const request = async (force: boolean) => {
    const current = ++generation.current;
    setLoading(true);
    try {
      const result = await command("list_provider_models", {
        clientId,
        providerId: provider.id,
        force,
      });
      if (current === generation.current) {
        setCatalog(result);
        setError("");
      }
    } catch (e) {
      if (current === generation.current) setError(errorOf(e).message);
    } finally {
      if (current === generation.current) setLoading(false);
    }
  };
  useEffect(() => {
    setCatalog(null);
    void request(false);
    return () => {
      generation.current++;
    };
  }, [provider.id, version]);
  useEffect(() => {
    onDirtyChange?.(dirty);
    return () => onDirtyChange?.(false);
  }, [dirty, onDirtyChange]);
  const cancel = async () => {
    if (!dirty || (await confirmAction("丢弃未保存的模型白名单？"))) close();
  };
  const models = [...new Set([...selected, ...(catalog?.models ?? [])])].filter(
    (m) => m.toLowerCase().includes(query.toLowerCase()),
  );
  const add = () => {
    const values = custom
      .split(/[\n,，]/)
      .map((m) => m.trim())
      .filter(Boolean);
    if (values.some((m) => m.length > 256 || /[\x00-\x1f\x7f*]/.test(m))) {
      setError("请输入完整模型 ID，不支持通配符");
      return;
    }
    setSelected((previous) => [...new Set([...previous, ...values])]);
    setLimited(true);
    setCustom("");
    setError("");
  };
  return (
    <dialog
      ref={dialog}
      className="modal model-dialog"
      aria-label="模型白名单"
      onCancel={(e) => {
        e.preventDefault();
        if (!saving) cancel();
      }}
    >
      <form
        className="gateway-form"
        aria-label="模型白名单"
        onSubmit={(e) => {
          e.preventDefault();
          if (custom.trim()) {
            setError("请先添加手填模型，或清空输入");
            return;
          }
          if (limited && !selected.length) {
            setError("白名单至少选择一个模型");
            return;
          }
          setSaving(true);
          setError("");
          void save(limited ? selected : null, baseline.current)
            .catch((e) => {
              baseline.current = null;
              setError(errorOf(e).message);
            })
            .finally(() => setSaving(false));
        }}
      >
        <div className="modal-heading">
          <h2>模型白名单</h2>
          <button
            type="button"
            className="icon-button"
            aria-label="关闭模型白名单"
            onClick={cancel}
          >
            <X size={18} />
          </button>
        </div>
        <div className="model-provider" title={provider.name}>
          {provider.name}
        </div>
        <div className="segmented" role="group" aria-label="模型规则">
          <button
            type="button"
            aria-pressed={!limited}
            className={!limited ? "active" : ""}
            onClick={() => setLimited(false)}
          >
            不限模型
          </button>
          <button
            type="button"
            aria-pressed={limited}
            className={limited ? "active" : ""}
            onClick={() => setLimited(true)}
          >
            白名单
          </button>
        </div>
        <div className="model-search">
          <Search size={15} />
          <input
            autoFocus
            aria-label="搜索模型"
            placeholder="搜索模型 ID"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
          />
          <button
            type="button"
            className="icon-button"
            aria-label="刷新模型列表"
            disabled={
              loading ||
              Boolean(catalog?.retryAt && catalog.retryAt > Date.now() / 1000)
            }
            onClick={() => void request(true)}
          >
            <RefreshCw size={15} className={loading ? "spin" : ""} />
          </button>
        </div>
        {catalog?.error && (
          <div className="form-error" role="status">
            {catalog.error}
            {catalog.stale && catalog.models.length ? " · 显示上次列表" : ""}
          </div>
        )}
        <div className="model-options" aria-label="可选模型">
          {!models.length && (
            <div className="model-empty">
              {loading ? "正在读取模型…" : "没有匹配模型，可手动添加"}
            </div>
          )}
          {models.map((model) => (
            <label key={model}>
              <input
                type="checkbox"
                checked={selected.includes(model)}
                onChange={(e) => {
                  setLimited(true);
                  setSelected(
                    e.target.checked
                      ? [...selected, model]
                      : selected.filter((m) => m !== model),
                  );
                }}
              />
              <span title={model}>{model}</span>
            </label>
          ))}
        </div>
        <label className="model-manual">
          手动添加模型 ID
          <textarea
            rows={2}
            placeholder="每行一个模型 ID"
            value={custom}
            onChange={(e) => setCustom(e.target.value)}
          />
        </label>
        <button
          type="button"
          className="text-button"
          disabled={!custom.trim()}
          onClick={add}
        >
          添加到白名单
        </button>
        <div className="model-policy-hint">
          {limited
            ? `已选 ${selected.length} 个模型 · 精确匹配`
            : "允许所有模型"}{" "}
          · 仅网关运行时生效
        </div>
        {error && (
          <div className="form-error" role="alert">
            {error}
          </div>
        )}
        <div className="dialog-actions">
          <button type="button" className="secondary" onClick={cancel}>
            取消
          </button>
          <button className="primary" disabled={saving}>
            {saving ? "保存中…" : baseline.current === null ? "重试" : "保存"}
          </button>
        </div>
      </form>
    </dialog>
  );
}
