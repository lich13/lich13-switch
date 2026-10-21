import { useEffect, useState } from "react";
import { command } from "./bridge";
import type { DshPreview } from "./bridge-contracts";
import { errorOf, type GatewayState, type GrokProtocol } from "./types";
import Modal from "./Modal";

export default function DshProfile({
  state,
  close,
  update,
  notify,
}: {
  state: GatewayState;
  close: () => void;
  update: (state: GatewayState) => void;
  notify: (text: string) => void;
}) {
  const [profile, setProfile] = useState(state.dshProfile?.profile ?? "");
  const [protocol, setProtocol] = useState<GrokProtocol>(
    state.dshProfile?.protocol ?? "chat_completions",
  );
  const [model, setModel] = useState(state.dshProfile?.model ?? "");
  const [makeDefault, setMakeDefault] = useState(
    state.dshProfile?.makeDefault ?? false,
  );
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
      const next = await command("attach_dsh_profile", {
        options: { profile, protocol, model: model || null, makeDefault },
        expectedRevision: state.revision,
        previewRevision: preview.revision,
      });
      update(next);
      notify(
        state.running
          ? "DSH 模型列表已同步"
          : "DSH profile 已接入；默认模型仅影响新会话",
      );
      close();
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
            disabled={busy || state.running}
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
