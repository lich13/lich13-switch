import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (v: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  preview: true,
  command: mock.command,
  subscribe: async (name: string, fn: (v: unknown) => void) => {
    mock.listeners.set(name, fn);
    return () => mock.listeners.delete(name);
  },
}));
import QuickPanel from "./QuickPanel";
import { demo } from "./preview";
import { claudeGatewayDemo, gatewayDemo } from "./gateway-preview";
import type { ProviderQuota } from "./types";
beforeEach(() => {
  localStorage.clear();
  mock.command.mockReset();
  mock.listeners.clear();
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown>) => {
      if (name === "get_state" || name === "switch_account")
        return structuredClone(demo);
      if (name === "get_gateway" || name === "update_gateway")
        return structuredClone(
          args?.clientId === "claude" ? claudeGatewayDemo : gatewayDemo,
        );
      if (name === "get_quick")
        return { pinned: false, tab: "providers", visible: true };
      if (name === "set_quick")
        return { pinned: false, tab: "providers", ...args };
    },
  );
});
afterEach(() => localStorage.clear());
it("shows ChatGPT subscription labels in the quick account list and hides API-key plans", async () => {
  const user = userEvent.setup();
  const accounts = structuredClone(demo);
  accounts.accounts = [
    {
      ...accounts.accounts[0],
      subscription: { planType: "pro", label: "Pro", stale: false },
    },
    {
      ...accounts.accounts[2],
      subscription: {
        planType: "api-secret",
        label: "API Secret Plan",
        stale: false,
      },
    },
    {
      id: "unknown",
      name: "未知账号",
      kind: "chatgpt" as const,
      email: null,
      current: false,
      updatedAt: 0,
      subscription: { planType: null, label: "未识别", stale: true },
    },
  ];
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_state" || name === "switch_account")
        return structuredClone(accounts);
      if (name === "get_gateway" || name === "update_gateway")
        return structuredClone(
          args?.clientId === "claude" ? claudeGatewayDemo : gatewayDemo,
        );
      if (name === "get_quick")
        return { pinned: false, tab: "providers", visible: true };
      if (name === "set_quick")
        return { pinned: false, tab: "providers", ...args };
    },
  );
  render(<QuickPanel />);
  await user.click(await screen.findByRole("button", { name: "账号" }));

  const known = await screen.findByRole("button", { name: "切换到 日常开发" });
  expect(within(known).getByText("Pro")).toHaveClass("account-plan");
  const unknown = screen.getByRole("button", { name: "切换到 未知账号" });
  const unknownPlan = within(unknown).getByText("未识别");
  expect(unknownPlan).toHaveClass("account-plan");
  expect(unknownPlan).toHaveAttribute("title", expect.stringContaining("凭据需刷新"));
  const api = screen.getByRole("button", { name: "切换到 API 开发账号" });
  expect(within(api).queryByText("API Secret Plan")).not.toBeInTheDocument();
});

it("uses current revisions for quick actions without initializing the main window", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  await screen.findByText("api.example.com");
  act(() =>
    mock.listeners.get("gateway-state")!({
      ...gatewayDemo,
      running: true,
      revision: "current-route",
      waitingRequests: 2,
    }),
  );
  expect(screen.getByText("等待 2")).toBeInTheDocument();
  await user.click(
    screen.getByRole("button", { name: "选择 api.example.com" }),
  );
  expect(mock.command).toHaveBeenCalledWith("update_gateway", {
    clientId: "codex",
    edit: { op: "select", id: "primary" },
    expectedRevision: "current-route",
  });
  expect(mock.command).not.toHaveBeenCalledWith(
    "frontend_ready",
    expect.anything(),
  );
  await user.click(screen.getByRole("button", { name: "账号" }));
  await user.type(screen.getByRole("textbox", { name: "搜索账号" }), "工作");
  expect(
    screen.getByRole("button", { name: "切换到 工作空间" }),
  ).toBeInTheDocument();
  expect(screen.queryByText("日常开发")).not.toBeInTheDocument();
  act(() =>
    mock.listeners.get("switch-state")!({
      ...demo,
      authRevision: "external-auth",
    }),
  );
  await user.click(screen.getByRole("button", { name: "切换到 工作空间" }));
  expect(mock.command).toHaveBeenCalledWith("switch_account", {
    id: "work",
    expectedRevision: "external-auth",
  });
});

it("keeps the fixed tall panel without dynamic resize commands", async () => {
  render(<QuickPanel />);
  await screen.findByText("api.example.com");
  expect(mock.command.mock.calls.some(([name]) => name === "resize_quick")).toBe(
    false,
  );
});

it("keeps provider naming separate from selection and exposes reset in the quick panel", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  const name = await screen.findByRole("button", {
    name: `${gatewayDemo.providers[0].name} 名称`,
  });
  await user.dblClick(name);
  const input = screen.getByRole("textbox", {
    name: `${gatewayDemo.providers[0].name} 名称`,
  });
  await user.clear(input);
  await user.type(input, "快捷名称");
  await user.keyboard("{Enter}");
  expect(mock.command).toHaveBeenCalledWith("update_gateway", {
    clientId: "codex",
    edit: { op: "renameProvider", id: "primary", name: "快捷名称" },
    expectedRevision: gatewayDemo.revision,
  });
  await user.click(
    screen.getByRole("button", {
      name: `${gatewayDemo.providers[0].name} 重置熔断`,
    }),
  );
  expect(mock.command).toHaveBeenCalledWith("update_gateway", {
    clientId: "codex",
    edit: { op: "reset", id: "primary" },
    expectedRevision: gatewayDemo.revision,
  });
});
it("can restore a newer saved credential without auto-writing the auth file", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  await screen.findByText("api.example.com");
  await user.click(screen.getByRole("button", { name: "账号" }));
  act(() =>
    mock.listeners.get("switch-state")!({
      ...demo,
      authSync: {
        state: "older",
        accountId: "personal",
        message: "检测到旧凭据，已保留已保存版本",
        at: 1,
      },
    }),
  );
  expect(
    mock.command.mock.calls.some(([name]) => name === "switch_account"),
  ).toBe(false);
  await user.click(screen.getByRole("button", { name: "使用已保存版本" }));
  expect(mock.command).toHaveBeenCalledWith("switch_account", {
    id: "personal",
    expectedRevision: demo.authRevision,
  });
});
it("keeps provider settings HTTP/SSE-only and opens shared policy settings", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  await screen.findByText("api.example.com");
  expect(screen.queryByText("HTTP 桥接")).not.toBeInTheDocument();
  await user.click(
    screen.getByRole("button", { name: "api.example.com 白名单" }),
  );
  expect(mock.command).toHaveBeenCalledWith("open_main", {
    page: "gateway",
    clientId: "codex",
    providerId: "primary",
  });
  expect(
    mock.command.mock.calls.some(([name]) => name === "update_gateway"),
  ).toBe(false);
});

it("keeps the provider quota beside the name in the quick panel", async () => {
  const gateway = structuredClone(gatewayDemo);
  const p = gateway.providers[0];
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
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_state" || name === "switch_account")
        return structuredClone(demo);
      if (name === "get_gateway" || name === "update_gateway")
        return structuredClone(gateway);
      if (name === "get_quick")
        return { pinned: false, tab: "providers", visible: true };
      if (name === "set_quick")
        return { pinned: false, tab: "providers", ...args };
    },
  );
  render(<QuickPanel />);
  const name = await screen.findByRole("button", { name: `${p.name} 名称` });
  const row = name.closest("[data-provider-id]");
  expect(row).not.toBeNull();
  const quota = within(row as HTMLElement).getByLabelText(`${p.name} 额度`);
  expect(name.compareDocumentPosition(quota) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  expect(within(quota).getByText("Pro")).toHaveClass("quota-subscription");
});

it("quick concurrency editing keeps drafts during events and consumes Escape before the panel", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  await user.click(
    await screen.findByRole("button", { name: "api.example.com 并发上限" }),
  );
  const input = screen.getByRole("spinbutton");
  await user.clear(input);
  await user.type(input, "12");
  act(() =>
    mock.listeners.get("gateway-state")!({
      ...gatewayDemo,
      activeConnections: 2,
    }),
  );
  expect(input).toHaveValue(12);
  await user.keyboard("{Escape}");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(mock.command.mock.calls.some(([name]) => name === "hide_quick")).toBe(
    false,
  );
  await user.keyboard("{Escape}");
  expect(mock.command).toHaveBeenCalledWith("hide_quick");
  expect(
    mock.command.mock.calls.some(
      ([name]) => name === "update_gateway" || name === "switch_account",
    ),
  ).toBe(false);
});

it("switches and remembers the quick client independently and routes actions using only its events", async () => {
  localStorage.setItem("lich13-switch.main.client", "codex");
  const user = userEvent.setup();
  const view = render(<QuickPanel />);
  await screen.findByText(gatewayDemo.providers[0].name);
  const selector = screen.getByRole("navigation", { name: "客户端" });
  await user.click(
    within(selector).getByRole("button", { name: "Claude Code" }),
  );
  await screen.findByText(claudeGatewayDemo.providers[0].name);
  expect(mock.command).toHaveBeenCalledWith("get_gateway", {
    clientId: "claude",
  });
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("query_provider_quota", {
      clientId: "claude",
      providerId: claudeGatewayDemo.providers[0].id,
      force: false,
    }),
  );
  expect(localStorage.getItem("lich13-switch.quick.client")).toBe("claude");
  expect(localStorage.getItem("lich13-switch.main.client")).toBe("codex");
  act(() =>
    mock.listeners.get("gateway-state")!({
      ...gatewayDemo,
      revision: "background-codex",
      waitingRequests: 9,
    }),
  );
  expect(
    screen.getByText(claudeGatewayDemo.providers[0].name),
  ).toBeInTheDocument();
  expect(
    screen.queryByText(gatewayDemo.providers[0].name),
  ).not.toBeInTheDocument();
  expect(screen.queryByText("等待 9")).not.toBeInTheDocument();
  act(() =>
    mock.listeners.get("gateway-state")!({
      ...claudeGatewayDemo,
      running: true,
      revision: "current-claude",
      waitingRequests: 2,
    }),
  );
  expect(screen.getByText("等待 2")).toBeInTheDocument();
  await user.click(
    screen.getByRole("button", {
      name: `选择 ${claudeGatewayDemo.providers[0].name}`,
    }),
  );
  expect(mock.command).toHaveBeenCalledWith("update_gateway", {
    clientId: "claude",
    edit: { op: "select", id: claudeGatewayDemo.providers[0].id },
    expectedRevision: "current-claude",
  });
  view.unmount();
  render(<QuickPanel />);
  await screen.findByText(claudeGatewayDemo.providers[0].name);
  expect(screen.getByRole("button", { name: "Claude Code" })).toHaveAttribute(
    "aria-pressed",
    "true",
  );
  expect(localStorage.getItem("lich13-switch.main.client")).toBe("codex");
});

it("protects an unsaved quick concurrency draft until a client switch is confirmed", async () => {
  const user = userEvent.setup();
  render(<QuickPanel />);
  await user.click(
    await screen.findByRole("button", { name: "api.example.com 并发上限" }),
  );
  const cap = screen.getByRole("spinbutton", { name: "上限（0 不限）" });
  await user.clear(cap);
  await user.type(cap, "12");
  await user.click(
    within(screen.getByRole("navigation", { name: "客户端" })).getByRole(
      "button",
      { name: "Claude Code" },
    ),
  );
  await user.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "取消" },
    ),
  );
  expect(cap).toHaveValue(12);
  expect(
    screen.getByRole("dialog", { name: "api.example.com 并发上限" }),
  ).toBeInTheDocument();
  expect(mock.command).not.toHaveBeenCalledWith("get_gateway", {
    clientId: "claude",
  });
  await user.click(
    within(screen.getByRole("navigation", { name: "客户端" })).getByRole(
      "button",
      { name: "Claude Code" },
    ),
  );
  await user.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "放弃修改" },
    ),
  );
  await screen.findByText(claudeGatewayDemo.providers[0].name);
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(
    mock.command.mock.calls.some(([name]) => name === "update_gateway"),
  ).toBe(false);
});
