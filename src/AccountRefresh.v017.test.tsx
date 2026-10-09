import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { Account, LoginState, ViewState } from "./types";

const mocks = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (payload: unknown) => void>(),
}));

vi.mock("./bridge", () => ({
  preview: false,
  command: mocks.command,
  subscribe: vi.fn(async (event: string, listener: (payload: unknown) => void) => {
    mocks.listeners.set(event, listener);
    return () => mocks.listeners.delete(event);
  }),
}));

import App from "./App";

let state: ViewState;
let login: LoginState;

beforeEach(() => {
  mocks.command.mockReset();
  mocks.listeners.clear();
  login = {
    phase: "idle",
    mode: "",
    url: null,
    code: null,
    message: "",
    callbackReady: false,
    targetAccountId: null,
  };
  state = {
    accounts: [
      {
        id: "fixture-current",
        name: "当前 ChatGPT",
        kind: "chatgpt",
        email: null,
        current: true,
        updatedAt: 1,
        credentialRevision: "fixture-current-revision",
      },
      {
        id: "fixture-target",
        name: "备用 ChatGPT",
        kind: "chatgpt",
        email: null,
        current: false,
        updatedAt: 1,
        credentialRevision: "fixture-target-revision",
      },
      {
        id: "fixture-api",
        name: "API 凭据",
        kind: "apiKey",
        email: null,
        current: false,
        updatedAt: 1,
        credentialRevision: "fixture-api-revision",
      },
    ],
    authRevision: "fixture-current-file-revision",
    configRevision: "fixture-config-revision",
    currentState: "saved",
    preferences: {
      codexHome: "/fixture/codex",
      cliPath: "",
      theme: "dark",
    },
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
  mocks.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      switch (name) {
        case "get_state":
          return structuredClone(state);
        case "get_login":
          return structuredClone(login);
        case "get_provider_imports":
          return [];
        case "start_login":
          login = {
            phase: "waiting",
            mode: String(args.mode),
            url: "https://example.invalid/device",
            code: "ABCD-EFGH",
            message: "等待授权",
            callbackReady: false,
            targetAccountId: String(args.targetAccountId),
          };
          return structuredClone(login);
        case "frontend_ready":
        case "copy_login_value":
        case "cancel_login":
          return;
        default:
          return;
      }
    },
  );
});

function accountRow(name: string) {
  const row = screen
    .getAllByText(name)
    .map((element) => element.closest<HTMLElement>(".account-row"))
    .find((element) => element !== null);
  if (!row) throw new Error(`Missing fixture account row: ${name}`);
  return within(row);
}

function loginCalls() {
  return mocks.command.mock.calls.filter(([name]) => name === "start_login");
}

function expectedRequest(account: Account) {
  return {
    mode: "device",
    targetAccountId: account.id,
    expectedCredentialRevision: account.credentialRevision,
  };
}

function emitLogin(update: Partial<LoginState>) {
  login = { ...login, ...update };
  const listener = mocks.listeners.get("login-state");
  if (!listener) throw new Error("Missing login-state listener");
  act(() => listener(structuredClone(login)));
}

async function mount() {
  render(<App />);
  await screen.findByRole("heading", { name: "账号", level: 1 });
  await waitFor(() =>
    expect(document.querySelectorAll(".account-row")).toHaveLength(
      state.accounts.length,
    ),
  );
}

async function openRefresh(
  user: ReturnType<typeof userEvent.setup>,
  account: Account,
) {
  await user.click(
    accountRow(account.name).getByRole("button", { name: "设备码刷新" }),
  );
  const dialog = await screen.findByRole("dialog", { name: "刷新账号凭据" });
  await waitFor(() =>
    expect(mocks.command).toHaveBeenCalledWith(
      "start_login",
      expectedRequest(account),
    ),
  );
  await waitFor(() =>
    expect(within(dialog).getByRole("button", { name: "取消登录" })).toBeEnabled(),
  );
  return dialog;
}

describe("targeted account credential refresh", () => {
  it("offers refresh on current and saved ChatGPT accounts and hides it for API keys", async () => {
    await mount();

    expect(
      accountRow("当前 ChatGPT").getByRole("button", { name: "设备码刷新" }),
    ).toBeEnabled();
    expect(
      accountRow("备用 ChatGPT").getByRole("button", { name: "设备码刷新" }),
    ).toBeEnabled();
    expect(
      accountRow("API 凭据").queryByRole("button", { name: "设备码刷新" }),
    ).not.toBeInTheDocument();
    expect(loginCalls()).toHaveLength(0);
  });

  it.each([0, 1])(
    "starts one device login with account %i and its credential revision",
    async (index) => {
      const user = userEvent.setup();
      const account = state.accounts[index];
      await mount();

      const dialog = await openRefresh(user, account);

      expect(loginCalls()).toEqual([["start_login", expectedRequest(account)]]);
      expect(
        within(dialog).getByRole("heading", { name: account.name }),
      ).toBeInTheDocument();
      expect(
        within(dialog).queryByRole("button", { name: "使用浏览器登录" }),
      ).not.toBeInTheDocument();
      expect(
        within(dialog).queryByRole("button", { name: "导入凭据" }),
      ).not.toBeInTheDocument();
      expect(
        within(dialog).queryByRole("button", { name: "API Key" }),
      ).not.toBeInTheDocument();
      expect(
        mocks.command.mock.calls.some(([name]) => name === "switch_account"),
      ).toBe(false);
      expect(login.targetAccountId).toBe(account.id);
      for (const other of state.accounts.filter(
        (item) => item.kind === "chatgpt",
      )) {
        expect(
          accountRow(other.name).getByRole("button", { name: "设备码刷新" }),
        ).toBeDisabled();
      }
    },
  );

  it("keeps device-code copy and cancellation bound to the targeted session", async () => {
    const user = userEvent.setup();
    await mount();
    const dialog = await openRefresh(user, state.accounts[1]);
    const controls = within(dialog);

    emitLogin({ url: null, code: null });
    expect(controls.getByRole("button", { name: "复制链接" })).toBeDisabled();
    expect(controls.getByRole("button", { name: "复制设备码" })).toBeDisabled();
    emitLogin({ url: "https://example.invalid/device", code: "ABCD-EFGH" });
    await user.click(controls.getByRole("button", { name: "复制链接" }));
    expect(mocks.command).toHaveBeenCalledWith("copy_login_value", { kind: "url" });
    expect(controls.getByRole("button", { name: "复制链接" })).toHaveTextContent(
      "已复制",
    );
    await user.click(controls.getByRole("button", { name: "复制设备码" }));
    expect(mocks.command).toHaveBeenCalledWith("copy_login_value", { kind: "code" });
    expect(controls.getByRole("button", { name: "复制设备码" })).toHaveTextContent(
      "已复制",
    );

    await user.click(controls.getByRole("button", { name: "取消登录" }));

    expect(mocks.command).toHaveBeenCalledWith("cancel_login");
    expect(controls.getByRole("button", { name: "取消登录" })).toBeDisabled();
    expect(controls.getByRole("button", { name: "复制链接" })).toBeDisabled();
    expect(controls.getByRole("button", { name: "复制设备码" })).toBeDisabled();
    emitLogin({
      phase: "cancelled",
      url: null,
      code: null,
      message: "登录已取消",
    });
    expect(
      controls.queryByRole("button", { name: "复制链接" }),
    ).not.toBeInTheDocument();
    expect(
      controls.queryByRole("button", { name: "复制设备码" }),
    ).not.toBeInTheDocument();
    expect(loginCalls()).toHaveLength(1);
  });

  it("preserves a synchronous cancelled event when the cancellation command resolves later", async () => {
    const user = userEvent.setup();
    await mount();
    const controls = within(await openRefresh(user, state.accounts[1]));
    const original = mocks.command.getMockImplementation()!;
    let finishCancellation!: () => void;
    const cancellation = new Promise<void>((resolve) => {
      finishCancellation = resolve;
    });
    mocks.command.mockImplementation(
      (name: string, args: Record<string, unknown> = {}) => {
        if (name === "cancel_login") {
          emitLogin({
            phase: "cancelled",
            url: null,
            code: null,
            message: "登录已取消",
          });
          return cancellation;
        }
        return original(name, args);
      },
    );

    await user.click(controls.getByRole("button", { name: "取消登录" }));

    expect(controls.getByText("登录已取消")).toBeInTheDocument();
    expect(controls.getByRole("button", { name: "使用设备码" })).toBeDisabled();
    expect(
      controls.queryByRole("button", { name: "取消登录" }),
    ).not.toBeInTheDocument();

    await act(async () => finishCancellation());

    expect(controls.getByText("登录已取消")).toBeInTheDocument();
    expect(controls.queryByText("正在取消登录…")).not.toBeInTheDocument();
    expect(controls.getByRole("button", { name: "使用设备码" })).toBeEnabled();
    expect(
      controls.queryByRole("button", { name: "复制设备码" }),
    ).not.toBeInTheDocument();
  });

  it("restores waiting after cancellation fails, shows the error, and permits a retry", async () => {
    const user = userEvent.setup();
    await mount();
    const controls = within(await openRefresh(user, state.accounts[1]));
    const original = mocks.command.getMockImplementation()!;
    let failCancellation!: (error: Error) => void;
    const cancellation = new Promise<void>((_, reject) => {
      failCancellation = reject;
    });
    const cancel = vi.fn()
      .mockReturnValueOnce(cancellation)
      .mockResolvedValue(undefined);
    mocks.command.mockImplementation(
      (name: string, args: Record<string, unknown> = {}) =>
        name === "cancel_login" ? cancel() : original(name, args),
    );

    await user.click(controls.getByRole("button", { name: "取消登录" }));
    expect(controls.getByText("正在取消登录…")).toBeInTheDocument();
    expect(controls.getByRole("button", { name: "取消登录" })).toBeDisabled();

    await act(async () => failCancellation(new Error("取消失败，请重试")));

    expect(controls.getByRole("alert")).toHaveTextContent("取消失败，请重试");
    expect(controls.getByText("等待授权")).toBeInTheDocument();
    expect(controls.getByText("ABCD-EFGH")).toBeInTheDocument();
    expect(controls.getByRole("button", { name: "取消登录" })).toBeEnabled();
    expect(controls.getByRole("button", { name: "复制链接" })).toBeEnabled();
    expect(controls.getByRole("button", { name: "复制设备码" })).toBeEnabled();

    await user.click(controls.getByRole("button", { name: "取消登录" }));

    expect(cancel).toHaveBeenCalledTimes(2);
    expect(controls.queryByRole("alert")).not.toBeInTheDocument();
    expect(controls.getByText("正在取消登录…")).toBeInTheDocument();
    emitLogin({
      phase: "cancelled",
      url: null,
      code: null,
      message: "登录已取消",
    });
    expect(controls.getByText("登录已取消")).toBeInTheDocument();
    expect(controls.getByRole("button", { name: "使用设备码" })).toBeEnabled();
    expect(loginCalls()).toHaveLength(1);
  });

  it.each([
    ["cancelled", "登录已取消"],
    ["error", "设备码已过期"],
  ])(
    "preserves a later %s state when pending cancellation fails",
    async (phase, message) => {
      const user = userEvent.setup();
      await mount();
      const controls = within(await openRefresh(user, state.accounts[1]));
      const original = mocks.command.getMockImplementation()!;
      let failCancellation!: (error: Error) => void;
      const cancellation = new Promise<void>((_, reject) => {
        failCancellation = reject;
      });
      mocks.command.mockImplementation(
        (name: string, args: Record<string, unknown> = {}) =>
          name === "cancel_login" ? cancellation : original(name, args),
      );

      await user.click(controls.getByRole("button", { name: "取消登录" }));
      emitLogin({ phase, message, url: null, code: null });
      expect(controls.getByText(message)).toBeInTheDocument();
      expect(controls.getByRole("button", { name: "使用设备码" })).toBeDisabled();

      await act(async () => failCancellation(new Error("取消命令失败")));

      expect(controls.getByRole("alert")).toHaveTextContent("取消命令失败");
      expect(controls.getByText(message)).toBeInTheDocument();
      expect(controls.queryByText("等待授权")).not.toBeInTheDocument();
      expect(controls.queryByText("正在取消登录…")).not.toBeInTheDocument();
      expect(
        controls.queryByRole("button", { name: "取消登录" }),
      ).not.toBeInTheDocument();
      expect(
        controls.queryByRole("button", { name: "复制设备码" }),
      ).not.toBeInTheDocument();
      expect(controls.getByRole("button", { name: "使用设备码" })).toBeEnabled();
    },
  );

  it("preserves the selected account when identity validation fails and the user retries", async () => {
    const user = userEvent.setup();
    const account = state.accounts[1];
    await mount();
    const dialog = await openRefresh(user, account);

    emitLogin({
      phase: "error",
      url: null,
      code: null,
      message: "登录账号或工作区不一致，未更新指定账号",
    });

    expect(
      within(dialog).getByText("登录账号或工作区不一致，未更新指定账号"),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByRole("heading", { name: account.name }),
    ).toBeInTheDocument();
    await user.click(within(dialog).getByRole("button", { name: "使用设备码" }));
    await waitFor(() => expect(loginCalls()).toHaveLength(2));
    expect(loginCalls()).toEqual([
      ["start_login", expectedRequest(account)],
      ["start_login", expectedRequest(account)],
    ]);
    expect(
      mocks.command.mock.calls.some(([name]) => name === "import_auth_file"),
    ).toBe(false);
    expect(
      mocks.command.mock.calls.some(([name]) => name === "import_current"),
    ).toBe(false);
  });

  it("cancels a targeted login when its dialog closes and releases refresh after cancellation", async () => {
    const user = userEvent.setup();
    await mount();
    const dialog = await openRefresh(user, state.accounts[0]);

    await user.click(within(dialog).getByRole("button", { name: "关闭对话框" }));

    expect(
      screen.queryByRole("dialog", { name: "刷新账号凭据" }),
    ).not.toBeInTheDocument();
    expect(
      mocks.command.mock.calls.filter(([name]) => name === "cancel_login"),
    ).toEqual([["cancel_login"]]);
    emitLogin({
      phase: "cancelled",
      url: null,
      code: null,
      message: "登录已取消",
    });
    expect(
      accountRow("当前 ChatGPT").getByRole("button", { name: "设备码刷新" }),
    ).toBeEnabled();
    expect(
      accountRow("备用 ChatGPT").getByRole("button", { name: "设备码刷新" }),
    ).toBeEnabled();
    expect(loginCalls()).toHaveLength(1);
  });
});
