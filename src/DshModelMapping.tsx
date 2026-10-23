import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import type { GatewayEdit } from "./bridge-contracts";
import type { EditRevision } from "./gateway-edit";
import { errorOf, type Provider } from "./types";
import Modal from "./Modal";
import { confirmAction } from "./confirmation";
import { useDshCatalog } from "./dsh-catalog";

const MANUAL = "__manual_model__";
export default function DshModelMapping({
  provider,
  revision,
  save,
  close,
  onDirtyChange,
}: {
  provider: Provider;
  revision: string;
  save: (edit: GatewayEdit, revision: EditRevision) => Promise<void>;
  close: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const catalog = useDshCatalog();
  const [mapping, setMapping] = useState<Record<string, string | null>>({
    ...provider.dshModels,
  });
  const [manual, setManual] = useState<Set<string>>(new Set());
  const [models, setModels] = useState<string[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const baseline = useRef<EditRevision>(revision);
  const initial = useRef(JSON.stringify(mapping));
  const dirty = JSON.stringify(mapping) !== initial.current;
  const generation = useRef(0);
  async function load(force: boolean) {
    const seq = ++generation.current;
    setLoading(true);
    try {
      const result = await command("list_provider_models", {
        clientId: "dsh",
        providerId: provider.id,
        force,
      });
      if (seq === generation.current) {
        setModels(result.models);
        setError(result.error ?? "");
      }
    } catch (e) {
      if (seq === generation.current) setError(errorOf(e).message);
    } finally {
      if (seq === generation.current) setLoading(false);
    }
  }
  useEffect(() => {
    void load(false);
    return () => {
      generation.current++;
    };
  }, [provider.id]);
  useEffect(() => {
    onDirtyChange?.(dirty);
    return () => onDirtyChange?.(false);
  }, [dirty, onDirtyChange]);
  const leave = async () => {
    if (!dirty || (await confirmAction("放弃未保存的模型映射？"))) close();
  };
  const rows = [...catalog.models];
  for (const id of Object.keys(mapping))
    if (!rows.some((m) => m.id === id)) rows.push({ id, name: id });
  const update = (id: string, value: string) =>
    setMapping((old) => ({ ...old, [id]: value || null }));
  return (
    <Modal
      title={`${provider.name} 模型映射`}
      close={() => void leave()}
      busy={busy}
      compact
    >
      <form
        className="gateway-form dsh-model-mapping"
        onSubmit={(e) => {
          e.preventDefault();
          setBusy(true);
          setError("");
          const next = Object.fromEntries(
            Object.entries(mapping).map(([id, value]) => [
              id,
              value?.trim() || null,
            ]),
          );
          void save(
            { op: "dshModelsProvider", id: provider.id, mapping: next },
            baseline.current,
          )
            .then(close)
            .catch((e) => {
              baseline.current = null;
              setError(errorOf(e).message);
            })
            .finally(() => setBusy(false));
        }}
      >
        {rows.map(({ id, name }) => {
          const value = mapping[id] ?? "";
          const options = [...new Set([value, ...models])].filter(Boolean);
          return (
            <div className="dsh-mapping-row" key={id}>
              <label title={id}>
                <span>{name || id}</span>
                {name !== id && <small className="muted">{id}</small>}
                <select
                  aria-label={`${name || id} 上游模型`}
                  disabled={busy}
                  value={manual.has(id) ? MANUAL : value}
                  onChange={(e) => {
                    const value = e.target.value;
                    setManual((old) => {
                      const next = new Set(old);
                      if (value === MANUAL) next.add(id);
                      else next.delete(id);
                      return next;
                    });
                    if (value !== MANUAL) update(id, value);
                  }}
                >
                  <option value="">不承接</option>
                  {options.map((model) => (
                    <option key={model} value={model}>
                      {model}
                    </option>
                  ))}
                  <option value={MANUAL}>手动填写…</option>
                </select>
              </label>
              {manual.has(id) && (
                <input
                  aria-label={`${name || id} 自定义模型 ID`}
                  value={value}
                  maxLength={256}
                  disabled={busy}
                  placeholder="完整模型 ID"
                  onChange={(e) => update(id, e.target.value)}
                />
              )}
            </div>
          );
        })}
        {!rows.length && (
          <p className="muted">
            {catalog.loading ? "正在读取模型…" : "没有可用模型"}
          </p>
        )}
        <div className="dsh-model-actions">
          <button
            type="button"
            disabled={catalog.loading || busy}
            onClick={() => void catalog.refresh()}
          >
            刷新官方模型
          </button>
          <button
            type="button"
            disabled={loading || busy}
            onClick={() => void load(true)}
          >
            刷新供应商模型
          </button>
        </div>
        {(catalog.error || error) && (
          <p className="form-error" role="alert">
            {[catalog.error, error].filter(Boolean).join("；")}
          </p>
        )}
        <footer className="modal-actions">
          <button type="button" disabled={busy} onClick={() => void leave()}>
            取消
          </button>
          <button className="primary" disabled={busy || !dirty}>
            保存
          </button>
        </footer>
      </form>
    </Modal>
  );
}
