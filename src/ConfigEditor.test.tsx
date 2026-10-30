import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, it, expect, vi } from "vitest";
import type { ConfigDocument } from "./types";
const mocks = vi.hoisted(() => ({
  command: vi.fn(),
  events: new Map<string, (p: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  command: mocks.command,
  subscribe: vi.fn(async (event, fn) => {
    mocks.events.set(event, fn);
    return () => mocks.events.delete(event);
  }),
}));
vi.mock("@uiw/react-codemirror", () => ({
  default: ({
    value,
    onChange,
    "aria-label": label,
  }: {
    value: string;
    onChange: (s: string) => void;
    "aria-label": string;
  }) => (
    <textarea
      aria-label={label}
      value={value}
      onChange={(e) => onChange(e.target.value)}
    />
  ),
}));
import ConfigEditor from "./ConfigEditor";
import { get } from "./claude-settings";
import {
  getCachedCodeMirrorSurface,
  loadCodeMirrorSurface,
  resetCodeMirrorLoaderForTests,
} from "./codeMirrorLoader";
let doc: ConfigDocument;
beforeEach(() => {
  resetCodeMirrorLoaderForTests();
  localStorage.clear();
  localStorage.setItem("lich13-switch.config.client", "claude");
  mocks.events.clear();
  mocks.command.mockReset();
  doc = {
    clientId: "claude",
    path: "/fixture/settings.json",
    text: '{"model":"old","env":{"ANTHROPIC_MODEL":"effective","ANTHROPIC_AUTH_TOKEN":"fixture-secret"},"future":0}',
    revision: "r1",
    guarded: false,
    canRestore: true,
  };
  mocks.command.mockImplementation(async (name, args) => {
    if (name === "read_config") return { ...doc };
    if (name === "validate_config") {
      get(args.text, []);
      return;
    }
    if (name === "read_previous_config") return '{"language":"中文"}';
    if (name === "save_config") {
      get(args.text, []);
      if (args.expectedRevision !== doc.revision)
        throw { code: "CONFLICT", message: "配置已变化，草稿已保留" };
      doc = { ...doc, text: args.text, revision: "r2" };
      return { ...doc };
    }
  });
});
function mount() {
  return render(
    <ConfigEditor
      revision="codex-only"
      home="/codex"
      claudeHome="/claude"
      theme="dark"
      onDirty={vi.fn()}
      onMessage={vi.fn()}
    />,
  );
}
it("shares visual/source drafts, edits the effective model, and saves once with Mod+S", async () => {
  const u = userEvent.setup();
  mount();
  const model = await screen.findByLabelText("默认模型");
  expect(getCachedCodeMirrorSurface()).toBeNull();
  expect(model).toHaveValue("effective");
  fireEvent.change(model, { target: { value: "manual-model" } });
  await u.click(screen.getByRole("button", { name: "JSON" }));
  const source = (await screen.findByLabelText(
    "JSON 编辑器",
  )) as HTMLTextAreaElement;
  expect(getCachedCodeMirrorSurface()).not.toBeNull();
  expect(JSON.parse(source.value).model).toBe("old");
  expect(JSON.parse(source.value).env.ANTHROPIC_MODEL).toBe("manual-model");
  await u.click(screen.getByRole("button", { name: "可视化" }));
  expect(screen.getByLabelText("默认模型")).toHaveValue("manual-model");
  fireEvent.keyDown(window, { key: "s", ctrlKey: true });
  await waitFor(() =>
    expect(
      mocks.command.mock.calls.filter(([n]) => n === "save_config"),
    ).toHaveLength(1),
  );
  expect(doc.text).toContain('"future":0');
  expect(mocks.command).toHaveBeenCalledWith(
    "save_config",
    expect.objectContaining({ clientId: "claude", expectedRevision: "r1" }),
  );
  expect(localStorage.getItem("lich13-switch.config.client")).toBe("claude");
  expect(Object.values(localStorage).join()).not.toContain("fixture-secret");
});
it("keeps invalid source, and protects drafts from other-client and external events", async () => {
  const u = userEvent.setup();
  mount();
  await screen.findByLabelText("默认模型");
  await u.click(screen.getByRole("button", { name: "JSON" }));
  const source = await screen.findByLabelText("JSON 编辑器");
  fireEvent.change(source, { target: { value: '{"env":' } });
  await u.click(screen.getByRole("button", { name: "可视化" }));
  expect(source).toHaveValue('{"env":');
  expect(screen.getByRole("alert")).toBeInTheDocument();
  await act(async () =>
    mocks.events.get("config-state")?.({
      clientId: "codex",
      revision: "new",
      guarded: true,
    }),
  );
  expect(source).toHaveValue('{"env":');
  await act(async () =>
    mocks.events.get("config-state")?.({
      clientId: "claude",
      revision: "new",
      guarded: true,
    }),
  );
  expect(screen.getByText(/磁盘配置已变化/)).toBeInTheDocument();
  expect(source).toHaveValue('{"env":');
});
it("locks managed connection fields and keeps preferences editable", async () => {
  doc.guarded = true;
  const u = userEvent.setup();
  mount();
  await screen.findByLabelText("默认模型");
  await u.click(screen.getByRole("button", { name: "环境变量" }));
  expect(screen.getByLabelText("ANTHROPIC_AUTH_TOKEN")).toBeDisabled();
  expect(screen.getByLabelText("ANTHROPIC_AUTH_TOKEN")).toHaveAttribute(
    "type",
    "password",
  );
  await u.click(screen.getByRole("button", { name: "偏好" }));
  await u.selectOptions(screen.getByLabelText("自动记忆"), "false");
  await u.click(screen.getByRole("button", { name: "保存" }));
  await waitFor(() =>
    expect(JSON.parse(doc.text).autoMemoryEnabled).toBe(false),
  );
  expect(JSON.parse(doc.text).env.ANTHROPIC_AUTH_TOKEN).toBe("fixture-secret");
});
it("restores previous configuration only into the draft and confirms client changes", async () => {
  const u = userEvent.setup();
  mount();
  await screen.findByLabelText("默认模型");
  await u.click(screen.getByLabelText("恢复上次配置"));
  await waitFor(() => expect(screen.getByText("未保存")).toBeInTheDocument());
  expect(
    mocks.command.mock.calls.some(([name]) => name === "save_config"),
  ).toBe(false);
  await u.click(screen.getByRole("button", { name: "Codex" }));
  const dialog = await screen.findByRole("dialog", { name: "确认操作" });
  await u.click(within(dialog).getByRole("button", { name: "取消" }));
  expect(screen.getByText("settings.json")).toBeInTheDocument();
  expect(screen.getByText("未保存")).toBeInTheDocument();
  await u.click(screen.getByLabelText("撤销草稿并重新读取"));
  await u.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "取消" },
    ),
  );
  expect(screen.getByText("未保存")).toBeInTheDocument();
  await u.click(screen.getByLabelText("撤销草稿并重新读取"));
  await u.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "放弃修改" },
    ),
  );
  await waitFor(() =>
    expect(screen.queryByText("未保存")).not.toBeInTheDocument(),
  );
  expect(
    mocks.command.mock.calls.some(([name]) => name === "save_config"),
  ).toBe(false);
});

it("edits Antigravity app config across visual and JSON modes, diffs, restores backup, and guards conflicts", async () => {
  const u = userEvent.setup();
  localStorage.setItem("lich13-switch.config.client", "antigravity");
  const enumPrefixes: Record<string, string> = {
    themeMode: "THEME_MODE_",
    autoExecutionPolicy: "CASCADE_COMMANDS_AUTO_EXECUTION_",
  };
  const validPrefixEnum = (val: unknown, prefix: string) =>
    typeof val === "string" &&
    val.startsWith(prefix) &&
    /^[A-Z0-9_]+$/.test(val.slice(prefix.length));
  const appDoc: ConfigDocument = {
    ...doc,
    clientId: "antigravity",
    path: "/fixture/config.json",
    canRestore: true,
    text: '{"userSettings":{"themeMode":"THEME_MODE_DARK","conversationWidth":"CONVERSATION_WIDTH_UNSPECIFIED","autoExecutionPolicy":"eager"},"plugins":{"example.invalid":{"enabled":true}},"future":7}',
  };
  mocks.command.mockImplementation(async (name, args) => {
    if (name === "read_antigravity_config") return { ...appDoc };
    if (name === "validate_antigravity_config") {
      const parsed = get(args.text, []);
      if (
        !parsed ||
        typeof parsed !== "object" ||
        Array.isArray(parsed) ||
        (parsed.userSettings !== undefined &&
          (typeof parsed.userSettings !== "object" ||
            parsed.userSettings === null ||
            Array.isArray(parsed.userSettings)))
      ) {
        throw { code: "CONFIG_TYPE", message: "userSettings 必须是对象" };
      }
      const settings = (parsed.userSettings ?? {}) as Record<string, unknown>;
      for (const [key, prefix] of Object.entries(enumPrefixes)) {
        if (
          settings[key] !== undefined &&
          !validPrefixEnum(settings[key], prefix)
        ) {
          throw {
            code: "CONFIG_TYPE",
            message: `userSettings.${key} 枚举值无效`,
          };
        }
      }
      return;
    }
    if (name === "read_previous_antigravity_config")
      return '{"userSettings":{"themeMode":"THEME_MODE_INHERIT"},"future":7}';
    if (name === "save_antigravity_config")
      throw { code: "CONFLICT", message: "配置已变化，草稿已保留" };
  });
  mount();
  const theme = await screen.findByLabelText("主题模式");
  expect(screen.getByText("config.json")).toBeInTheDocument();
  expect(theme).toHaveValue("THEME_MODE_DARK");
  expect(screen.getByLabelText("对话宽度")).toHaveValue(
    "CONVERSATION_WIDTH_UNSPECIFIED",
  );
  expect(screen.getByRole("option", { name: "未指定（默认）" })).toBeInTheDocument();
  await u.selectOptions(theme, "THEME_MODE_LIGHT");

  // Historical/unknown enum values on disk are visibly flagged and preserved until reselected.
  await u.click(screen.getByRole("button", { name: "执行与安全" }));
  const execPolicy = screen.getByLabelText("命令自动执行策略");
  expect(execPolicy).toHaveValue("eager");
  expect(
    screen.getByRole("option", { name: "未知当前值（eager）" }),
  ).toBeInTheDocument();
  await u.selectOptions(
    execPolicy,
    "CASCADE_COMMANDS_AUTO_EXECUTION_PROCEED_IN_SANDBOX",
  );
  await u.selectOptions(screen.getByLabelText("终端沙盒"), "true");

  // Plugin summary renders in the remote/plugins tab.
  await u.click(screen.getByRole("button", { name: "远程与插件" }));
  expect(screen.getByText("example.invalid")).toBeInTheDocument();

  // Diff modal works for Antigravity drafts.
  await u.click(screen.getByLabelText("查看改动"));
  const diffDialog = await screen.findByRole("dialog", { name: "配置改动" });
  expect(
    within(diffDialog).getByText("userSettings.themeMode"),
  ).toBeInTheDocument();
  await u.click(within(diffDialog).getByLabelText("关闭"));

  // Switch to JSON mode: official enum values, unknown fields, and plugins remain intact.
  await u.click(screen.getByRole("button", { name: "JSON" }));
  const source = (await screen.findByLabelText(
    "JSON 编辑器",
  )) as HTMLTextAreaElement;
  expect(JSON.parse(source.value)).toEqual({
    userSettings: {
      themeMode: "THEME_MODE_LIGHT",
      conversationWidth: "CONVERSATION_WIDTH_UNSPECIFIED",
      autoExecutionPolicy:
        "CASCADE_COMMANDS_AUTO_EXECUTION_PROCEED_IN_SANDBOX",
      enableTerminalSandbox: true,
    },
    plugins: { "example.invalid": { enabled: true } },
    future: 7,
  });

  // Bogus short enum values (like "dark") and invalid userSettings types are rejected when returning to visual mode.
  fireEvent.change(source, {
    target: { value: '{"userSettings":{"themeMode":"dark"}}' },
  });
  await u.click(screen.getByRole("button", { name: "可视化" }));
  expect(
    await screen.findByText("userSettings.themeMode 枚举值无效"),
  ).toBeInTheDocument();

  fireEvent.change(source, { target: { value: '{"userSettings":0}' } });
  await u.click(screen.getByRole("button", { name: "可视化" }));
  expect(await screen.findByText("userSettings 必须是对象")).toBeInTheDocument();

  // Restore previous backup into draft via Antigravity command.
  await u.click(screen.getByLabelText("恢复上次配置"));
  await u.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "恢复" },
    ),
  );
  await waitFor(() =>
    expect(source).toHaveValue(
      '{"userSettings":{"themeMode":"THEME_MODE_INHERIT"},"future":7}',
    ),
  );

  // External config-state event for antigravity marks conflict without losing draft.
  await act(async () =>
    mocks.events.get("config-state")?.({
      clientId: "antigravity",
      revision: "external-r2",
      guarded: false,
    }),
  );
  expect(screen.getByText(/磁盘配置已变化/)).toBeInTheDocument();

  // Save uses save_antigravity_config and preserves draft on CONFLICT.
  await u.click(screen.getByRole("button", { name: "保存" }));
  expect(mocks.command).toHaveBeenCalledWith("save_antigravity_config", {
    text: '{"userSettings":{"themeMode":"THEME_MODE_INHERIT"},"future":7}',
    expectedRevision: "r1",
  });
  expect(source).toHaveValue(
    '{"userSettings":{"themeMode":"THEME_MODE_INHERIT"},"future":7}',
  );
  expect(await screen.findByText("配置已变化，草稿已保留")).toBeInTheDocument();
  expect(
    mocks.command.mock.calls.some(([name]) => name === "read_config"),
  ).toBe(false);
});

it("recovers cleanly after a transient CodeMirror lazy-load failure", async () => {
  let attempts = 0;
  resetCodeMirrorLoaderForTests(async () => {
    attempts += 1;
    if (attempts === 1) throw new Error("chunk load failed");
    return import("./CodeMirrorSurface");
  });
  await expect(loadCodeMirrorSurface()).rejects.toThrow("chunk load failed");
  expect(getCachedCodeMirrorSurface()).toBeNull();
  const Loaded = await loadCodeMirrorSurface();
  expect(Loaded).toBeTypeOf("function");
  expect(getCachedCodeMirrorSurface()).toBe(Loaded);
});
