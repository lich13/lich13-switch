import { render, screen, waitFor, act, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, it, expect, vi } from "vitest";
import type { ClientId, GatewayState, ProviderQuota } from "./types";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (s: GatewayState) => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, fn: (s: GatewayState) => void) => {
    mock.listeners.set(name, fn);
    return () => mock.listeners.delete(name);
  }),
}));
import Gateway from "./Gateway";
import { claudeGatewayDemo, gatewayDemo } from "./gateway-preview";
import { providerTestingPreview } from "./provider-testing-preview";
let state: GatewayState;
let claudeState: GatewayState;
beforeEach(() => {
  localStorage.clear();
  state = structuredClone(gatewayDemo);
  claudeState = structuredClone(claudeGatewayDemo);
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args?: { clientId?: ClientId }) => {
      if (name.startsWith("provider_test_")) return providerTestingPreview(name, args ?? {});
      if (name === "get_gateway" || name === "update_gateway")
        return structuredClone(
          args?.clientId === "claude" ? claudeState : state,
        );
    },
  );
});
afterEach(() => localStorage.clear());
describe("gateway controls", () => {
  it.each(["codex", "claude"] as const)("%s preserves a conflicting provider draft and retries only with a fresh version", async (client) => {
    localStorage.setItem("lich13-switch.main.client", client);
    const user = userEvent.setup();
    let current = client === "claude" ? claudeState : state;
    current.running = true;
    current.mode = "auto";
    mock.command.mockImplementation(async (name, args) => {
      if (name === "get_gateway") return structuredClone(current);
      if (name === "update_gateway") {
        if (args.expectedRevision !== current.revision) throw { code: "CONFLICT", message: "网关设置已变化，请使用最新状态重试" };
        return structuredClone(current);
      }
    });
    render(<Gateway notify={() => {}} />);
    // Explicit UI selection also covers independent client preferences.
    await screen.findByRole("button", { name: "添加" });
    await user.click(screen.getByRole("button", { name: "添加" }));
    const address = screen.getByLabelText(client === "codex" ? "base_url" : "ANTHROPIC_BASE_URL");
    const key = screen.getByLabelText(client === "codex" ? "experimental_bearer_token" : "ANTHROPIC_AUTH_TOKEN");
    await user.type(address, "https://new.invalid/v1");
    await user.type(key, "fixture-key");
    const original = current.revision;
    // Runtime events keep the same semantic version and cannot erase input.
    act(() => mock.listeners.get("gateway-state")!({ ...current, lastSuccessful: "backup", waitingRequests: 2 }));
    expect(key).toHaveValue("fixture-key");
    current = { ...current, revision: "external-edit" };
    act(() => mock.listeners.get("gateway-state")!(current));
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
    expect(mock.command).toHaveBeenCalledWith("update_gateway", expect.objectContaining({ expectedRevision: original }));
    expect(key).toHaveValue("fixture-key");
    const writes = () => mock.command.mock.calls.filter(([name]) => name === "update_gateway");
    expect(writes()).toHaveLength(1);
    // The authoritative version can change again without delivering a frontend event.
    current = { ...current, revision: "latest-on-retry" };
    await user.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "添加供应商" })).not.toBeInTheDocument());
    expect(writes()).toHaveLength(2);
    expect(writes()[1][1]).toMatchObject({ clientId: client, expectedRevision: "latest-on-retry", edit: { op: "saveProvider", token: "fixture-key", baseUrl: "https://new.invalid/v1" } });
  });

  it("retries advanced settings without losing typed parameters", async () => {
    const user = userEvent.setup();
    let current = structuredClone(state);
    mock.command.mockImplementation(async (name, args) => {
      if (name === "get_gateway") return structuredClone(current);
      if (name === "update_gateway") {
        if (args.expectedRevision !== current.revision) throw { code: "CONFLICT", message: "设置已变化" };
        current = { ...current, settings: args.edit.settings, revision: "saved" };
        return structuredClone(current);
      }
    });
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    await user.click(screen.getByText("高级设置"));
    const input = screen.getByLabelText("容量错误等待 / 秒");
    await user.clear(input);
    await user.type(input, "120");
    current = { ...current, revision: "external" };
    act(() => mock.listeners.get("gateway-state")!(current));
    await user.click(screen.getByRole("button", { name: "保存参数" }));
    expect((await screen.findAllByRole("alert")).some(e => e.textContent?.includes("设置已变化"))).toBe(true);
    expect(input).toHaveValue(120);
    await user.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() => expect(mock.command).toHaveBeenCalledWith("update_gateway", expect.objectContaining({ expectedRevision: "external", edit: { op: "settings", settings: expect.objectContaining({ capacityRetrySeconds: 120 }) } })));
  });
  it("resets a provider from the row and edits its name by double click", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    const provider = await screen.findByRole("button", {
      name: `${state.providers[0].name} 名称`,
    });
    expect(screen.queryByLabelText("网关操作")).not.toBeInTheDocument();
    expect(screen.queryByText("刷新全部额度")).not.toBeInTheDocument();
    await user.dblClick(provider);
    const input = screen.getByRole("textbox", {
      name: `${state.providers[0].name} 名称`,
    });
    await user.clear(input);
    await user.type(input, "新的供应商");
    await user.keyboard("{Enter}");
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("update_gateway", {
        clientId: "codex",
        edit: {
          op: "renameProvider",
          id: state.providers[0].id,
          name: "新的供应商",
        },
        expectedRevision: state.revision,
      }),
    );
    await user.click(
      screen.getByRole("button", {
        name: `${state.providers[0].name} 重置熔断`,
      }),
    );
    expect(mock.command).toHaveBeenCalledWith("update_gateway", {
      clientId: "codex",
      edit: { op: "reset", id: state.providers[0].id },
      expectedRevision: state.revision,
    });
    expect(screen.queryByText("测试连接")).not.toBeInTheDocument();
  });

  it("keeps the provider quota beside the name in the main panel", async () => {
    const p = state.providers[0];
    p.quota = {
      providerId: p.id,
      version: p.quotaVersion,
      state: "ok",
      source: "sub2api",
      checkedAt: 1,
      successAt: 1,
      retryAt: null,
      stale: false,
      error: null,
      keyStatus: null,
      plans: [
        {
          name: "API Key 配额",
          remaining: 8,
          used: 2,
          total: 10,
          unit: "USD",
          unlimited: false,
          resetAt: null,
        },
      ],
      subscriptions: [
        {
          name: "Pro",
          status: "active",
          expiresAt: null,
          active: true,
          plans: [],
        },
      ],
      expiresAt: null,
      expiresAtUnix: null,
      today: null,
      totalUsage: null,
    } as ProviderQuota;
    render(<Gateway notify={() => {}} />);
    const name = await screen.findByRole("button", {
      name: `${p.name} 名称`,
    });
    const row = name.closest("[data-provider-id]");
    expect(row).not.toBeNull();
    const quota = within(row as HTMLElement).getByLabelText(`${p.name} 额度`);
    expect(name.compareDocumentPosition(quota) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    expect(within(quota).getByText("Pro")).toHaveClass("quota-subscription");
  });

  it("consumes explicit provider focus after opening the requested editor", async () => {
    const handled = vi.fn();
    const providerId = state.providers[0].id;
    const { rerender } = render(
      <Gateway
        notify={() => {}}
        focusProvider={{ id: providerId, sequence: 9 }}
        onFocusHandled={handled}
      />,
    );
    expect(await screen.findByRole("dialog", { name: `${state.providers[0].name} · 白名单` })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
    expect(handled).toHaveBeenCalledTimes(1);
    rerender(
      <Gateway notify={() => {}} focusProvider={null} onFocusHandled={handled} />,
    );
    expect(screen.getByRole("dialog", { name: `${state.providers[0].name} · 白名单` })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
    expect(handled).toHaveBeenCalledTimes(1);
  });

  it("starts a fresh provider policy editor after explicit navigation", async () => {
    const providerId = state.providers[0].id;
    const { rerender } = render(
      <Gateway
        notify={() => {}}
        focusProvider={{ id: providerId, sequence: 1 }}
      />,
    );
    expect(await screen.findByRole("dialog", { name: `${state.providers[0].name} · 白名单` })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
    // App confirms discarding before emitting this new navigation sequence.
    rerender(
      <Gateway
        notify={() => {}}
        focusProvider={{ id: providerId, sequence: 2 }}
      />,
    );
    expect(await screen.findByRole("dialog", { name: `${state.providers[0].name} · 白名单` })).toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
    expect(
      mock.command.mock.calls.some(([name]) => name === "update_gateway"),
    ).toBe(false);
  });

  it("waits for the requested client before consuming connection-test navigation", async () => {
    const handled = vi.fn();
    const { rerender } = render(<Gateway notify={() => {}} onFocusHandled={handled} />);
    await screen.findByRole("button", { name: `${state.providers[0].name} 名称` });
    const target = claudeState.providers[0];
    rerender(<Gateway notify={() => {}} focusProvider={{ id: target.id, clientId: "claude", action: "testing", sequence: 3 }} onFocusHandled={handled} />);
    const dialog = await screen.findByRole("dialog", { name: `${target.name} · 测试` });
    expect(within(dialog).getByRole("tab", { name: "连接测试" })).toHaveAttribute("aria-selected", "true");
    expect(handled).toHaveBeenCalledTimes(1);
    expect(mock.command).toHaveBeenCalledWith("provider_test_state", { clientId: "claude", providerId: target.id });
    expect(mock.command.mock.calls.some(([name]) => name === "provider_test_start")).toBe(false);
  });

  it("accepts an optional display name without adding model settings", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "添加" }));
    await user.type(screen.getByLabelText("名称"), "主力供应商");
    await user.type(
      screen.getByLabelText("base_url"),
      "https://new.example.com/sub/v1",
    );
    const token = screen.getByLabelText("experimental_bearer_token");
    expect(token).toHaveAttribute("type", "password");
    await user.type(token, "fixture-token");
    act(() =>
      mock.listeners.get("gateway-state")!({
        ...state,
        revision: "changed-while-editing",
      }),
    );
    await user.click(screen.getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("update_gateway", {
        clientId: "codex",
        edit: {
          op: "saveProvider",
          id: null,
          name: "主力供应商",
          baseUrl: "https://new.example.com/sub/v1",
          token: "fixture-token",
        },
        expectedRevision: state.revision,
      }),
    );
  });
  it("keeps a failed provider draft open with its error", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await user.click(await screen.findByRole("button", { name: "添加" }));
    await user.type(
      screen.getByLabelText("base_url"),
      "http://127.0.0.1:15722/v1",
    );
    await user.type(
      screen.getByLabelText("experimental_bearer_token"),
      "fixture",
    );
    mock.command.mockRejectedValueOnce({
      code: "LOOP",
      message: "上游不能指向本网关",
    });
    await user.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "上游不能指向本网关",
    );
    expect(screen.getByLabelText("base_url")).toHaveValue(
      "http://127.0.0.1:15722/v1",
    );
  });
  it("switches a stopped provider with the visible config revision and restart feedback", async () => {
    const notify = vi.fn();
    const user = userEvent.setup();
    render(<Gateway notify={notify} />);
    await user.click(await screen.findByRole("button", { name: "选择" }));
    expect(mock.command).toHaveBeenCalledWith("update_gateway", {
      clientId: "codex",
      edit: { op: "select", id: "backup" },
      expectedRevision: state.revision,
      expectedConfigRevision: state.configRevision,
    });
    expect(notify).toHaveBeenCalledWith("文件已切换，请重新打开 Codex");
  });
});

it("keeps model choices and manual entries when discovery fails and events refresh", async () => {
  const user = userEvent.setup();
  let discoveryFailed = false;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "get_gateway") return structuredClone(state);
    if (name === "list_provider_models") {
      if (discoveryFailed)
        throw { code: "NETWORK", message: "模型列表读取失败" };
      return { models: ["gpt-A", "gpt-B"], error: null, stale: false };
    }
    if (name === "update_gateway")
      throw { code: "CONFLICT", message: "设置已变化" };
  });
  render(<Gateway notify={() => {}} />);
  const operations = await screen.findByLabelText("api.example.com 操作");
  await user.click(operations);
  await user.click(
    within(operations.closest("details")!).getByRole("button", {
      name: "白名单",
    }),
  );
  await user.click(await screen.findByRole("checkbox", { name: "gpt-A" }));
  await user.type(
    screen.getByRole("textbox", { name: "手动添加模型 ID" }),
    "custom-model",
  );
  await user.click(screen.getByRole("button", { name: "添加到白名单" }));
  act(() =>
    mock.listeners.get("gateway-state")!({
      ...state,
      revision: "changed",
      activeConnections: 2,
    }),
  );
  expect(screen.getByRole("checkbox", { name: "gpt-A" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "custom-model" })).toBeChecked();
  discoveryFailed = true;
  await user.click(screen.getByRole("button", { name: "刷新模型列表" }));
  const dialog = screen.getByRole("dialog", { name: `${state.providers[0].name} · 白名单` });
  expect(await within(dialog).findByRole("status")).toHaveTextContent(
    "模型列表读取失败",
  );
  expect(screen.getByRole("checkbox", { name: "gpt-A" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "custom-model" })).toBeChecked();
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByText("设置已变化")).toBeInTheDocument();
  expect(screen.getByRole("checkbox", { name: "custom-model" })).toBeChecked();
  expect(mock.command).toHaveBeenCalledWith("update_gateway", {
    clientId: "codex",
    edit: {
      op: "policyProvider",
      id: "primary",
      allowedModels: ["gpt-A", "custom-model"],
    },
    expectedRevision: state.revision,
  });
});

it("refreshes authoritative configuration after a cap conflict, preserves input and permits explicit retry", async () => {
  const user = userEvent.setup();
  let attempts = 0;
  mock.command.mockImplementation(
    async (
      name: string,
      args?: { edit: { maxConcurrency: number }; expectedRevision: string },
    ) => {
      if (name === "get_gateway") return structuredClone(state);
      if (name === "update_gateway") {
        if (++attempts === 1) {
          state.revision = "other-window";
          state.providers[0].queued = false;
          throw { code: "CONFLICT", message: "网关设置已变化" };
        }
        expect(args!.expectedRevision).toBe("other-window");
        state.providers[0].maxConcurrency = args!.edit.maxConcurrency;
        state.revision = "saved";
        return structuredClone(state);
      }
    },
  );
  render(<Gateway notify={() => {}} />);
  await user.click(
    await screen.findByRole("button", { name: "api.example.com 并发上限" }),
  );
  const input = screen.getByRole("spinbutton", { name: "上限（0 不限）" });
  await user.clear(input);
  await user.type(input, "12");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
  expect(input).toHaveValue(12);
  expect(
    screen.getByRole("button", { name: "api.example.com 加入队列" }),
  ).toHaveAttribute("aria-pressed", "false");
  await user.click(screen.getByRole("button", { name: "保存" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  expect(
    screen.getByRole("button", { name: "api.example.com 并发上限" }),
  ).toHaveTextContent("并发 0/12");
  expect(
    mock.command.mock.calls
      .filter(([name]) => name === "update_gateway")
      .map(([, args]) => args.edit.op),
  ).toEqual(["concurrencyProvider", "concurrencyProvider"]);
  expect(state.providers[0].queued).toBe(false);
});

describe("client isolation", () => {
  it("edits bounded capacity waiting only for Codex and preserves the draft during events", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByLabelText(`${state.providers[0].name} 操作`);
    await user.click(screen.getByText("高级设置", { selector: "summary" }));
    const input = screen.getByRole("spinbutton", { name: "容量错误等待 / 秒" });
    expect(input).toHaveValue(60);
    expect(input).toHaveAttribute("min", "1");
    expect(input).toHaveAttribute("max", "86400");
    await user.clear(input);
    await user.type(input, "120");
    act(() =>
      mock.listeners.get("gateway-state")!({ ...state, activeConnections: 3 }),
    );
    expect(input).toHaveValue(120);
    await user.click(screen.getByRole("button", { name: "保存参数" }));
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("update_gateway", {
        clientId: "codex",
        edit: {
          op: "settings",
          settings: { ...state.settings, capacityRetrySeconds: 120 },
        },
        expectedRevision: state.revision,
      }),
    );
    await user.click(screen.getByRole("button", { name: "Claude Code" }));
    await screen.findByLabelText(`${claudeState.providers[0].name} 操作`);
    await user.click(screen.getByText("高级设置", { selector: "summary" }));
    expect(
      screen.queryByRole("spinbutton", { name: "容量错误等待 / 秒" }),
    ).not.toBeInTheDocument();
  });
  it("queries each selected client and ignores the other client's gateway events", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByLabelText(`${state.providers[0].name} 操作`);
    expect(mock.command).toHaveBeenCalledWith("get_gateway", {
      clientId: "codex",
    });
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("query_provider_quota", {
        clientId: "codex",
        providerId: state.providers[0].id,
        force: false,
      }),
    );
    act(() =>
      mock.listeners.get("gateway-state")!({
        ...claudeState,
        revision: "background-claude",
      }),
    );
    expect(
      screen.getByLabelText(`${state.providers[0].name} 操作`),
    ).toBeInTheDocument();
    expect(
      screen.queryByLabelText(`${claudeState.providers[0].name} 操作`),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Claude Code" }));
    await screen.findByLabelText(`${claudeState.providers[0].name} 操作`);
    expect(
      screen.queryByLabelText(`${state.providers[0].name} 操作`),
    ).not.toBeInTheDocument();
    expect(mock.command).toHaveBeenCalledWith("get_gateway", {
      clientId: "claude",
    });
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("query_provider_quota", {
        clientId: "claude",
        providerId: claudeState.providers[0].id,
        force: false,
      }),
    );
    expect(localStorage.getItem("lich13-switch.main.client")).toBe("claude");
    act(() =>
      mock.listeners.get("gateway-state")!({
        ...state,
        revision: "background-codex",
        providers: state.providers.map((p) => ({
          ...p,
          name: `后台 ${p.name}`,
        })),
      }),
    );
    expect(
      screen.getByLabelText(`${claudeState.providers[0].name} 操作`),
    ).toBeInTheDocument();
    expect(
      screen.queryByLabelText(`后台 ${state.providers[0].name} 操作`),
    ).not.toBeInTheDocument();
    act(() =>
      mock.listeners.get("gateway-state")!({
        ...claudeState,
        revision: "foreground-claude",
        providers: claudeState.providers.map((p, index) => ({
          ...p,
          name: index ? p.name : "当前 Claude 供应商",
        })),
      }),
    );
    expect(
      screen.getByLabelText("当前 Claude 供应商 操作"),
    ).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Codex" }));
    await screen.findByLabelText(`${state.providers[0].name} 操作`);
    expect(
      mock.command.mock.calls
        .filter(([name]) => name === "get_gateway")
        .map(([, args]) => args.clientId),
    ).toEqual(["codex", "claude", "codex"]);
    expect(
      mock.command.mock.calls.some(([name]) => name === "update_gateway"),
    ).toBe(false);
  });

  it("keeps an unsaved settings draft when switching is refused and discards it only after confirmation", async () => {
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByLabelText(`${state.providers[0].name} 操作`);
    await user.click(screen.getByText("高级设置", { selector: "summary" }));
    const port = screen.getByRole("spinbutton", { name: "共享本地端口" });
    await user.clear(port);
    await user.type(port, "23456");
    await user.click(screen.getByRole("button", { name: "Claude Code" }));
    await user.click(
      within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
        "button",
        { name: "取消" },
      ),
    );
    expect(port).toHaveValue(23456);
    expect(screen.getByRole("button", { name: "Codex" })).toHaveAttribute(
      "aria-pressed",
      "true",
    );
    expect(mock.command).not.toHaveBeenCalledWith("get_gateway", {
      clientId: "claude",
    });

    await user.click(screen.getByRole("button", { name: "Claude Code" }));
    await user.click(
      within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
        "button",
        { name: "放弃修改" },
      ),
    );
    await screen.findByLabelText(`${claudeState.providers[0].name} 操作`);
    await user.click(screen.getByText("高级设置", { selector: "summary" }));
    expect(screen.getByRole("spinbutton", { name: "共享本地端口" })).toHaveValue(
      claudeState.settings.port,
    );
    expect(
      mock.command.mock.calls.some(([name]) => name === "update_gateway"),
    ).toBe(false);
    await user.click(screen.getByRole("button", { name: "Codex" }));
    await screen.findByLabelText(`${state.providers[0].name} 操作`);
    expect(
      screen.queryByRole("dialog", { name: "确认操作" }),
    ).not.toBeInTheDocument();
    await user.click(screen.getByText("高级设置", { selector: "summary" }));
    expect(screen.getByRole("spinbutton", { name: "共享本地端口" })).toHaveValue(
      state.settings.port,
    );
  });

  it("restores Claude selection and saves only its two credential fields with the Claude revision", async () => {
    localStorage.setItem("lich13-switch.main.client", "claude");
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByLabelText(`${claudeState.providers[0].name} 操作`);
    expect(mock.command).not.toHaveBeenCalledWith("get_gateway", {
      clientId: "codex",
    });
    await user.click(screen.getByRole("button", { name: "添加" }));
    const dialog = screen.getByRole("dialog", { name: "添加供应商" });
    expect(dialog.querySelectorAll("input")).toHaveLength(4);
    const base = within(dialog).getByLabelText("ANTHROPIC_BASE_URL");
    const token = within(dialog).getByLabelText("ANTHROPIC_AUTH_TOKEN");
    expect(token).toHaveAttribute("type", "password");
    expect(
      within(dialog).queryByLabelText("experimental_bearer_token"),
    ).not.toBeInTheDocument();
    await user.type(base, "https://claude.fixture.invalid/deployment");
    await user.type(token, "claude-form-fixture");
    act(() =>
      mock.listeners.get("gateway-state")!({
        ...state,
        revision: "unrelated-codex-update",
      }),
    );
    expect(base).toHaveValue("https://claude.fixture.invalid/deployment");
    expect(token).toHaveValue("claude-form-fixture");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
    expect(mock.command).toHaveBeenCalledWith("update_gateway", {
      clientId: "claude",
      edit: {
        op: "saveProvider",
        id: null,
        name: "",
        baseUrl: "https://claude.fixture.invalid/deployment",
        token: "claude-form-fixture",
      },
      expectedRevision: claudeState.revision,
    });
  });
});

it("keeps one icon-bearing testing entry per provider and removes global testing", async () => {
  const user = userEvent.setup();
  render(<Gateway notify={() => {}} />);
  await screen.findByRole("button", { name: "添加" });
  expect(screen.queryByRole("button", { name: "测试与检测" })).not.toBeInTheDocument();
  const operations = screen.getByLabelText(`${state.providers[0].name} 操作`);
  await user.click(operations);
  const menu = within(operations.closest("details")!);
  const entry = menu.getByRole("button", { name: "连接测试" });
  expect(entry.querySelector("svg")).not.toBeNull();
  expect(menu.queryByRole("button", { name: "模型指纹测试" })).not.toBeInTheDocument();
});

it.each(["codex", "claude", "grok", "dsh"] as const)("%s keeps connection values and client onboarding without metadata or file exchange", async (clientId) => {
  localStorage.setItem("lich13-switch.main.client", clientId);
  state.clientId = clientId;
  state.running = false;
  state.grokManaged = false;
  state.current = {
    kind: "gateway", modelSource: null, warning: null, model: "fixture-current-model", address: "http://127.0.0.1:43721/v1",
    protocol: "chat_completions", addressSource: "fixture-source", credentialSource: "fixture-credential-source",
  };
  state.dshProfiles = clientId === "dsh" ? [{ profile: "fixture", current: { ...state.current, model: "fixture-profile-model" } }] : [];
  mock.command.mockImplementation(async (name) => name === "get_gateway" ? structuredClone(state) : null);
  const { container } = render(<Gateway notify={() => {}} quotaRefreshSeconds={0} />);
  await screen.findByText("fixture-current-model");
  const current = container.querySelector(".gateway-current")!;
  expect(current).toHaveTextContent("本地网关");
  expect(current).toHaveTextContent("http://127.0.0.1:43721/v1");
  expect(current.querySelector("small")).toBeNull();
  expect(current).not.toHaveTextContent("fixture-source");
  expect(current).not.toHaveTextContent("fixture-credential-source");
  expect(current).not.toHaveTextContent("chat_completions");
  if (clientId === "dsh") expect(current).toHaveTextContent("fixture-profile-model");
  if (clientId === "grok" || clientId === "dsh") expect(screen.getByRole("button", { name: "导入并接入" })).toBeInTheDocument();
  expect(screen.queryByRole("group", { name: "供应商导入导出" })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: /^(导入|导出)$/ })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "添加" })).toBeInTheDocument();
});

it("keeps a gateway event that arrives before the initial read", async () => {
  let resolve!: (value: GatewayState) => void;
  mock.command.mockImplementation((name) => name === "get_gateway" ? new Promise<GatewayState>((done) => { resolve = done; }) : Promise.resolve(null));
  render(<Gateway notify={() => {}} quotaRefreshSeconds={0} />);
  const latest = { ...state, current: { kind: "gateway" as const, modelSource: null, warning: null, model: "fixture-latest-model" } };
  await act(async () => { mock.listeners.get("gateway-state")!(latest); });
  await screen.findByText("fixture-latest-model");
  await act(async () => { resolve({ ...state, current: { kind: "gateway", modelSource: null, warning: null, model: "fixture-stale-model" } }); });
  expect(screen.getByText("fixture-latest-model")).toBeInTheDocument();
  expect(screen.queryByText("fixture-stale-model")).not.toBeInTheDocument();
});
