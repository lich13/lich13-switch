import { useState } from "react";
import { Plus, RotateCcw, Trash2, Search, RefreshCw } from "lucide-react";
import { command } from "./bridge";
import { errorOf } from "./types";
import {
  fields,
  groups,
  get,
  managed,
  patch,
  sensitive,
  type Field,
  type Json,
  type Path,
} from "./claude-settings";

export type Editing = {
  text: string;
  guarded: boolean;
  change: (path: Path, value: Json | undefined) => void;
};
function Scalar({
  value,
  path,
  editing,
  label,
  kind,
}: {
  value: Json | undefined;
  path: Path;
  editing: Editing;
  label: string;
  kind?: Field["kind"];
}) {
  const locked = editing.guarded && managed(path);
  if (kind === "boolean" || typeof value === "boolean")
    return (
      <select
        aria-label={label}
        disabled={locked}
        value={value === undefined ? "default" : String(value)}
        onChange={(e) =>
          editing.change(
            path,
            e.target.value === "default"
              ? undefined
              : e.target.value === "true",
          )
        }
      >
        <option value="default">默认</option>
        <option value="true">开启</option>
        <option value="false">关闭</option>
      </select>
    );
  return (
    <input
      aria-label={label}
      type={
        sensitive(path)
          ? "password"
          : kind === "integer" || typeof value === "number"
            ? "number"
            : "text"
      }
      autoComplete="off"
      spellCheck={false}
      disabled={locked}
      placeholder={value === undefined ? "默认" : undefined}
      value={value === undefined || value === null ? "" : String(value)}
      step={1}
      min={kind === "integer" ? 0 : undefined}
      list={kind === "model" ? "claude-models" : undefined}
      onChange={(e) => {
        const v = e.target.value;
        if (
          kind === "integer" &&
          v &&
          (!Number.isInteger(Number(v)) || Number(v) < 0)
        )
          return;
        editing.change(
          path,
          typeof value === "number" ? (v === "" ? 0 : Number(v)) : v,
        );
      }}
    />
  );
}
export function Setting({
  field,
  editing,
  flagUnknown,
}: {
  field: Field;
  editing: Editing;
  flagUnknown?: boolean;
}) {
  const path =
    field.override && get(editing.text, field.override) !== undefined
      ? field.override
      : field.path;
  const value = get(editing.text, path);
  const unknown =
    field.kind === "select" &&
    value !== undefined &&
    !field.options?.some(([v]) => v === value);
  const unknownText =
    flagUnknown && typeof value === "string" && value.endsWith("_UNSPECIFIED")
      ? "未指定（默认）"
      : flagUnknown
        ? `未知当前值（${String(value)}）`
        : String(value);
  return (
    <div className="setting-row">
      <label htmlFor={path.join(".")}>{field.label}</label>
      <div className="setting-value">
        {field.kind === "select" ? (
          <select
            id={path.join(".")}
            aria-label={field.label}
            value={value === undefined ? "__default__" : String(value)}
            onChange={(e) =>
              editing.change(
                path,
                e.target.value === "__default__" ? undefined : e.target.value,
              )
            }
          >
            <option value="__default__">默认</option>
            {unknown && <option value={String(value)}>{unknownText}</option>}
            {field.options?.map(([v, label]) => (
              <option key={v} value={v}>
                {label}
              </option>
            ))}
          </select>
        ) : (
          <Scalar
            value={value}
            path={path}
            editing={editing}
            label={field.label}
            kind={field.kind}
          />
        )}
        <button
          className="icon-button"
          aria-label={`${field.label}恢复默认`}
          title="恢复默认"
          disabled={value === undefined}
          onClick={() => editing.change(path, undefined)}
        >
          <RotateCcw size={14} />
        </button>
      </div>
    </div>
  );
}
function StringList({
  path,
  label,
  editing,
}: {
  path: Path;
  label: string;
  editing: Editing;
}) {
  const value = get(editing.text, path),
    entries = Array.isArray(value) ? value : [];
  const [draft, setDraft] = useState("");
  return (
    <section className="settings-list-group">
      <h3>{label}</h3>
      {entries.map((entry, i) => (
        <div className="settings-list-row" key={i}>
          <input
            aria-label={`${label} ${i + 1}`}
            value={String(entry)}
            onChange={(e) => editing.change([...path, i], e.target.value)}
          />
          <button
            className="icon-button"
            aria-label={`删除${label} ${i + 1}`}
            onClick={() => editing.change([...path, i], undefined)}
          >
            <Trash2 size={14} />
          </button>
        </div>
      ))}
      <form
        className="settings-list-row"
        onSubmit={(e) => {
          e.preventDefault();
          if (!draft.trim()) return;
          editing.change(path, [...entries, draft.trim()]);
          setDraft("");
        }}
      >
        <input
          aria-label={`添加${label}`}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <button
          className="icon-button"
          aria-label={`确认添加${label}`}
          disabled={!draft.trim()}
        >
          <Plus size={15} />
        </button>
      </form>
    </section>
  );
}
function Permissions({ editing }: { editing: Editing }) {
  const [tool, setTool] = useState("Bash"),
    [target, setTarget] = useState(""),
    [action, setAction] = useState("allow");
  return (
    <>
      <section className="settings-list-group">
        <h3>权限规则</h3>
        <form
          className="rule-builder"
          onSubmit={(e) => {
            e.preventDefault();
            const p = ["permissions", action];
            const old = get(editing.text, p);
            const rule = target ? `${tool}(${target})` : tool;
            editing.change(p, [
              ...new Set([...(Array.isArray(old) ? old : []), rule]),
            ]);
            setTarget("");
          }}
        >
          <select
            aria-label="规则工具"
            value={tool}
            onChange={(e) => setTool(e.target.value)}
          >
            {["Bash", "Read", "Edit", "Write", "WebFetch", "Agent"].map((t) => (
              <option key={t}>{t}</option>
            ))}
          </select>
          <input
            aria-label="匹配内容"
            placeholder="匹配内容"
            value={target}
            onChange={(e) => setTarget(e.target.value)}
          />
          <select
            aria-label="规则处理方式"
            value={action}
            onChange={(e) => setAction(e.target.value)}
          >
            <option value="allow">允许</option>
            <option value="ask">询问</option>
            <option value="deny">拒绝</option>
          </select>
          <button className="secondary">添加</button>
        </form>
      </section>
      {[
        ["allow", "允许"],
        ["ask", "询问"],
        ["deny", "拒绝"],
        ["additionalDirectories", "额外访问目录"],
      ].map(([key, label]) => (
        <StringList
          key={key}
          path={["permissions", key]}
          label={label}
          editing={editing}
        />
      ))}
    </>
  );
}
function AddField({
  path,
  value,
  editing,
}: {
  path: Path;
  value: Json[] | { [key: string]: Json };
  editing: Editing;
}) {
  const [adding, setAdding] = useState(false),
    [name, setName] = useState(""),
    [type, setType] = useState("string");
  const array = Array.isArray(value);
  if (!adding)
    return (
      <button
        className="text-button add-setting"
        onClick={() => setAdding(true)}
      >
        <Plus size={14} />
        {array ? "添加项" : "添加字段"}
      </button>
    );
  return (
    <form
      className="settings-list-row"
      onSubmit={(e) => {
        e.preventDefault();
        if (!array && (!name.trim() || Object.hasOwn(value, name))) return;
        const initial: Record<string, Json> = {
          string: "",
          number: 0,
          boolean: false,
          array: [],
          object: {},
          null: null,
        };
        editing.change([...path, array ? value.length : name], initial[type]);
        setName("");
        setAdding(false);
      }}
    >
      {!array && (
        <input
          aria-label="字段名称"
          placeholder="字段名称"
          required
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
      )}
      <select
        aria-label="字段类型"
        value={type}
        onChange={(e) => setType(e.target.value)}
      >
        {[
          ["string", "文本"],
          ["number", "数字"],
          ["boolean", "开关"],
          ["array", "列表"],
          ["object", "对象"],
          ["null", "null"],
        ].map(([v, l]) => (
          <option key={v} value={v}>
            {l}
          </option>
        ))}
      </select>
      <button
        className="secondary"
        disabled={!array && (!name.trim() || Object.hasOwn(value, name))}
      >
        添加
      </button>
      <button
        type="button"
        className="text-button"
        onClick={() => setAdding(false)}
      >
        取消
      </button>
    </form>
  );
}
export function JsonNode({
  path,
  value,
  editing,
  depth = 0,
}: {
  path: Path;
  value: Json;
  editing: Editing;
  depth?: number;
}) {
  const label = String(path.at(-1) ?? "配置");
  const containsManaged =
    editing.guarded &&
    (managed(path) ||
      path.length === 0 ||
      (path.length === 1 && path[0] === "env"));
  if (value && typeof value === "object")
    return (
      <details className="json-node" open={depth === 0 ? true : undefined}>
        <summary>
          <span>{label}</span>
          <span className="node-count">{Object.keys(value).length}</span>
          {path.length > 0 && (
            <button
              className="icon-button"
              aria-label={`删除 ${path.join(".")}`}
              disabled={containsManaged}
              onClick={(e) => {
                e.preventDefault();
                editing.change(path, undefined);
              }}
            >
              <Trash2 size={14} />
            </button>
          )}
        </summary>
        <div className="json-children">
          {Object.entries(value).map(([key, v]) => (
            <JsonNode
              key={key}
              path={[...path, Array.isArray(value) ? Number(key) : key]}
              value={v}
              editing={editing}
              depth={depth + 1}
            />
          ))}
          <AddField path={path} value={value} editing={editing} />
        </div>
      </details>
    );
  return (
    <div className="setting-row tree-scalar">
      <span title={path.join(".")}>{label}</span>
      <div className="setting-value">
        {value === null ? (
          <code>null</code>
        ) : (
          <Scalar
            value={value}
            path={path}
            editing={editing}
            label={path.join(".")}
          />
        )}
        <button
          className="icon-button"
          disabled={containsManaged}
          aria-label={`删除 ${path.join(".")}`}
          onClick={() => editing.change(path, undefined)}
        >
          <Trash2 size={14} />
        </button>
      </div>
    </div>
  );
}
function Environment({ editing }: { editing: Editing }) {
  const [search, setSearch] = useState(""),
    [name, setName] = useState("");
  const env = get(editing.text, ["env"]);
  const rows = env && typeof env === "object" && !Array.isArray(env) ? env : {};
  return (
    <>
      <label className="config-search">
        <Search size={15} />
        <input
          aria-label="搜索环境变量"
          placeholder="搜索变量"
          value={search}
          onChange={(e) => setSearch(e.target.value)}
        />
      </label>
      <div className="env-list">
        {Object.entries(rows)
          .filter(([key]) => key.toLowerCase().includes(search.toLowerCase()))
          .map(([key, v]) => {
            const field = Object.values(fields)
              .flat()
              .find((f) => f.path[0] === "env" && f.path[1] === key);
            return (
              <div className="environment-row" key={key}>
                <label title={key}>{key}</label>
                {field?.kind === "select" ? (
                  <select
                    aria-label={key}
                    value={String(v)}
                    onChange={(e) =>
                      editing.change(["env", key], e.target.value)
                    }
                  >
                    {!field.options?.some(([value]) => value === v) && (
                      <option value={String(v)}>{String(v)}</option>
                    )}
                    {field.options?.map(([value, label]) => (
                      <option key={value} value={value}>
                        {label}
                      </option>
                    ))}
                  </select>
                ) : (
                  <Scalar
                    path={["env", key]}
                    value={v}
                    label={key}
                    editing={editing}
                    kind={field?.kind}
                  />
                )}
                <button
                  className="icon-button"
                  aria-label={`删除 ${key}`}
                  disabled={editing.guarded && managed(["env", key])}
                  onClick={() => editing.change(["env", key], undefined)}
                >
                  <Trash2 size={14} />
                </button>
              </div>
            );
          })}
      </div>
      <form
        className="settings-list-row"
        onSubmit={(e) => {
          e.preventDefault();
          if (
            /^[A-Za-z_][A-Za-z0-9_]*$/.test(name) &&
            !Object.hasOwn(rows, name)
          ) {
            editing.change(["env", name], "");
            setName("");
          }
        }}
      >
        <input
          aria-label="新环境变量名称"
          placeholder="变量名称"
          value={name}
          onChange={(e) => setName(e.target.value)}
        />
        <button
          className="secondary"
          disabled={
            !/^[A-Za-z_][A-Za-z0-9_]*$/.test(name) || Object.hasOwn(rows, name)
          }
        >
          添加
        </button>
      </form>
    </>
  );
}
export default function ClaudeSettings({
  text,
  onChange,
  guarded,
}: {
  text: string;
  onChange: (text: string) => void;
  guarded: boolean;
}) {
  const [group, setGroup] = useState<(typeof groups)[number]>("模型"),
    [error, setError] = useState(""),
    [models, setModels] = useState<string[]>([]),
    [loading, setLoading] = useState(false);
  let value: Json;
  try {
    value = get(text, [])!;
  } catch {
    return (
      <div className="banner error" role="alert">
        请在 JSON 编辑器中修正配置
      </div>
    );
  }
  const editing: Editing = {
    text,
    guarded,
    change: (path, next) => {
      try {
        if (guarded && managed(path))
          throw new Error("网关运行中，连接字段由网关管理");
        onChange(patch(text, path, next));
        setError("");
      } catch (e) {
        setError(errorOf(e).message);
      }
    },
  };
  const loadModels = async () => {
    setLoading(true);
    setError("");
    try {
      const gateway = await command("get_gateway", {
        clientId: "claude",
      });
      const id = gateway.selected ?? gateway.providers[0]?.id;
      if (!id) throw new Error("请先添加 Claude 供应商");
      const list = await command("list_provider_models", {
        clientId: "claude",
        providerId: id,
        force: true,
      });
      setModels(list.models);
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setLoading(false);
    }
  };
  return (
    <div className="claude-settings">
      <nav className="settings-tabs" aria-label="Claude 配置分组">
        {groups.map((g) => (
          <button
            key={g}
            aria-pressed={group === g}
            onClick={() => setGroup(g)}
          >
            {g}
          </button>
        ))}
      </nav>
      <div className="settings-content">
        {error && (
          <div className="inline-error" role="alert">
            {error}
          </div>
        )}
        {group === "模型" && (
          <div className="config-group-action">
            <button
              className="text-button"
              disabled={loading}
              onClick={() => void loadModels()}
            >
              <RefreshCw size={14} className={loading ? "spin" : ""} />
              读取模型
            </button>
          </div>
        )}
        <datalist id="claude-models">
          {[...new Set(["opus", "sonnet", "haiku", "opusplan", ...models])].map(
            (m) => (
              <option key={m} value={m} />
            ),
          )}
        </datalist>
        {(fields[group] ?? []).map((f) => (
          <Setting key={f.label} field={f} editing={editing} />
        ))}
        {group === "权限" && <Permissions editing={editing} />}
        {group === "环境变量" && <Environment editing={editing} />}
        {group === "高级" && (
          <JsonNode path={[]} value={value} editing={editing} />
        )}
      </div>
    </div>
  );
}
