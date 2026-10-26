import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { GatewayState } from "./types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: GatewayState) => void>(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(
    async (name: string, listener: (value: GatewayState) => void) => {
      mock.listeners.set(name, listener);
      return () => mock.listeners.delete(name);
    },
  ),
}));

import Gateway from "./Gateway";
import { gatewayDemo } from "./gateway-preview";

let state: GatewayState;

beforeEach(() => {
  localStorage.clear();
  state = structuredClone(gatewayDemo);
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "get_gateway") return structuredClone(state);
      if (name === "query_provider_quota") return undefined;
      if (name === "update_gateway") {
        const edit = args.edit as {
          op?: string;
          settings?: GatewayState["settings"];
        };
        if (edit.op === "settings" && edit.settings)
          state.settings = edit.settings;
        return structuredClone(state);
      }
      return undefined;
    },
  );
});

it("renders handoff waiting as neutral, non-clickable provider state", async () => {
  const [first, second] = state.providers;
  const third = {
    ...structuredClone(second),
    id: "fixture-third-receiver",
    name: "第三接收方",
    baseUrl: "https://third.fixture.invalid/v1",
  };
  state.providers.push(third);
  state.handoffWaiting = [
    { providerId: first.id, state: "awaiting_compaction" },
    { providerId: second.id, state: "awaiting_request" },
    { providerId: third.id, state: "capacity_full" },
  ];
  const user = userEvent.setup();
  render(<Gateway notify={() => {}} />);
  await screen.findByRole("button", { name: `${first.name} 名称` });

  const cases = [
    [first, "待压缩后接管"],
    [second, "待下一请求接管"],
    [third, "接管名额已满"],
  ] as const;
  for (const [provider, label] of cases) {
    const row = screen
      .getByRole("button", { name: `${provider.name} 名称` })
      .closest("[data-provider-id]");
    expect(row).not.toBeNull();
    const item = row as HTMLElement;
    const status = within(item).getByText(label);
    expect(status.tagName).toBe("SPAN");
    expect(
      within(item).queryByRole("button", { name: label }),
    ).not.toBeInTheDocument();
    await user.click(status);
  }
  expect(mock.command.mock.calls.some(([name]) => name === "open_main")).toBe(
    false,
  );
});

it("adds active requests and reserved threads in concurrency controls and exposes both counts", async () => {
  const provider = state.providers[0];
  provider.activeRequests = 2;
  provider.reservedThreads = 3;
  provider.maxConcurrency = 8;
  const user = userEvent.setup();
  render(<Gateway notify={() => {}} />);

  const control = await screen.findByRole("button", {
    name: `${provider.name} 并发上限`,
  });
  expect(control).toHaveTextContent("并发 5/8");
  await user.click(control);
  const popover = await screen.findByRole("dialog", {
    name: `${provider.name} 并发上限`,
  });
  const active = within(popover).getByText("正在请求");
  const reserved = within(popover).getByText("线程预留");
  expect(active.nextElementSibling).toHaveTextContent("2");
  expect(reserved.nextElementSibling).toHaveTextContent("3");
  expect(mock.command.mock.calls.some(([name]) => name === "open_main")).toBe(
    false,
  );
});

it("uses the waiting budget label and preserves explicitly saved timeout values", async () => {
  state.settings = {
    ...state.settings,
    firstByteSeconds: 600,
    idleSeconds: 600,
    retryBudgetSeconds: 37,
  };
  delete state.settings.threadIdleReleaseSeconds;
  const user = userEvent.setup();
  render(<Gateway notify={() => {}} />);
  await screen.findByRole("button", { name: "添加" });
  await user.click(screen.getByText("高级设置", { selector: "summary" }));
  await user.click(screen.getByText("故障策略", { selector: "summary" }));

  expect(
    screen.getByRole("spinbutton", { name: "线程空闲释放 / 秒" }),
  ).toHaveValue(300);
  expect(screen.getByRole("spinbutton", { name: "首字节 / 秒" })).toHaveValue(
    600,
  );
  expect(screen.getByRole("spinbutton", { name: "流静默 / 秒" })).toHaveValue(
    600,
  );
  expect(
    screen.getByRole("spinbutton", { name: "等待与重试预算 / 秒" }),
  ).toHaveValue(37);
  expect(
    screen.queryByRole("spinbutton", { name: "非流式总时限 / 秒" }),
  ).not.toBeInTheDocument();

  const idle = screen.getByRole("spinbutton", { name: "线程空闲释放 / 秒" });
  await user.clear(idle);
  await user.type(idle, "420");
  await user.click(screen.getByRole("button", { name: "保存参数" }));
  await vi.waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith(
      "update_gateway",
      expect.objectContaining({
        edit: expect.objectContaining({
          op: "settings",
          settings: expect.objectContaining({
            threadIdleReleaseSeconds: 420,
            firstByteSeconds: 600,
            idleSeconds: 600,
            retryBudgetSeconds: 37,
          }),
        }),
      }),
    ),
  );
});
