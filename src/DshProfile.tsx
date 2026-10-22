import { useEffect, useState } from "react";
import { command } from "./bridge";
import type { DshPreview } from "./bridge-contracts";
import { errorOf, type GatewayState } from "./types";
import Modal from "./Modal";
import { preferredDshProfile, rememberDshProfile } from "./dsh-profile";

export async function connectDsh(state: GatewayState, preview: DshPreview) {
  if (!preview.profile) throw new Error("没有可用的 DSH profile");
  const result = await command("attach_dsh_profiles", {
    all: true,
    options: {
      profile: preview.profile,
      protocol: "chat_completions",
      model:
        preview.model === "deepseek-v4-pro" ? preview.model : "deepseek-flash",
      makeDefault: true,
    },
    expectedRevision: state.revision,
    previewRevision: preview.revision,
  });
  const failed = result.results.filter((item) => item.status !== "success");
  if (failed.length)
    throw new Error(
      failed
        .map((item) => `${item.profile}：${item.error || "未执行"}`)
        .join("；"),
    );
  if (!state.running && result.state.mode !== "auto") {
    result.state = await command("update_gateway", {
      clientId: "dsh",
      expectedRevision: result.state.revision,
      edit: { op: "mode", mode: "auto" },
    });
  }
  return result.state;
}

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
  const [profile, setProfile] = useState(preferredDshProfile);
  const [preview, setPreview] = useState<DshPreview | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  useEffect(() => {
    let gone = false;
    setPreview(null);
    setError("");
    void command("preview_dsh_profile", {
      profile,
      protocol: "chat_completions",
    })
      .then((next) => {
        if (!gone) {
          setPreview(next);
          setProfile(next.profile || "");
        }
      })
      .catch((e) => {
        if (!gone) setError(errorOf(e).message);
      });
    return () => {
      gone = true;
    };
  }, [profile]);
  return (
    <Modal
      title={state.running ? "同步 DSH" : "导入并接入 DSH"}
      close={close}
      busy={busy}
      compact
    >
      <form
        className="gateway-form"
        onSubmit={(e) => {
          e.preventDefault();
          if (!preview) return;
          setBusy(true);
          setError("");
          void connectDsh(state, preview)
            .then((next) => {
              update(next);
              notify(
                state.running ? "DSH 已同步" : "DSH 已接入，自动切换已启用",
              );
              close();
            })
            .catch((e) => setError(errorOf(e).message))
            .finally(() => setBusy(false));
        }}
      >
        <label>
          配置来源
          <select
            aria-label="DSH 配置来源"
            value={profile}
            disabled={busy}
            onChange={(e) => {
              rememberDshProfile(e.target.value);
              setProfile(e.target.value);
            }}
          >
            {preview?.profiles.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
        </label>
        <label>
          新会话默认模型
          <select
            value={
              preview?.model === "deepseek-v4-pro"
                ? "deepseek-v4-pro"
                : "deepseek-flash"
            }
            disabled={!preview || busy}
            onChange={(e) =>
              setPreview((p) => p && { ...p, model: e.target.value })
            }
          >
            <option value="deepseek-flash">DeepSeek-V4.1-Flash</option>
            <option value="deepseek-v4-pro">DeepSeek-V4-Pro</option>
          </select>
        </label>
        {!!preview?.providers?.length && (
          <ul className="dsh-results">
            {preview.providers
              .filter(
                (p, i, all) =>
                  all.findIndex(
                    (v) => v.name === p.name && v.error === p.error,
                  ) === i,
              )
              .map((p) => (
                <li key={`${p.profile}:${p.name}`}>
                  <strong>{p.name}</strong>
                  {p.error && <span className="form-error">{p.error}</span>}
                </li>
              ))}
          </ul>
        )}
        {error && (
          <p className="banner error" role="alert">
            {error}
          </p>
        )}
        <footer className="modal-actions">
          <button type="button" disabled={busy} onClick={close}>
            取消
          </button>
          <button className="primary" disabled={busy || !preview?.profile}>
            {state.running ? "同步" : "导入并接入"}
          </button>
        </footer>
      </form>
    </Modal>
  );
}
