import { useMemo, useRef } from "react";
import CodeMirror from "@uiw/react-codemirror";
import { StreamLanguage } from "@codemirror/language";
import { toml } from "@codemirror/legacy-modes/mode/toml";
import { yaml } from "@codemirror/legacy-modes/mode/yaml";
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
import { sensitive } from "./claude-settings";
import { errorOf } from "./types";

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

const yamlExtensions = [
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

export type CodeMirrorSurfaceProps =
  | {
      mode: "yaml";
      ariaLabel: string;
      value: string;
      theme: "light" | "dark";
      editable: boolean;
      onChange: (value: string) => void;
    }
  | {
      mode: "json" | "toml";
      ariaLabel: string;
      value: string;
      theme: "light" | "dark";
      maskSecrets: boolean;
      onSave: () => void;
      validateText: (text: string) => Promise<void>;
      onChange: (value: string) => void;
    };

export default function CodeMirrorSurface(props: CodeMirrorSurfaceProps) {
  if (props.mode === "yaml") {
    return (
      <CodeMirror
        aria-label={props.ariaLabel}
        value={props.value}
        theme={props.theme}
        height="100%"
        extensions={yamlExtensions}
        editable={props.editable}
        onChange={props.onChange}
      />
    );
  }
  return <ConfigCodeMirror {...props} />;
}

function ConfigCodeMirror({
  mode,
  ariaLabel,
  value,
  theme,
  maskSecrets,
  onSave,
  validateText,
  onChange,
}: Extract<CodeMirrorSurfaceProps, { mode: "json" | "toml" }>) {
  const saveRef = useRef(onSave);
  const validateRef = useRef(validateText);
  saveRef.current = onSave;
  validateRef.current = validateText;

  const extensions = useMemo(
    () => [
      mode === "json" ? json() : StreamLanguage.define(toml),
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
      ...(mode === "json" && maskSecrets ? [maskedJson] : []),
      linter(
        async (view) => {
          try {
            await validateRef.current(view.state.doc.toString());
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
    [mode, maskSecrets],
  );

  return (
    <CodeMirror
      value={value}
      onChange={onChange}
      theme={theme}
      height="100%"
      extensions={extensions}
      aria-label={ariaLabel}
      basicSetup={{
        foldGutter: true,
        autocompletion: false,
        highlightActiveLine: true,
      }}
    />
  );
}
