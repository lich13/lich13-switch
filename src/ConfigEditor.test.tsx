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
let doc: ConfigDocument;
beforeEach(() => {
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
  expect(model).toHaveValue("effective");
  fireEvent.change(model, { target: { value: "manual-model" } });
  await u.click(screen.getByRole("button", { name: "JSON" }));
  const source = screen.getByLabelText("JSON 编辑器") as HTMLTextAreaElement;
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
  const source = screen.getByLabelText("JSON 编辑器");
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

it("edits Antigravity app JSON through its own commands and preserves a conflicted draft", async () => {
  localStorage.setItem("lich13-switch.config.client", "antigravity");
  const appDoc = {
    ...doc,
    clientId: "antigravity",
    path: "/fixture/config.json",
    canRestore: false,
    text: '{"userSettings":{},"future":7}',
  };
  mocks.command.mockImplementation(async (name) => {
    if (name === "read_antigravity_config") return appDoc;
    if (name === "save_antigravity_config")
      throw { code: "CONFLICT", message: "配置已变化，草稿已保留" };
  });
  mount();
  const source = await screen.findByLabelText("JSON 编辑器");
  expect(screen.getByText("config.json")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "恢复上次配置" })).toBeDisabled();
  fireEvent.change(source, {
    target: { value: '{"userSettings":{"theme":"dark"},"future":7}' },
  });
  await userEvent.setup().click(screen.getByRole("button", { name: "保存" }));
  expect(mocks.command).toHaveBeenCalledWith("save_antigravity_config", {
    text: '{"userSettings":{"theme":"dark"},"future":7}',
    expectedRevision: "r1",
  });
  expect(source).toHaveValue('{"userSettings":{"theme":"dark"},"future":7}');
  expect(await screen.findByText("配置已变化，草稿已保留")).toBeInTheDocument();
  expect(
    mocks.command.mock.calls.some(([name]) => name === "read_config"),
  ).toBe(false);
});
