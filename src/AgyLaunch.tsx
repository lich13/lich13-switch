import { useEffect, useState } from "react";
import { FolderOpen, RefreshCw } from "lucide-react";
import Modal from "./Modal";
import { command } from "./bridge";
import { errorOf, type GatewayState } from "./types";
export default function AgyLaunch({
  state,
  close,
}: {
  state: GatewayState;
  close: () => void;
}) {
  const [model, setModel] = useState(""),
    [directory, setDirectory] = useState("");
  const [models, setModels] = useState<string[]>([]),
    [error, setError] = useState("");
  const [busy, setBusy] = useState(false),
    [loading, setLoading] = useState(false);
  const [installed, setInstalled] = useState<boolean | null>(null);
  const provider =
    state.providers.find((p) => p.id === state.selected) ??
    state.providers.find((p) => p.queued);
  useEffect(() => {
    let gone = false;
    void command("get_agy_launch")
      .then((info) => {
        if (!gone) {
          setInstalled(info.installed);
          setModel(info.model ?? "");
        }
      })
      .catch((e) => {
        if (!gone) setError(errorOf(e).message);
      });
    return () => {
      gone = true;
    };
  }, []);
  const refresh = async () => {
    if (!provider) return;
    setLoading(true);
    setError("");
    try {
      const result = await command("list_provider_models", {
        clientId: "agy",
        providerId: provider.id,
        force: true,
      });
      if (result.error) setError(result.error);
      setModels(result.models);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setLoading(false);
    }
  };
  return (
    <Modal title="启动 Antigravity" close={close} busy={busy} compact>
      <form
        className="agy-launch-form"
        onSubmit={(e) => {
          e.preventDefault();
          if (
            busy ||
            installed !== true ||
            !state.running ||
            !model.trim() ||
            !directory
          )
            return;
          setBusy(true);
          setError("");
          void command("launch_agy", {
            model: model.trim(),
            directory,
            expectedRevision: state.revision,
          })
            .then(close)
            .catch((e) => setError(errorOf(e).message))
            .finally(() => setBusy(false));
        }}
      >
        <label htmlFor="agy-model">模型</label>
        <div className="agy-input-action">
          <input
            id="agy-model"
            disabled={busy}
            required
            value={model}
            onChange={(e) => setModel(e.target.value)}
            list="agy-models"
            autoComplete="off"
            placeholder="模型 ID"
          />
          <button
            type="button"
            className="icon-button"
            title="读取模型"
            aria-label="读取模型"
            disabled={loading || !provider || busy}
            onClick={() => void refresh()}
          >
            <RefreshCw
              size={16}
              className={loading ? "quota-spin" : undefined}
            />
          </button>
        </div>
        <datalist id="agy-models">
          {models.map((id) => (
            <option key={id} value={id} />
          ))}
        </datalist>
        <label htmlFor="agy-directory">工作目录</label>
        <div className="agy-input-action">
          <input
            id="agy-directory"
            disabled={busy}
            required
            value={directory}
            onChange={(e) => setDirectory(e.target.value)}
            placeholder="选择工作目录"
          />
          <button
            type="button"
            className="icon-button"
            aria-label="选择工作目录"
            disabled={busy}
            onClick={() => {
              void command("choose_agy_directory")
                .then((path) => {
                  if (path) setDirectory(path);
                })
                .catch((e) => setError(errorOf(e).message));
            }}
          >
            <FolderOpen size={16} />
          </button>
        </div>
        {(error || installed === false || !state.running) && (
          <div className="form-error" role="alert">
            {error ||
              (installed === false
                ? "未找到 agy，请安装 Antigravity CLI 后重试"
                : "请先启用 Antigravity 网关")}
          </div>
        )}
        <div className="modal-actions">
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={close}
          >
            取消
          </button>
          <button
            className="primary"
            disabled={
              busy ||
              installed !== true ||
              !state.running ||
              !model.trim() ||
              !directory
            }
          >
            启动
          </button>
        </div>
      </form>
    </Modal>
  );
}
