import { useState } from "react";
import { errorOf } from "./types";
import { get, patch, type Field, type Json } from "./claude-settings";
import { JsonNode, Setting, type Editing } from "./ClaudeSettings";

export const antigravityGroups = [
  "外观与交互",
  "执行与安全",
  "远程与插件",
  "高级",
] as const;

export const antigravityFields: Record<string, Field[]> = {
  外观与交互: [
    {
      path: ["userSettings", "themeMode"],
      label: "主题模式",
      kind: "select",
      options: [
        ["THEME_MODE_INHERIT", "跟随系统"],
        ["THEME_MODE_DARK", "深色"],
        ["THEME_MODE_LIGHT", "浅色"],
      ],
    },
    {
      path: ["userSettings", "conversationWidth"],
      label: "对话宽度",
      kind: "select",
      options: [
        ["CONVERSATION_WIDTH_DEFAULT", "标准"],
        ["CONVERSATION_WIDTH_WIDE", "宽屏"],
        ["CONVERSATION_WIDTH_NARROW", "紧凑"],
      ],
    },
    {
      path: ["userSettings", "artifactReviewMode"],
      label: "产物审阅模式",
      kind: "select",
      options: [
        ["ARTIFACT_REVIEW_MODE_ALWAYS", "始终审阅"],
        ["ARTIFACT_REVIEW_MODE_AUTO", "自动判断"],
        ["ARTIFACT_REVIEW_MODE_TURBO", "极速免审"],
      ],
    },
    {
      path: ["userSettings", "enableSoundsForSpecialEvents"],
      label: "特殊事件提示音",
      kind: "boolean",
    },
  ],
  执行与安全: [
    {
      path: ["userSettings", "autoExecutionPolicy"],
      label: "命令自动执行策略",
      kind: "select",
      options: [
        ["CASCADE_COMMANDS_AUTO_EXECUTION_OFF", "关闭自动执行"],
        ["CASCADE_COMMANDS_AUTO_EXECUTION_AUTO", "模型按风险判断"],
        ["CASCADE_COMMANDS_AUTO_EXECUTION_PROCEED_IN_SANDBOX", "沙盒内自动执行"],
        ["CASCADE_COMMANDS_AUTO_EXECUTION_EAGER", "积极自动执行"],
      ],
    },
    {
      path: ["userSettings", "enableTerminalSandbox"],
      label: "终端沙盒",
      kind: "boolean",
    },
    {
      path: ["userSettings", "nonWorkspaceFileAccessPolicy"],
      label: "工作区外文件访问",
      kind: "select",
      options: [
        ["AGENT_SETTING_POLICY_ASK", "每次询问"],
        ["AGENT_SETTING_POLICY_ALLOW", "允许访问"],
        ["AGENT_SETTING_POLICY_DENY", "禁止访问"],
      ],
    },
    {
      path: ["userSettings", "browserJsExecutionPolicy"],
      label: "浏览器脚本执行",
      kind: "select",
      options: [
        ["BROWSER_JS_EXECUTION_POLICY_DISABLED", "禁用"],
        ["BROWSER_JS_EXECUTION_POLICY_ALWAYS_ASK", "每次询问"],
        ["BROWSER_JS_EXECUTION_POLICY_MODEL_DECIDES", "模型自动判断"],
        ["BROWSER_JS_EXECUTION_POLICY_TURBO", "自动允许"],
      ],
    },
  ],
  远程与插件: [
    {
      path: ["userSettings", "remoteControlEnabled"],
      label: "远程控制",
      kind: "boolean",
    },
    {
      path: ["userSettings", "remoteControlHostname"],
      label: "远程控制主机名",
      kind: "text",
    },
  ],
};

function PluginsOverview({ text }: { text: string }) {
  const raw = get(text, ["plugins"]);
  const entries =
    raw && typeof raw === "object" && !Array.isArray(raw)
      ? Object.entries(raw)
      : [];
  return (
    <section className="settings-list-group" aria-label="已配置插件">
      <h3>已配置插件</h3>
      {entries.length === 0 ? (
        <p className="muted">
          当前未检测到插件配置项。插件与未知字段在可视化编辑时会自动无损保留，也可在「高级」或「JSON」视图中直接维护。
        </p>
      ) : (
        <div className="ag-plugin-list">
          {entries.map(([id, item]) => {
            const count =
              item && typeof item === "object"
                ? Array.isArray(item)
                  ? `${item.length} 项`
                  : `${Object.keys(item).length} 个字段`
                : typeof item;
            return (
              <div className="ag-plugin-item" key={id}>
                <code title={id}>{id}</code>
                <span className="node-count">{count}</span>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

export default function AntigravitySettings({
  text,
  onChange,
}: {
  text: string;
  onChange: (text: string) => void;
}) {
  const [group, setGroup] =
      useState<(typeof antigravityGroups)[number]>("外观与交互"),
    [error, setError] = useState("");
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
    guarded: false,
    change: (path, next) => {
      try {
        onChange(patch(text, path, next));
        setError("");
      } catch (e) {
        setError(errorOf(e).message);
      }
    },
  };
  return (
    <div className="claude-settings antigravity-settings">
      <nav className="settings-tabs" aria-label="Antigravity 配置分组">
        {antigravityGroups.map((g) => (
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
        {(antigravityFields[group] ?? []).map((f) => (
          <Setting key={f.label} field={f} editing={editing} flagUnknown />
        ))}
        {group === "远程与插件" && <PluginsOverview text={text} />}
        {group === "高级" && (
          <JsonNode path={[]} value={value} editing={editing} />
        )}
      </div>
    </div>
  );
}
