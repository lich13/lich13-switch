import { useEffect, useRef, useState } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { StreamLanguage } from "@codemirror/language";
import { yaml } from "@codemirror/legacy-modes/mode/yaml";
import { EditorView } from "@codemirror/view";
import { Save, RotateCcw, History } from "lucide-react";
import { command } from "./bridge";
import type { DshFileKey, DshFileView } from "./bridge-contracts";
import ClientSelection from "./ClientSelection";
import { errorOf, type ClientId } from "./types";
import { confirmAction } from "./confirmation";
import Modal from "./Modal";

const names: Record<DshFileKey, string> = {
  profilePatch: "profile · cordis.patch.yml",
  homePatch: "home · cordis.patch.yml",
  settings: "旧版 settings.yaml",
  environment: ".env · 只读",
  credentials: ".credentials.yaml · 只读",
};
const extensions = [
  StreamLanguage.define(yaml),
  EditorView.theme({
    "&": { height: "100%", background: "var(--canvas)" },
    ".cm-scroller": {
      fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace",
      fontSize: "13px",
    },
    ".cm-content": { padding: "12px 0" },
    ".cm-gutters": {
      background: "var(--canvas)",
      borderRight: "1px solid var(--line)",
    },
  }),
];

export default function DshConfigEditor({
  select,
  theme,
  onDirty,
  onMessage,
}: {
  select: (id: ClientId) => void;
  theme: "light" | "dark";
  onDirty: (dirty: boolean) => void;
  onMessage: (message: string) => void;
}) {
  const [profile, setProfile] = useState("");
  const [key, setKey] = useState<DshFileKey>("profilePatch");
  const [doc, setDoc] = useState<DshFileView | null>(null);
  const normalizedSelection = useRef<{
    profile: string;
    key: DshFileKey;
  } | null>(null);
  const [choices, setChoices] = useState<
    Pick<DshFileView, "profiles" | "files">
  >({
    profiles: [],
    files: ["profilePatch"],
  });
  const [text, setText] = useState("");
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [loading, setLoading] = useState(false);
  const [previous, setPrevious] = useState<string | null>(null);
  const dirty = !!doc && !doc.readOnly && text !== doc.text;
  useEffect(() => {
    onDirty(dirty);
    return () => onDirty(false);
  }, [dirty, onDirty]);
  useEffect(() => {
    const normalized = normalizedSelection.current;
    normalizedSelection.current = null;
    if (normalized?.profile === profile && normalized.key === key) return;
    let gone = false;
    setLoading(true);
    setError("");
    void command("read_dsh_file", { profile: profile || null, key })
      .then((value) => {
        if (!gone) {
          if (profile !== (value.profile ?? "") || key !== value.key) {
            normalizedSelection.current = {
              profile: value.profile ?? "",
              key: value.key,
            };
          }
          setDoc(value);
          setChoices(value);
          setText(value.text);
          setProfile(value.profile ?? "");
          setKey(value.key);
          setLoading(false);
        }
      })
      .catch((e) => {
        if (!gone) {
          setError(errorOf(e).message);
          setDoc(null);
        }
      })
      .finally(() => {
        if (!gone) setLoading(false);
      });
    return () => {
      gone = true;
    };
  }, [profile, key]);
  async function switchFile(nextProfile: string, nextKey: DshFileKey) {
    const selectedKey =
      !nextProfile && nextKey === "profilePatch" ? "homePatch" : nextKey;
    if (nextProfile === profile && selectedKey === key) {
      if (!doc) await reload();
      return;
    }
    if (
      dirty &&
      !(await confirmAction("切换文件或 profile 会丢弃未保存的修改。"))
    )
      return;
    setDoc(null);
    setText("");
    setPrevious(null);
    setProfile(nextProfile);
    setKey(selectedKey);
  }
  async function save(restore = false) {
    if (!doc || busy || loading || doc.readOnly) return;
    if (
      restore &&
      !(await confirmAction("恢复上一版会替换当前文件和未保存草稿，是否继续？"))
    )
      return;
    setBusy(true);
    setError("");
    try {
      const args = {
        profile: profile || null,
        key,
        expectedRevision: doc.revision,
      };
      const next = restore
        ? await command("restore_dsh_file", args)
        : await command("save_dsh_file", { ...args, text });
      setDoc(next);
      setChoices(next);
      setText(next.text);
      setPrevious(null);
      onMessage(restore ? "DSH 配置已恢复" : "DSH 配置已保存");
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  }
  async function reload() {
    if (dirty && !(await confirmAction("重新读取会丢弃未保存草稿。"))) return;
    setBusy(true);
    setError("");
    try {
      const next = await command("read_dsh_file", {
        profile: profile || null,
        key,
      });
      setDoc(next);
      setChoices(next);
      setText(next.text);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  }
  return (
    <section
      className="config-page dsh-config-page"
      onKeyDown={(e) => {
        if ((e.metaKey || e.ctrlKey) && e.key === "s") {
          e.preventDefault();
          if (dirty) void save();
        }
      }}
    >
      <div className="config-heading">
        <ClientSelection
          client="dsh"
          select={select}
          disabled={busy || loading}
        />
        <button
          className="primary"
          disabled={!dirty || busy || loading}
          onClick={() => void save()}
        >
          <Save size={15} />
          {busy ? "保存中…" : "保存"}
        </button>
      </div>
      <div className="dsh-file-selectors">
        <label>
          profile
          <select
            aria-label="DSH 配置 profile"
            value={profile}
            disabled={busy || loading}
            onChange={(e) => void switchFile(e.target.value, "profilePatch")}
          >
            <option value="">请选择 profile</option>
            {choices.profiles.map((p) => (
              <option key={p}>{p}</option>
            ))}
          </select>
        </label>
        <label>
          文件
          <select
            aria-label="DSH 配置文件"
            value={key}
            disabled={busy || loading}
            onChange={(e) =>
              void switchFile(profile, e.target.value as DshFileKey)
            }
          >
            {choices.files.map((file) => (
              <option
                key={file}
                value={file}
                disabled={file === "profilePatch" && !profile}
              >
                {names[file]}
              </option>
            ))}
          </select>
        </label>
      </div>
      <div className="editor-toolbar">
        <span>
          {names[key]}
          {dirty && <span className="status amber"> · 未保存</span>}
        </span>
        <div className="inline">
          <button disabled={busy || loading} onClick={() => void reload()}>
            <RotateCcw size={14} />
            重新读取
          </button>
          <button
            disabled={busy || loading || !doc?.canRestore}
            onClick={() => {
              setBusy(true);
              void command("read_previous_dsh_file", {
                profile: profile || null,
                key,
              })
                .then(setPrevious)
                .catch((e) => setError(errorOf(e).message))
                .finally(() => setBusy(false));
            }}
          >
            <History size={14} />
            上一版
          </button>
          <button
            disabled={busy || loading || !doc?.canRestore}
            onClick={() => void save(true)}
          >
            恢复上一版
          </button>
        </div>
      </div>
      {error && (
        <p className="banner error" role="alert">
          {error}
          {dirty && "；草稿已保留"}
        </p>
      )}
      {doc?.readOnly && (
        <p className="muted">仅显示名称与遮蔽值，请在 DSH 中管理凭据。</p>
      )}
      <div className="dsh-source-editor">
        <CodeMirror
          aria-label="DSH 配置内容"
          value={text}
          theme={theme}
          height="100%"
          extensions={extensions}
          editable={!busy && !loading && !!doc && !doc.readOnly}
          onChange={setText}
        />
      </div>
      {previous != null && (
        <Modal title="DSH 上一版配置" close={() => setPrevious(null)}>
          <pre className="dsh-previous">{previous}</pre>
          <div className="form-actions">
            <button onClick={() => setPrevious(null)}>关闭</button>
            <button disabled={busy} onClick={() => void save(true)}>
              恢复上一版
            </button>
          </div>
        </Modal>
      )}
    </section>
  );
}
