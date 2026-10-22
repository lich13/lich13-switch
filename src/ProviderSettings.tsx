import DshModelMapping from "./DshModelMapping";
import type { GatewayEdit } from "./bridge-contracts";
import { useEffect, useRef, useState } from "react";
import { RefreshCw, Search } from "lucide-react";
import type { EditRevision } from "./gateway-edit";
import { confirmAction } from "./confirmation";
import { command } from "./bridge";
import Modal from "./Modal";
import { errorOf } from "./types";
import type { ClientId, ModelCatalog, Provider } from "./types";
function StandardProviderSettings({
  provider,
  runtime,
  clientId,
  revision,
  disabled = false,
  save,
  close,
  onDirtyChange,
}: {
  provider: Provider;
  runtime: Provider;
  clientId: ClientId;
  revision: string;
  disabled?: boolean;
  save: (edit: GatewayEdit, revision: EditRevision) => Promise<void>;
  close: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const [limited, setLimited] = useState(provider.allowedModels != null);
  const [selected, setSelected] = useState(provider.allowedModels ?? []);
  const [custom, setCustom] = useState("");
  const [query, setQuery] = useState("");
  const [catalog, setCatalog] = useState<ModelCatalog | null>(null);
  const [loading, setLoading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [catalogError, setCatalogError] = useState("");
  const baseline = useRef<EditRevision>(revision);
  const snapshot = (models: string[] | null) => JSON.stringify(models);
  const [saved, setSaved] = useState(snapshot(provider.allowedModels));
  const dirty = !!custom || saved !== snapshot(limited ? selected : null);
  const generation = useRef(0);
  useEffect(() => {
    if (!dirty && !saving && !error) {
      setLimited(runtime.allowedModels != null);
      setSelected(runtime.allowedModels ?? []);
      setSaved(snapshot(runtime.allowedModels));
      baseline.current = revision;
    }
  }, [JSON.stringify(runtime.allowedModels), revision, dirty, saving, error]);
  useEffect(() => {
    onDirtyChange?.(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const load = async (force: boolean) => {
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
        setCatalogError("");
      }
    } catch (e) {
      if (current === generation.current) setCatalogError(errorOf(e).message);
    } finally {
      if (current === generation.current) setLoading(false);
    }
  };
  useEffect(() => {
    setCatalog(null);
    void load(false);
    return () => {
      generation.current++;
    };
  }, [provider.id, clientId, runtime.quotaVersion]);
  const leave = async () => {
    if (!saving && (!dirty || (await confirmAction("放弃未保存的修改？"))))
      close();
  };
  const add = () => {
    const values = custom
      .split(/[\n,，]/)
      .map((v) => v.trim())
      .filter(Boolean);
    if (
      values.some(
        (v) =>
          new TextEncoder().encode(v).length > 256 ||
          /[\x00-\x1f\x7f*]/.test(v),
      )
    ) {
      setError("请输入完整模型 ID，不支持通配符");
      return;
    }
    setSelected((old) => [...new Set([...old, ...values])]);
    setLimited(true);
    setCustom("");
    setError("");
  };
  const models = [...new Set([...selected, ...(catalog?.models ?? [])])].filter(
    (v) => v.toLowerCase().includes(query.toLowerCase()),
  );
  return (
    <Modal
      title={`${provider.name} · 白名单`}
      close={() => void leave()}
      busy={saving}
    >
      <form
        className="gateway-form provider-settings"
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
          void save(
            {
              op: "policyProvider",
              id: provider.id,
              allowedModels: limited ? selected : null,
            },
            baseline.current,
          )
            .then(() => {
              setSaved(snapshot(limited ? selected : null));
              close();
            })
            .catch((e) => {
              baseline.current = null;
              setError(errorOf(e).message);
            })
            .finally(() => setSaving(false));
        }}
      >
        <div className="segmented" role="group" aria-label="模型规则">
          <button
            type="button"
            aria-pressed={!limited}
            className={!limited ? "active" : ""}
            disabled={saving}
            onClick={() => setLimited(false)}
          >
            不限模型
          </button>
          <button
            type="button"
            aria-pressed={limited}
            className={limited ? "active" : ""}
            disabled={saving}
            onClick={() => setLimited(true)}
          >
            白名单
          </button>
        </div>
        <div className="model-search">
          <Search size={15} />
          <input
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
              !!(catalog?.retryAt && catalog.retryAt > Date.now() / 1000)
            }
            onClick={() => void load(true)}
          >
            <RefreshCw size={15} className={loading ? "spin" : ""} />
          </button>
        </div>
        {(catalogError || catalog?.error) && (
          <div className="form-error" role="status">
            {catalogError || catalog?.error}
          </div>
        )}
        {catalog?.stale && (
          <p className="muted">
            配置已变化，旧模型列表仅供参考；手动刷新后才用于本地模型并集。
          </p>
        )}
        <div className="model-options" aria-label="可选模型">
          {!models.length && (
            <div className="model-empty">
              {loading ? "正在读取模型…" : "没有匹配模型"}
            </div>
          )}
          {models.map((model) => (
            <label
              key={model}
              title={(() => {
                const meta = catalog?.metadata?.[model];
                return meta
                  ? [
                      meta.name,
                      meta.context
                        ? `上下文 ${meta.context.toLocaleString()}`
                        : null,
                      meta.output
                        ? `输出上限 ${meta.output.toLocaleString()}`
                        : null,
                      meta.reasoningEfforts
                        ? `推理档位 ${Object.keys(meta.reasoningEfforts).join(" / ")}`
                        : null,
                      `models.dev · ${meta.sources.join(" / ")}`,
                    ]
                      .filter(Boolean)
                      .join(" · ")
                  : model;
              })()}
            >
              <input
                type="checkbox"
                disabled={saving}
                checked={selected.includes(model)}
                onChange={(e) => {
                  setLimited(true);
                  setSelected(
                    e.target.checked
                      ? [...selected, model]
                      : selected.filter((v) => v !== model),
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
            value={custom}
            disabled={saving}
            onChange={(e) => setCustom(e.target.value)}
          />
        </label>
        <button
          type="button"
          className="text-button"
          disabled={saving || !custom.trim()}
          onClick={add}
        >
          添加到白名单
        </button>
        {error && (
          <div className="form-error" role="alert">
            {error}
          </div>
        )}
        <div className="dialog-actions">
          <button
            type="button"
            className="secondary"
            disabled={saving}
            onClick={() => void leave()}
          >
            取消
          </button>
          <button className="primary" disabled={disabled || saving}>
            {saving ? "保存中…" : baseline.current === null ? "重试" : "保存"}
          </button>
        </div>
      </form>
    </Modal>
  );
}

export default function ProviderSettings(
  props: Parameters<typeof StandardProviderSettings>[0],
) {
  return props.clientId === "dsh" ? (
    <DshModelMapping {...props} />
  ) : (
    <StandardProviderSettings {...props} />
  );
}
