import { useEffect, useState } from "react";
import { command } from "./bridge";
import type { DshBatch, DshPreview } from "./bridge-contracts";
import { errorOf, type GatewayState, type GrokProtocol } from "./types";
import Modal from "./Modal";

export default function DshProfile({
  state,
  close,
  update,
  notify,
  openModels,
}: {
  state: GatewayState;
  close: () => void;
  update: (state: GatewayState) => void;
  notify: (text: string) => void;
  openModels?: () => void;
}) {
  const [profile, setProfile] = useState(state.dshProfile?.profile ?? "");
  const [protocol, setProtocol] = useState<GrokProtocol>(
    state.dshProfile?.protocol ?? "chat_completions",
  );
  const [model, setModel] = useState(state.dshProfile?.model ?? "");
  const [makeDefault, setMakeDefault] = useState(
    state.dshProfile?.makeDefault ?? false,
  );
  const [all, setAll] = useState(false);
  const [results, setResults] = useState<DshBatch["results"]>([]);
  const [preview, setPreview] = useState<DshPreview | null>(null);
  const [profiles, setProfiles] = useState<string[]>([]);
  const [busy, setBusy] = useState(false),
    [loading, setLoading] = useState(false),
    [error, setError] = useState("");
  useEffect(() => {
    let gone = false;
    setPreview(null);
    setError("");
    setLoading(true);
    void command("preview_dsh_profile", { profile: profile || null, protocol })
      .then((next) => {
        if (!gone) {
          setPreview(next);
          setProfiles(next.profiles);
        }
      })
      .catch((e) => {
        if (!gone) setError(errorOf(e).message);
      })
      .finally(() => {
        if (!gone) setLoading(false);
      });
    return () => {
      gone = true;
    };
  }, [profile, protocol]);
  async function apply() {
    if (!preview || !profile) return;
    setBusy(true);
    setError("");
    try {
      const next = await command("attach_dsh_profiles", {
        all,
        options: { profile, protocol, model: model || null, makeDefault },
        expectedRevision: state.revision,
        previewRevision: preview.revision,
      });
      update(next.state);
      setResults(next.results);
      setPreview(await command("preview_dsh_profile", { profile, protocol }));
      if (next.results.some((result) => result.status !== "success")) {
        setError("部分 profile 未完成，已保留成功项。请查看下方结果后重试。");
        return;
      }
      notify(
        state.running
          ? "DSH 模型列表已同步"
          : "DSH profile 已接入；默认模型仅影响新会话",
      );
      if (!all) close();
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <Modal title="DSH profile 接入" close={close} busy={busy}>
      <form
        className="gateway-form"
        onSubmit={(e) => {
          e.preventDefault();
          void apply();
        }}
      >
        <label>
          现有 profile
          <select
            required
            value={profile}
            disabled={busy}
            onChange={(e) => setProfile(e.target.value)}
          >
            <option value="">请选择 profile</option>
            {profiles.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
        </label>
        <label>
          API 协议
          <select
            value={protocol}
            disabled={busy || state.running}
            onChange={(e) => {
              setProtocol(e.target.value as GrokProtocol);
              setModel("");
            }}
          >
            <option value="chat_completions">openai-completions</option>
            <option value="responses">openai-responses</option>
            <option value="messages">anthropic-messages</option>
          </select>
        </label>
        <label>
          模型 ID
          <input
            list="dsh-model-options"
            value={model}
            maxLength={256}
            required={makeDefault || !preview?.models.length}
            onChange={(e) => setModel(e.target.value)}
            placeholder="从列表选择，或填写明确模型 ID"
          />
        </label>
        <datalist id="dsh-model-options">
          {preview?.models.map((id) => (
            <option key={id} value={id} />
          ))}
        </datalist>
        <p className="muted">
          所选协议的在队模型：{preview?.models.length ?? "—"}
          。接入或同步时更新路由模型列表。
        </p>
        {!preview?.models.length && (
          <p className="model-empty">
            所选协议没有可用列表，可直接手填模型 ID，或{" "}
            <button type="button" disabled={busy} onClick={openModels}>
              管理模型列表
            </button>
            。
          </p>
        )}
        <label className="setting-row">
          <span>同步到全部现有 profile</span>
          <input
            type="checkbox"
            checked={all}
            disabled={busy}
            onChange={(e) => setAll(e.target.checked)}
          />
        </label>
        <label className="setting-row">
          <span>设为默认（仅新会话）</span>
          <input
            type="checkbox"
            checked={makeDefault}
            onChange={(e) => setMakeDefault(e.target.checked)}
          />
        </label>
        {preview?.warning && <p className="muted">{preview.warning}</p>}
        {error && (
          <p className="banner error" role="alert">
            {error}
          </p>
        )}
        {!!results.length && (
          <ul className="dsh-results" aria-label="profile 接入结果">
            {results.map((result) => (
              <li key={result.profile}>
                <strong>{result.profile}</strong>
                <span>
                  {
                    { success: "完成", failed: "失败", pending: "未执行" }[
                      result.status
                    ]
                  }
                </span>
                {result.error && (
                  <span className="form-error">{result.error}</span>
                )}
              </li>
            ))}
          </ul>
        )}
        <div className="form-actions">
          <button type="button" disabled={busy} onClick={close}>
            取消
          </button>
          <button
            className="primary"
            disabled={busy || loading || !preview || !profile}
          >
            {state.running ? "同步模型列表" : "接入 profile"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
