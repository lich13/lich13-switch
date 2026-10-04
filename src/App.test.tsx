import {
  render,
  screen,
  waitFor,
  fireEvent,
  act,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, it, expect, vi } from "vitest";
import type { ViewState, ConfigDocument } from "./types";
const mocks = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (p: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  preview: false,
  command: mocks.command,
  subscribe: vi.fn(async (event: string, fn: (p: unknown) => void) => {
    mocks.listeners.set(event, fn);
    return () => mocks.listeners.delete(event);
  }),
}));
vi.mock("@uiw/react-codemirror", () => ({
  default: ({
    value,
    onChange,
  }: {
    value: string;
    onChange: (s: string) => void;
  }) => (
    <textarea
      aria-label="TOML 编辑器"
      value={value}
      onChange={(e) => onChange(e.target.value)}
    />
  ),
}));
import App from "./App";
import { gatewayDemo } from "./gateway-preview";
let state: ViewState;
let doc: ConfigDocument;
let pendingImports: { id: string; name: string; baseUrl: string }[];
beforeEach(() => {
  mocks.listeners.clear();
  mocks.command.mockReset();
  pendingImports = [];
  state = {
    accounts: [
      {
        id: "a",
        name: "个人账号",
        email: "person@example.invalid",
        kind: "chatgpt",
        current: true,
        updatedAt: 1,
      },
      {
        id: "b",
        name: "工作账号",
        email: null,
        kind: "apiKey",
        current: false,
        updatedAt: 1,
      },
    ],
    authRevision: "auth-1",
    configRevision: "cfg-1",
    currentState: "saved",
    preferences: { codexHome: "/test/.codex", cliPath: "", theme: "dark" },
    authSource: {
      provider: "openai",
      credentialStore: "file",
      inlineToken: false,
      envKey: false,
      commandAuth: false,
      requiresOpenaiAuth: true,
      warning: null,
    },
    error: null,
  };
  doc = {
    clientId: "codex",
    guarded: false,
    canRestore: false,
    text: '# keep\nmodel = "original"\n',
    revision: "cfg-1",
    path: "/test/.codex/config.toml",
  };
  mocks.command.mockImplementation(
    async (name: string, args: Record<string, unknown>) => {
      switch (name) {
        case "get_provider_imports":
          return structuredClone(pendingImports);
        case "get_gateway":
          return structuredClone(gatewayDemo);
        case "get_state":
          return structuredClone(state);
        case "get_login":
          return {
            phase: "idle",
            mode: "",
            url: null,
            code: null,
            message: "",
          };
        case "read_config":
          return { ...doc };
        case "save_config":
          if (String(args.text).includes("bad = ["))
            throw { code: "TOML", message: "TOML 语法错误", line: 2 };
          if (args.expectedRevision !== doc.revision)
            throw {
              code: "CONFLICT",
              message: "配置已被其他程序修改。草稿已保留",
            };
          doc = { ...doc, text: String(args.text), revision: "cfg-2" };
          return { ...doc };
        case "set_preferences": {
          state = {
            ...state,
            preferences: args.preferences as ViewState["preferences"],
          };
          return structuredClone(state);
        }
        case "check_for_updates":
          return {
            hasUpdate: false,
            currentVersion: "0.12.1",
            latestVersion: "0.12.1",
            releaseUrl: "https://github.com/lich13/lich13-switch/releases",
            asset: null,
          };
        case "notification_permission":
          return { permission: "granted", error: null };
        case "open_github":
        case "open_update_release":
          return;
        case "switch_account":
          return {
            ...state,
            accounts: state.accounts.map((a) => ({
              ...a,
              current: a.id === args.id,
            })),
          };
        case "start_login":
          return {
            phase: "waiting",
            mode: "browser",
            url: null,
            code: null,
            message: "请在浏览器完成登录",
          };
        default:
          return;
      }
    },
  );
});

describe("settings presentation", () => {
  it("removes path controls and previews the selected theme immediately", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: /^设置/ }));
    expect(screen.queryByText("Codex 配置目录")).not.toBeInTheDocument();
    expect(screen.queryByText("Claude Code 配置目录")).not.toBeInTheDocument();
    expect(screen.queryByText("Codex CLI 路径")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "浅色" }));
    expect(document.documentElement.dataset.theme).toBe("light");
    await user.click(screen.getByRole("button", { name: "关闭对话框" }));
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  it("persists the selected theme while retaining hidden path preferences", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: /^设置/ }));
    await user.click(screen.getByRole("button", { name: "浅色" }));
    await user.click(screen.getByRole("button", { name: "保存设置" }));
    await waitFor(() =>
      expect(mocks.command).toHaveBeenCalledWith("set_preferences", {
        preferences: expect.objectContaining({
          theme: "light",
          codexHome: "/test/.codex",
          cliPath: "",
        }),
      }),
    );
    expect(document.documentElement.dataset.theme).toBe("light");
    await user.click(screen.getByRole("button", { name: /^设置/ }));
    expect(screen.getByRole("button", { name: "浅色" })).toHaveClass("active");
  });

  it("checks updates and opens the fixed GitHub project", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: /^设置/ }));
    await user.click(screen.getByRole("button", { name: "检查更新" }));
    expect(await screen.findByText("已是最新版本")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "GitHub" }));
    expect(mocks.command).toHaveBeenCalledWith("open_github");
  });
});
describe("user workflows", () => {
  it("copies active device login values and keeps API-key accounts without a manual add option", async () => {
    const user = userEvent.setup();
    render(<App />);
    expect(await screen.findByRole("button", { name: "切换到 工作账号" })).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "添加账号" }));
    expect(screen.queryByRole("button", { name: "API Key" })).not.toBeInTheDocument();
    const session = { phase: "waiting", mode: "device", url: null as string | null, code: null as string | null, message: "等待授权" };
    act(() => mocks.listeners.get("login-state")?.(session));
    expect(screen.getByRole("button", { name: "复制链接" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "复制设备码" })).toBeDisabled();
    session.url = "https://auth.openai.com/codex/device";
    session.code = "ABCD-EFGH";
    act(() => mocks.listeners.get("login-state")?.({ ...session }));
    await user.click(screen.getByRole("button", { name: "复制链接" }));
    expect(mocks.command).toHaveBeenCalledWith("copy_login_value", { kind: "url" });
    expect(screen.getByRole("button", { name: "复制链接" })).toHaveTextContent("已复制");
    await user.click(screen.getByRole("button", { name: "复制设备码" }));
    expect(mocks.command).toHaveBeenCalledWith("copy_login_value", { kind: "code" });
    mocks.command.mockRejectedValueOnce({ code: "CLIPBOARD", message: "无法写入剪贴板，请重试" });
    await user.click(screen.getByRole("button", { name: "复制链接" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("无法写入剪贴板");
    act(() => mocks.listeners.get("login-state")?.({ ...session, phase: "cancelling" }));
    expect(screen.getByRole("button", { name: "复制链接" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "复制设备码" })).toBeDisabled();
    act(() => mocks.listeners.get("login-state")?.({ ...session, phase: "cancelled", url: null, code: null }));
    expect(screen.queryByRole("button", { name: "复制链接" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "导入凭据" }));
    expect(screen.getByRole("button", { name: "选择凭据文件" })).toBeInTheDocument();
  });
  it("keeps provider secrets in the open draft across activation and protects explicit navigation", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "网关" }));
    await u.click(await screen.findByRole("button", { name: "添加" }));
    await u.type(
      screen.getByLabelText("base_url"),
      "https://draft.example.invalid/v1",
    );
    await u.type(
      screen.getByLabelText("experimental_bearer_token"),
      "draft-fixture-key",
    );
    fireEvent(window, new Event("blur"));
    fireEvent(window, new Event("focus"));
    act(() => {
      mocks.listeners.get("gateway-state")?.({
        ...structuredClone(gatewayDemo),
        waitingRequests: 2,
      });
      mocks.listeners.get("navigate")?.("gateway");
    });
    expect(screen.getByLabelText("experimental_bearer_token")).toHaveValue(
      "draft-fixture-key",
    );
    await act(async () => {
      void mocks.listeners.get("navigate")?.("accounts");
    });
    await u.click(
      within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
        "button",
        { name: "取消" },
      ),
    );
    expect(screen.getByLabelText("base_url")).toHaveValue(
      "https://draft.example.invalid/v1",
    );
    await act(async () => {
      void mocks.listeners.get("navigate")?.("accounts");
    });
    await u.click(
      within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
        "button",
        { name: "放弃修改" },
      ),
    );
    expect(
      screen.queryByLabelText("experimental_bearer_token"),
    ).not.toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: "网关" }));
    await u.click(await screen.findByRole("button", { name: "添加" }));
    expect(screen.getByLabelText("experimental_bearer_token")).toHaveValue("");
  });
  it("queues a provider import behind the active API draft without replacing it", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "网关" }));
    await u.click(await screen.findByRole("button", { name: "添加" }));
    await u.type(
      screen.getByLabelText("base_url"),
      "https://draft.example.invalid/v1",
    );
    await u.type(
      screen.getByLabelText("experimental_bearer_token"),
      "draft-fixture-key",
    );
    pendingImports = [
      {
        id: "queued-import",
        name: "链接供应商",
        baseUrl: "https://import.example.invalid/v1",
      },
    ];
    await act(async () => mocks.listeners.get("provider-imports")?.(null));
    expect(screen.getByLabelText("base_url")).toHaveValue(
      "https://draft.example.invalid/v1",
    );
    expect(screen.getByLabelText("experimental_bearer_token")).toHaveValue(
      "draft-fixture-key",
    );
    expect(
      screen.queryByRole("heading", { name: "导入供应商" }),
    ).not.toBeInTheDocument();
    expect(screen.getAllByRole("dialog")).toHaveLength(1);
    await u.click(screen.getByRole("button", { name: "取消" }));
    expect(
      await screen.findByRole("heading", { name: "导入供应商" }),
    ).toBeInTheDocument();
    expect(screen.getByText("链接供应商")).toBeInTheDocument();
    expect(
      mocks.command.mock.calls.some(
        ([name]) => name === "confirm_provider_import",
      ),
    ).toBe(false);
  });
  it("preserves Windows CRLF when the editor changes content", async () => {
    doc.text = '# keep\r\nmodel = "original"\r\n';
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "配置" }));
    fireEvent.change(await screen.findByLabelText("TOML 编辑器"), {
      target: { value: '# keep\nmodel = "updated"\n' },
    });
    await u.click(screen.getByRole("button", { name: /保存/ }));
    await waitFor(() =>
      expect(mocks.command).toHaveBeenCalledWith("save_config", {
        clientId: "codex",
        text: '# keep\r\nmodel = "updated"\r\n',
        expectedRevision: "cfg-1",
      }),
    );
  });
  it("defers queued imports until an unsaved configuration draft is saved", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "配置" }));
    const editor = await screen.findByLabelText("TOML 编辑器");
    const draft = '# keep\nmodel = "unsaved-model"\n';
    fireEvent.change(editor, { target: { value: draft } });
    pendingImports = [
      {
        id: "config-import",
        name: "等待草稿的供应商",
        baseUrl: "https://queued.example.invalid/v1",
      },
    ];
    await act(async () => mocks.listeners.get("provider-imports")?.(null));
    expect(editor).toHaveValue(draft);
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: /保存/ }));
    expect(
      await screen.findByRole("heading", { name: "导入供应商" }),
    ).toBeInTheDocument();
    expect(screen.getByText("等待草稿的供应商")).toBeInTheDocument();
    expect(mocks.command).toHaveBeenCalledWith("save_config", {
      clientId: "codex",
      text: draft,
      expectedRevision: "cfg-1",
    });
    expect(
      mocks.command.mock.calls.some(
        ([name]) => name === "confirm_provider_import",
      ),
    ).toBe(false);
  });
  it("switches through the native command without saving config", async () => {
    const u = userEvent.setup();
    render(<App />);
    await u.click(
      await screen.findByRole("button", { name: "切换到 工作账号" }),
    );
    expect(mocks.command).toHaveBeenCalledWith("switch_account", {
      id: "b",
      expectedRevision: "auth-1",
    });
    expect(await screen.findByRole("status")).toHaveTextContent("文件已切换");
    expect(mocks.command.mock.calls.some((c) => c[0] === "save_config")).toBe(
      false,
    );
  });
  it("preserves unsaved drafts when external state changes and blocks conflicting saves", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "配置" }));
    const edit = await screen.findByLabelText("TOML 编辑器");
    fireEvent.change(edit, { target: { value: '# my draft\nmodel="draft"' } });
    doc.revision = "external";
    mocks.listeners.get("switch-state")?.({
      ...state,
      configRevision: "external",
    });
    expect(await screen.findByText(/磁盘配置已变化/)).toBeInTheDocument();
    await u.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent("草稿已保留");
    expect(edit).toHaveValue('# my draft\nmodel="draft"');
  });
  it("keeps invalid TOML in the editor after save failure", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "配置" }));
    const edit = await screen.findByLabelText("TOML 编辑器");
    fireEvent.change(edit, { target: { value: "# keep\nbad = [" } });
    await u.click(screen.getByRole("button", { name: /保存/ }));
    expect(await screen.findByRole("alert")).toHaveTextContent("TOML 语法错误");
    expect(edit).toHaveValue("# keep\nbad = [");
  });
  it("cancels the active official login", async () => {
    const u = userEvent.setup();
    render(<App />);
    await u.click(await screen.findByRole("button", { name: "添加账号" }));
    await u.click(screen.getByRole("button", { name: "使用浏览器登录" }));
    await u.click(await screen.findByRole("button", { name: "取消登录" }));
    expect(mocks.command).toHaveBeenCalledWith("cancel_login");
  });
  it("submits only a masked callback draft for the active browser session", async () => {
    const u = userEvent.setup();
    render(<App />);
    await u.click(await screen.findByRole("button", { name: "添加账号" }));
    await u.click(screen.getByRole("button", { name: "使用浏览器登录" }));
    act(() =>
      mocks.listeners.get("login-state")?.({
        phase: "waiting",
        mode: "browser",
        url: "https://auth.openai.com/codex",
        code: null,
        message: "等待授权",
        callbackReady: true,
      }),
    );
    const input = await screen.findByLabelText("回调地址");
    expect(input).toHaveAttribute("type", "password");
    mocks.command.mockResolvedValueOnce({
      phase: "waiting",
      mode: "browser",
      url: "https://auth.openai.com/codex",
      code: null,
      message: "已提交，等待登录完成",
      callbackReady: true,
    });
    await u.type(input, "http://127.0.0.1:1455/success?id_token=redacted");
    await u.click(screen.getByRole("button", { name: "完成登录" }));
    expect(mocks.command).toHaveBeenCalledWith("complete_login_callback", {
      callbackUrl: "http://127.0.0.1:1455/success?id_token=redacted",
    });
    act(() =>
      mocks.listeners.get("login-state")?.({
        phase: "cancelling",
        mode: "browser",
        url: null,
        code: null,
        message: "正在取消",
        callbackReady: false,
      }),
    );
    expect(screen.queryByLabelText("回调地址")).not.toBeInTheDocument();
  });
  it("receives tray changes and refreshes the selected account", async () => {
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    mocks.listeners.get("switch-state")?.({
      ...state,
      accounts: state.accounts.map((a) => ({ ...a, current: a.id === "b" })),
    });
    await waitFor(() =>
      expect(
        screen.getByRole("button", { name: "切换到 个人账号" }),
      ).toBeInTheDocument(),
    );
    expect(
      screen.queryByRole("button", { name: "切换到 工作账号" }),
    ).not.toBeInTheDocument();
  });
  it("asks before abandoning a config draft", async () => {
    const u = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await u.click(screen.getByRole("button", { name: "配置" }));
    fireEvent.change(await screen.findByLabelText("TOML 编辑器"), {
      target: { value: 'model="draft"' },
    });
    await u.click(screen.getByRole("button", { name: "账号" }));
    await u.click(
      within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
        "button",
        { name: "取消" },
      ),
    );
    expect(screen.getByLabelText("TOML 编辑器")).toHaveValue('model="draft"');
  });
});

describe("quota and notification settings", () => {
  it("keeps the default interval at 60 seconds, accepts 10 seconds and can disable auto refresh", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    expect(document.querySelector(".topbar")).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: /^设置/ }));

    const enabled = screen.getByRole("checkbox", { name: "额度自动刷新" });
    expect(enabled).toBeChecked();
    const interval = screen.getByRole("spinbutton", { name: "额度刷新间隔" });
    expect(interval).toHaveValue(60);
    expect(interval).toHaveAttribute("min", "10");
    expect(interval).toHaveAttribute("max", "86400");
    await user.clear(interval);
    await user.type(interval, "10");
    await user.click(screen.getByRole("button", { name: "保存设置" }));
    await waitFor(() =>
      expect(mocks.command).toHaveBeenCalledWith("set_preferences", {
        preferences: expect.objectContaining({ quotaRefreshSeconds: 10 }),
      }),
    );

    await user.click(screen.getByRole("button", { name: /^设置/ }));
    expect(screen.getByRole("spinbutton", { name: "额度刷新间隔" })).toHaveValue(10);
    await user.click(screen.getByRole("checkbox", { name: "额度自动刷新" }));
    expect(screen.queryByRole("spinbutton", { name: "额度刷新间隔" })).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "保存设置" }));
    await waitFor(() =>
      expect(mocks.command).toHaveBeenLastCalledWith("set_preferences", {
        preferences: expect.objectContaining({ quotaRefreshSeconds: 0 }),
      }),
    );
  });

  it("persists the system notification switch with the same preferences save", async () => {
    const user = userEvent.setup();
    render(<App />);
    await screen.findByRole("heading", { name: "账号", level: 1 });
    await user.click(screen.getByRole("button", { name: /^设置/ }));
    const notifications = screen.getByRole("checkbox", { name: "系统提醒" });
    expect(notifications).toBeChecked();
    await user.click(notifications);
    expect(notifications).not.toBeChecked();
    await user.click(screen.getByRole("button", { name: "保存设置" }));
    await waitFor(() =>
      expect(mocks.command).toHaveBeenLastCalledWith("set_preferences", {
        preferences: expect.objectContaining({ systemNotifications: false }),
      }),
    );
  });
});
