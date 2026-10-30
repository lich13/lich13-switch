import { subscriptions } from "./subscriptions";
import ClaudeProfile from "./ClaudeProfile";
import DshConfigEditor from "./DshConfigEditor";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Save,
  RotateCcw,
  Eye,
  EyeOff,
  Braces,
  History,
  ListChecks,
} from "lucide-react";
import { command } from "./bridge";
import {
  errorOf,
  type ConfigDocument,
  type AppError,
  type ConfigClientId,
} from "./types";
import ClientSelection, {
  clientName,
  useClientSelection,
} from "./ClientSelection";
import ClaudeSettings from "./ClaudeSettings";
import AntigravitySettings from "./AntigravitySettings";
import { changes, get } from "./claude-settings";
import Modal from "./Modal";
import { confirmAction } from "./confirmation";
import {
  getCachedCodeMirrorSurface,
  loadCodeMirrorSurface,
} from "./codeMirrorLoader";

type Props = {
  revision: string;
  home: string;
  claudeHome: string;
  grokHome?: string;
  theme: "light" | "dark";
  onDirty: (value: boolean) => void;
  onMessage: (s: string) => void;
};
export default function ConfigEditor(props: Props) {
  const [clientId, select] = useClientSelection("config");
  const dirty = useRef(false);
  const changed = useCallback(
    (value: boolean) => {
      dirty.current = value;
      props.onDirty(value);
    },
    [props.onDirty],
  );
  const home =
    clientId === "claude"
      ? props.claudeHome
      : clientId === "grok"
        ? (props.grokHome ?? "~/.grok")
        : props.home;
  if (clientId === "dsh")
    return (
      <DshConfigEditor
        theme={props.theme}
        select={(id) => select(id, dirty.current)}
        onDirty={changed}
        onMessage={props.onMessage}
      />
    );
  return (
    <EditorContent
      key={`${clientId}:${home}`}
      {...props}
      home={home}
      clientId={clientId}
      select={(id) => select(id, dirty.current)}
      onDirty={changed}
    />
  );
}
function EditorContent({
  revision,
  home,
  clientId,
  select,
  theme,
  onDirty,
  onMessage,
}: Props & { clientId: ConfigClientId; select: (id: ConfigClientId) => void }) {
  const jsonDualMode = clientId === "claude" || clientId === "antigravity";
  const [doc, setDoc] = useState<ConfigDocument | null>(null),
    [text, setText] = useState(""),
    [error, setError] = useState<AppError | null>(null),
    [busy, setBusy] = useState(false),
    [conflict, setConflict] = useState(false),
    [visual, setVisual] = useState(jsonDualMode),
    [revealed, setRevealed] = useState(false),
    [diff, setDiff] = useState(false),
    [Surface, setSurface] = useState(() => getCachedCodeMirrorSurface());
  const docRef = useRef(doc),
    textRef = useRef(text),
    saveRef = useRef(() => {}),
    loadSeq = useRef(0),
    saving = useRef(false),
    visualRef = useRef(visual);
  docRef.current = doc;
  textRef.current = text;
  visualRef.current = visual;
  const dirty = !!doc && text !== doc.text;
  useEffect(() => {
    onDirty(dirty);
    return () => onDirty(false);
  }, [dirty, onDirty]);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    try {
      const readPromise =
        clientId === "antigravity"
          ? command("read_antigravity_config")
          : command("read_config", { clientId });
      const d = !jsonDualMode
        ? (
            await Promise.all([
              readPromise,
              loadCodeMirrorSurface().then((Loaded) => {
                if (seq === loadSeq.current) setSurface(() => Loaded);
              }),
            ])
          )[0]
        : await readPromise;
      if (seq !== loadSeq.current) return;
      let fallbackToSource = false;
      if (jsonDualMode) {
        try {
          get(d.text, []);
        } catch {
          fallbackToSource = true;
        }
        if (fallbackToSource || !visualRef.current) {
          const Loaded = await loadCodeMirrorSurface();
          if (seq !== loadSeq.current) return;
          setSurface(() => Loaded);
        }
      }
      setDoc(d);
      setText(d.text);
      setError(null);
      setConflict(false);
      if (fallbackToSource) setVisual(false);
    } catch (e) {
      if (seq === loadSeq.current) setError(errorOf(e));
    }
  }, [clientId, jsonDualMode]);
  useEffect(() => {
    void load();
    return () => {
      loadSeq.current++;
    };
  }, [load, home]);
  useEffect(() => {
    const events = subscriptions((e) => setError(errorOf(e)));
    events.watch("config-state", (s) => {
      if (s.clientId !== clientId) return;
      setDoc((d) => (d ? { ...d, guarded: s.guarded } : d));
      if (docRef.current && s.revision !== docRef.current.revision) {
        if (textRef.current !== docRef.current.text) setConflict(true);
        else void load();
      }
    });
    return () => {
      events.dispose();
    };
  }, [clientId, load]);
  useEffect(() => {
    if (
      clientId === "codex" &&
      docRef.current &&
      revision !== docRef.current.revision
    ) {
      if (textRef.current !== docRef.current.text) setConflict(true);
      else void load();
    }
  }, [revision, clientId, load]);
  const save = useCallback(async () => {
    const current = docRef.current;
    if (!current || saving.current || textRef.current === current.text) return;
    saving.current = true;
    const submitted = textRef.current;
    setBusy(true);
    setError(null);
    try {
      const d = await (clientId === "antigravity"
        ? command("save_antigravity_config", {
            text: submitted,
            expectedRevision: current.revision,
          })
        : command("save_config", {
            clientId,
            text: submitted,
            expectedRevision: current.revision,
          }));
      const restart =
        clientId === "codex" ||
        (clientId === "claude" &&
          ["model", "env", "effortLevel", "modelSettings"].some((key) => {
            try {
              return (
                JSON.stringify(get(current.text, [key])) !==
                JSON.stringify(get(d.text, [key]))
              );
            } catch {
              return true;
            }
          }));
      setDoc(d);
      setText((draft) => (draft === submitted ? d.text : draft));
      setConflict(false);
      onMessage(
        restart
          ? `配置已保存，请重新打开 ${clientName(clientId)}`
          : "配置已保存",
      );
    } catch (e) {
      setError(errorOf(e));
    } finally {
      saving.current = false;
      setBusy(false);
    }
  }, [clientId, onMessage]);
  saveRef.current = () => {
    void save();
  };
  useEffect(() => {
    const handler = (e: KeyboardEvent) => {
      if (
        !e.defaultPrevented &&
        (e.metaKey || e.ctrlKey) &&
        e.key.toLowerCase() === "s"
      ) {
        e.preventDefault();
        if (dirty && !busy) saveRef.current();
      }
    };
    window.addEventListener("keydown", handler);
    return () => window.removeEventListener("keydown", handler);
  }, [dirty, busy]);
  useEffect(() => {
    if ((!jsonDualMode || !visual) && !Surface) {
      let active = true;
      void loadCodeMirrorSurface().then((Loaded) => {
        if (active) setSurface(() => Loaded);
      });
      return () => {
        active = false;
      };
    }
  }, [jsonDualMode, visual, Surface]);
  const validateText = useCallback(
    async (candidate: string) => {
      if (clientId === "antigravity")
        await command("validate_antigravity_config", { text: candidate });
      else await command("validate_config", { clientId, text: candidate });
    },
    [clientId],
  );
  const switchMode = async (next: boolean) => {
    if (next) {
      try {
        get(text, []);
        await validateText(text);
        setError(null);
      } catch (e) {
        setError(errorOf(e));
        return;
      }
    } else if (!Surface) {
      const Loaded = await loadCodeMirrorSurface();
      setSurface(() => Loaded);
    }
    setVisual(next);
  };
  const update = (value: string) => {
    setText(
      doc?.text.includes("\r\n") ? value.replace(/\r?\n/g, "\r\n") : value,
    );
    setError(null);
  };
  let delta: ReturnType<typeof changes> = [];
  if (diff && doc)
    try {
      delta = changes(doc.text, text);
    } catch {
      /* Invalid drafts remain in the source editor. */
    }
  return (
    <section className="config-page">
      {clientId === "claude" && (
        <ClaudeProfile
          disabled={busy}
          beforeChange={async () =>
            !dirty || (await confirmAction("切换配置会丢弃未保存的修改。"))
          }
          onChanged={() => void load()}
          notify={onMessage}
        />
      )}
      <div className="config-heading">
        <ClientSelection
          visibleClients={["codex", "claude", "grok", "dsh", "antigravity"]}
          client={clientId}
          select={select}
          disabled={busy}
        />
        <div className="inline">
          {jsonDualMode && (
            <div className="segmented" aria-label="编辑方式">
              <button
                aria-pressed={visual}
                onClick={() => void switchMode(true)}
                disabled={busy}
              >
                可视化
              </button>
              <button
                aria-pressed={!visual}
                onClick={() => void switchMode(false)}
                disabled={busy}
              >
                JSON
              </button>
            </div>
          )}
          <button
            className="primary"
            disabled={!dirty || busy}
            onClick={() => void save()}
          >
            <Save size={15} />
            {busy ? "保存中…" : "保存"}
          </button>
        </div>
      </div>
      <div className="editor-toolbar">
        <span title={doc?.path}>
          {clientId === "antigravity"
            ? "config.json"
            : clientId === "claude"
              ? "settings.json"
              : "config.toml"}
        </span>
        <div className="inline">
          {dirty && <span className="status amber">未保存</span>}
          {jsonDualMode && dirty && (
            <button
              className="icon-button"
              aria-label="查看改动"
              title="查看改动"
              onClick={() => {
                try {
                  changes(doc!.text, text);
                  setDiff(true);
                } catch (e) {
                  setError(errorOf(e));
                }
              }}
            >
              <ListChecks size={16} />
            </button>
          )}
          {jsonDualMode && !visual && (
            <>
              <button
                className="icon-button"
                aria-label="格式化 JSON"
                title="格式化 JSON"
                onClick={() => {
                  try {
                    update(JSON.stringify(get(text, []), null, 2) + "\n");
                  } catch (e) {
                    setError(errorOf(e));
                  }
                }}
              >
                <Braces size={16} />
              </button>
              <button
                className="icon-button"
                aria-label={revealed ? "隐藏敏感值" : "显示敏感值"}
                title={revealed ? "隐藏敏感值" : "显示敏感值"}
                onClick={() => setRevealed(!revealed)}
              >
                {revealed ? <EyeOff size={16} /> : <Eye size={16} />}
              </button>
            </>
          )}
          <button
            className="icon-button"
            aria-label="恢复上次配置"
            title="恢复上次配置"
            disabled={!doc?.canRestore || busy}
            onClick={async () => {
              if (
                dirty &&
                !(await confirmAction("恢复上次配置会替换当前草稿。", "恢复"))
              )
                return;
              void (
                clientId === "antigravity"
                  ? command("read_previous_antigravity_config")
                  : command("read_previous_config", { clientId })
              )
                .then(update)
                .catch((e) => setError(errorOf(e)));
            }}
          >
            <History size={16} />
          </button>
          <button
            className="icon-button"
            aria-label={dirty ? "撤销草稿并重新读取" : "重新读取配置"}
            title={dirty ? "撤销草稿并重新读取" : "重新读取配置"}
            disabled={busy}
            onClick={async () => {
              if (
                !dirty ||
                (await confirmAction("重新读取会丢弃未保存的草稿。"))
              )
                void load();
            }}
          >
            <RotateCcw size={16} />
          </button>
        </div>
      </div>
      {conflict && error?.code !== "CONFLICT" && (
        <div className="banner warning" role="alert">
          磁盘配置已变化，草稿已保留。请重新读取后合并。
        </div>
      )}
      {error && (
        <div className="banner error" role="alert">
          {error.message}
          {error.line ? `（第 ${error.line} 行）` : ""}
        </div>
      )}
      {!doc ? (
        <div className="empty">正在读取配置…</div>
      ) : clientId === "claude" && visual ? (
        <ClaudeSettings text={text} onChange={update} guarded={doc.guarded} />
      ) : clientId === "antigravity" && visual ? (
        <AntigravitySettings text={text} onChange={update} />
      ) : (
        <div className="editor-body">
          {Surface ? (
            <Surface
              mode={jsonDualMode ? "json" : "toml"}
              value={text}
              onChange={update}
              theme={theme}
              maskSecrets={jsonDualMode && !revealed}
              onSave={() => saveRef.current()}
              validateText={validateText}
              ariaLabel={jsonDualMode ? "JSON 编辑器" : "TOML 编辑器"}
            />
          ) : (
            <div className="empty">正在加载编辑器…</div>
          )}
        </div>
      )}
      {diff && (
        <Modal title="配置改动" close={() => setDiff(false)}>
          <div className="config-diff">
            {delta.map((d) => (
              <div key={d.path}>
                <strong>{d.path}</strong>
                <del>{d.before}</del>
                <ins>{d.after}</ins>
              </div>
            ))}
          </div>
        </Modal>
      )}
    </section>
  );
}
