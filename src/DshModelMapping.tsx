import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import type { GatewayEdit } from "./bridge-contracts";
import type { EditRevision } from "./gateway-edit";
import { errorOf, type Provider } from "./types";
import Modal from "./Modal";
import { confirmAction } from "./confirmation";

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
  const [flash, setFlash] = useState(provider.dshModels?.flash ?? "");
  const [pro, setPro] = useState(provider.dshModels?.pro ?? "");
  const [models, setModels] = useState<string[]>([]);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const baseline = useRef<EditRevision>(revision);
  const initial = useRef(JSON.stringify([flash, pro]));
  const dirty = JSON.stringify([flash, pro]) !== initial.current;
  const generation = useRef(0);
  const inferred = useRef(false);
  async function load(force: boolean) {
    const seq = ++generation.current;
    setLoading(true);
    try {
      const catalog = await command("list_provider_models", {
        clientId: "dsh",
        providerId: provider.id,
        force,
      });
      if (seq === generation.current) {
        setModels(catalog.models);
        setError("");
        if (!provider.dshModels && !inferred.current) {
          inferred.current = true;
          const unique = (kind: string) => {
            const choices = [
              ...new Set(
                catalog.models.filter(
                  (id) =>
                    id.toLowerCase().includes("deepseek") &&
                    id
                      .toLowerCase()
                      .split(/[^a-z0-9]+/)
                      .includes(kind),
                ),
              ),
            ];
            return choices.length === 1 ? choices[0] : "";
          };
          setFlash((value) => value || unique("flash"));
          setPro((value) => value || unique("pro"));
        }
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
  return (
    <Modal
      title={`${provider.name} 模型映射`}
      close={() => void leave()}
      busy={busy}
      compact
    >
      <form
        className="gateway-form"
        onSubmit={(e) => {
          e.preventDefault();
          setBusy(true);
          setError("");
          void save(
            {
              op: "dshModelsProvider",
              id: provider.id,
              mapping: { flash: flash.trim() || null, pro: pro.trim() || null },
            },
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
        <label>
          Flash
          <input
            aria-label="Flash 上游模型"
            list="dsh-provider-models"
            value={flash}
            maxLength={256}
            placeholder="未配置"
            onChange={(e) => setFlash(e.target.value)}
          />
        </label>
        <label>
          Pro
          <input
            aria-label="Pro 上游模型"
            list="dsh-provider-models"
            value={pro}
            maxLength={256}
            placeholder="未配置"
            onChange={(e) => setPro(e.target.value)}
          />
        </label>
        <datalist id="dsh-provider-models">
          {[...new Set([flash, pro, ...models])].filter(Boolean).map((id) => (
            <option key={id} value={id} />
          ))}
        </datalist>
        <p className="form-hint">留空时，此供应商不承接对应模型。</p>
        <button
          type="button"
          disabled={loading || busy}
          onClick={() => void load(true)}
        >
          刷新模型列表
        </button>
        {error && (
          <p className="form-error" role="alert">
            {error}
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
