import ClaudeProfile from "./ClaudeProfile";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { StreamLanguage } from "@codemirror/language";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { json } from "@codemirror/lang-json";
import { linter, lintGutter, type Diagnostic } from "@codemirror/lint";
import {
  Decoration,
  EditorView,
  keymap,
  WidgetType,
  type DecorationSet,
} from "@codemirror/view";
import { parseTree, type Node } from "jsonc-parser";
import { StateField, type EditorState } from "@codemirror/state";
import {
  Save,
  RotateCcw,
  Eye,
  EyeOff,
  Braces,
  History,
  ListChecks,
} from "lucide-react";
import { command, subscribe } from "./bridge";
import {
  errorOf,
  type ConfigDocument,
  type AppError,
  type ClientId,
} from "./types";
import ClientSelection, { useClientSelection } from "./ClientSelection";
import ClaudeSettings from "./ClaudeSettings";
import { changes, get, sensitive } from "./claude-settings";
import Modal from "./Modal";
import { confirmAction } from "./confirmation";

class Secret extends WidgetType {
  toDOM() {
    const span = document.createElement("span");
    span.className = "masked-secret";
    span.textContent = '"••••••••"';
    span.setAttribute("aria-label", "已遮蔽凭据");
    return span;
  }
}
function secretDecorations(state: EditorState): DecorationSet {
  const ranges: { from: number; to: number }[] = [];
  const visit = (node: Node) => {
    if (
      node.type === "property" &&
      sensitive([String(node.children?.[0].value)])
    ) {
      const value = node.children?.[1];
      if (value)
        ranges.push({ from: value.offset, to: value.offset + value.length });
    } else node.children?.forEach(visit);
  };
  const tree = parseTree(state.doc.toString());
  if (tree) visit(tree);
  return Decoration.set(
    ranges.map(({ from, to }) =>
      Decoration.replace({ widget: new Secret() }).range(from, to),
    ),
    true,
  );
}
const maskedJson = StateField.define<DecorationSet>({
  create: secretDecorations,
  update: (value, tr) => (tr.docChanged ? secretDecorations(tr.state) : value),
  provide: (field) => [
    EditorView.decorations.from(field),
    EditorView.atomicRanges.of((view) => view.state.field(field)),
  ],
});

type Props = {
  revision: string;
  home: string;
  claudeHome: string;
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
  const home = clientId === "claude" ? props.claudeHome : props.home;
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
}: Props & { clientId: ClientId; select: (id: ClientId) => void }) {
  const [doc, setDoc] = useState<ConfigDocument | null>(null),
    [text, setText] = useState(""),
    [error, setError] = useState<AppError | null>(null),
    [busy, setBusy] = useState(false),
    [conflict, setConflict] = useState(false),
    [visual, setVisual] = useState(clientId === "claude"),
    [revealed, setRevealed] = useState(false),
    [diff, setDiff] = useState(false);
  const docRef = useRef(doc),
    textRef = useRef(text),
    saveRef = useRef(() => {}),
    loadSeq = useRef(0),
    saving = useRef(false);
  docRef.current = doc;
  textRef.current = text;
  const dirty = !!doc && text !== doc.text;
  useEffect(() => {
    onDirty(dirty);
    return () => onDirty(false);
  }, [dirty, onDirty]);
  const load = useCallback(async () => {
    const seq = ++loadSeq.current;
    try {
      const d = await command<ConfigDocument>("read_config", { clientId });
      if (seq !== loadSeq.current) return;
      setDoc(d);
      setText(d.text);
      setError(null);
      setConflict(false);
      if (clientId === "claude") {
        try {
          get(d.text, []);
        } catch {
          setVisual(false);
        }
      }
    } catch (e) {
      if (seq === loadSeq.current) setError(errorOf(e));
    }
  }, [clientId]);
  useEffect(() => {
    void load();
    return () => {
      loadSeq.current++;
    };
  }, [load, home]);
  useEffect(() => {
    let disposed = false,
      unlisten: (() => void) | undefined;
    void subscribe<{ clientId: ClientId; revision: string; guarded: boolean }>(
      "config-state",
      (s) => {
        if (disposed || s.clientId !== clientId) return;
        setDoc((d) => (d ? { ...d, guarded: s.guarded } : d));
        if (docRef.current && s.revision !== docRef.current.revision) {
          if (textRef.current !== docRef.current.text) setConflict(true);
          else void load();
        }
      },
    ).then((fn) => {
      if (disposed) fn();
      else unlisten = fn;
    });
    return () => {
      disposed = true;
      unlisten?.();
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
      const d = await command<ConfigDocument>("save_config", {
        clientId,
        text: submitted,
        expectedRevision: current.revision,
      });
      const restart =
        clientId === "codex" ||
        ["model", "env", "effortLevel", "modelSettings"].some((key) => {
          try {
            return (
              JSON.stringify(get(current.text, [key])) !==
              JSON.stringify(get(d.text, [key]))
            );
          } catch {
            return true;
          }
        });
      setDoc(d);
      setText((draft) => (draft === submitted ? d.text : draft));
      setConflict(false);
      onMessage(
        restart
          ? `配置已保存，请重新打开 ${clientId === "claude" ? "Claude Code" : "Codex"}`
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
  const extensions = useMemo(
    () => [
      clientId === "claude" ? json() : StreamLanguage.define(toml),
      lintGutter(),
      keymap.of([
        {
          key: "Mod-s",
          run: () => {
            saveRef.current();
            return true;
          },
        },
      ]),
      EditorView.lineWrapping,
      ...(clientId === "claude" && !revealed ? [maskedJson] : []),
      linter(
        async (view) => {
          try {
            await command("validate_config", {
              clientId,
              text: view.state.doc.toString(),
            });
            return [];
          } catch (e) {
            const err = errorOf(e);
            if (!err.line) return [];
            const line = view.state.doc.line(
              Math.min(Math.max(1, err.line), view.state.doc.lines),
            );
            const from = Math.min(
              line.to,
              line.from + Math.max(0, (err.column ?? 1) - 1),
            );
            return [
              {
                from,
                to: Math.min(view.state.doc.length, from + 1),
                severity: "error",
                message: err.message,
              },
            ] as Diagnostic[];
          }
        },
        { delay: 450 },
      ),
      EditorView.theme({
        "&": { height: "100%", background: "var(--canvas)" },
        ".cm-scroller": {
          fontFamily: "ui-monospace, SFMono-Regular, Consolas, monospace",
          fontSize: "13px",
        },
        ".cm-gutters": {
          background: "var(--canvas)",
          borderRight: "1px solid var(--line)",
        },
        ".cm-content": { padding: "16px 0" },
        ".cm-line": { padding: "0 16px" },
      }),
    ],
    [clientId, revealed],
  );
  const switchMode = async (next: boolean) => {
    if (next) {
      try {
        get(text, []);
        await command("validate_config", { clientId, text });
        setError(null);
      } catch (e) {
        setError(errorOf(e));
        return;
      }
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
      {clientId==="claude"&&<ClaudeProfile disabled={busy} beforeChange={async()=>!dirty||await confirmAction("切换配置会丢弃未保存的修改。")} onChanged={()=>void load()} notify={onMessage}/>}
      <div className="config-heading">
        <ClientSelection client={clientId} select={select} disabled={busy} />
        <div className="inline">
          {clientId === "claude" && (
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
          {clientId === "claude" ? "settings.json" : "config.toml"}
        </span>
        <div className="inline">
          {dirty && <span className="status amber">未保存</span>}
          {clientId === "claude" && dirty && (
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
          {clientId === "claude" && !visual && (
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
              void command<string>("read_previous_config", { clientId })
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
      ) : (
        <div className="editor-body">
          <CodeMirror
            value={text}
            onChange={update}
            theme={theme}
            height="100%"
            extensions={extensions}
            aria-label={clientId === "claude" ? "JSON 编辑器" : "TOML 编辑器"}
            basicSetup={{
              foldGutter: true,
              autocompletion: false,
              highlightActiveLine: true,
            }}
          />
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
