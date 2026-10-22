import { useEffect, useRef, useState } from "react";
import { command } from "./bridge";
import type { GrokImportPreview } from "./bridge-contracts";
import { errorOf, type GatewayState } from "./types";
import Modal from "./Modal";

const sources: Record<string, string> = {
  model: "模型配置",
  modelApi: "模型 API 地址",
  environment: "环境变量",
  global: "全局端点",
  globalApi: "全局 API 端点",
};
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
  const [completed, setCompleted] = useState(false);
  const generation = useRef(0),
    token = useRef<string | null>(null);
  async function load(selected?: string, useEnvironment = true) {
    const current = ++generation.current;
    setBusy(true);
    setError("");
    try {
      const next = await command("preview_grok_import", {
        selected,
        useEnvironment,
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
  const model = preview?.models.find((model) => model.key === preview.selected);
  return (
    <Modal title="导入 Grok 当前配置" close={close} busy={busy} compact>
      {completed ? (
        <>
          <p>
            已建立独立 API 模型。Grok 中选择 <code>lich13-switch</code> 使用
            API，选择官方模型使用原生登录。
          </p>
          {!!preview?.clearEnvironment.length && (
            <>
              <p>
                当前终端仍保留旧变量。请新开终端，或执行以下命令后再启动 Grok：
              </p>
              <pre>unset {preview.clearEnvironment.join(" ")}</pre>
            </>
          )}
          <p>
            官方登录使用 <code>grok login</code>。
          </p>
        </>
      ) : (
        <>
          <label>
            模型入口
            <select
              disabled={busy}
              value={preview?.selected ?? ""}
              onChange={(event) =>
                void load(event.target.value, preview?.environmentLoaded)
              }
            >
              {!preview?.selected && <option value="">请选择模型</option>}
              {preview?.models.map((model) => (
                <option key={model.key} value={model.key}>
                  {model.key}
                </option>
              ))}
            </select>
          </label>
          <dl className="import-summary">
            <dt>读取层</dt>
            <dd>{preview?.layers?.join(" → ") || "用户配置"}</dd>
            <dt>模型来源</dt>
            <dd>{preview?.modelSource || "未知"}</dd>
          </dl>
          {model && (
            <dl className="import-summary">
              <dt>模型 ID</dt>
              <dd>{model.model || "未识别"}</dd>
              <dt>协议</dt>
              <dd>{model.protocol ?? "未识别"}</dd>
              <dt>地址</dt>
              <dd>{model.address ?? "未读取到"}</dd>
              <dt>地址来源</dt>
              <dd>{sources[model.addressSource ?? ""] ?? "未记录"}</dd>
              <dt>API Key</dt>
              <dd>
                {model.credentialAvailable
                  ? "••••••••"
                  : `未读取到${model.missingEnvironment.length ? `：${model.missingEnvironment.join(" / ")}` : ""}`}
              </dd>
              <dt>凭据来源</dt>
              <dd>
                {model.credentialSource ??
                  (preview?.environmentLoaded
                    ? "endpoint.env"
                    : "当前进程环境")}
              </dd>
            </dl>
          )}
          {preview?.environmentAvailable && (
            <label className="checkbox">
              <input
                type="checkbox"
                disabled={busy}
                checked={preview.environmentLoaded}
                onChange={(event) =>
                  void load(preview.selected, event.target.checked)
                }
              />
              读取 endpoint.env 中的简单赋值
            </label>
          )}
          {preview?.unknown?.map((message) => (
            <p className="muted" key={message}>
              {message}
            </p>
          ))}
          <p>
            确认后将当前 API 连接迁入独立模型，默认继续使用 API，并关闭 campaign
            覆盖；撤销受管默认选择时恢复原值。
          </p>
        </>
      )}
      {(error || (!completed && (preview?.error || model?.error))) && (
        <div className="form-error" role="alert">
          {error || preview?.error || model?.error}
        </div>
      )}
      <footer className="modal-actions">
        <button disabled={busy} onClick={close}>
          {completed ? "完成" : "取消"}
        </button>
        {!completed && (
          <button
            className="primary"
            disabled={busy || !preview?.token || !!preview.error || !!error}
            onClick={() => {
              if (!preview?.token) return;
              const current = generation.current;
              setBusy(true);
              setError("");
              void command("commit_grok_import", {
                token: preview.token,
                expectedRevision: preview.revision,
              })
                .then((state) => {
                  if (current !== generation.current) return;
                  token.current = null;
                  update(state);
                  setCompleted(true);
                  notify("Grok API 配置已导入");
                })
                .catch((e) => {
                  if (current === generation.current)
                    setError(errorOf(e).message);
                })
                .finally(() => {
                  if (current === generation.current) setBusy(false);
                });
            }}
          >
            确认导入
          </button>
        )}
      </footer>
    </Modal>
  );
}
