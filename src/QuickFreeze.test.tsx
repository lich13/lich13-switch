import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { GatewayFreeze } from "./bridge-contracts";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (v: unknown) => void>(),
  registration: null as Promise<void> | null,
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: async (name: string, fn: (v: unknown) => void) => {
    mock.listeners.set(name, fn);
    if (name === "gateway-freeze") await mock.registration;
    return () => mock.listeners.delete(name);
  },
}));
import QuickFreeze from "./QuickFreeze";
const idle: GatewayFreeze = {
  frozen: false,
  draining: 0,
  receiving: 0,
  waiting: 0,
  headers: 0,
  streaming: 0,
  finishing: 0,
  queued: 0,
};
beforeEach(() => {
  mock.command.mockReset().mockResolvedValue(idle);
  mock.listeners.clear();
  mock.registration = null;
});
afterEach(() => vi.restoreAllMocks());

it("reads the initial status only after change subscription is ready", async () => {
  let ready!: () => void;
  mock.registration = new Promise<void>((resolve) => {
    ready = resolve;
  });
  render(<QuickFreeze />);
  expect(mock.command).not.toHaveBeenCalled();
  expect(screen.getByText("读取状态")).toBeInTheDocument();
  mock.command.mockResolvedValue({ ...idle, frozen: true });
  await act(async () => ready());
  expect(await screen.findByText("可休眠")).toBeInTheDocument();
  expect(mock.command).toHaveBeenCalledExactlyOnceWith("get_gateway_freeze");
});

it("freezes globally, disables controls while draining, and thaws when safe", async () => {
  const user = userEvent.setup();
  render(<QuickFreeze />);
  await screen.findByText("未冻结");
  mock.command.mockResolvedValue({
    ...idle,
    frozen: true,
    draining: 1,
    streaming: 1,
    queued: 3,
  });
  await user.click(screen.getByRole("button", { name: "全局冻结" }));
  expect(mock.command).toHaveBeenLastCalledWith("set_gateway_frozen", {
    frozen: true,
  });
  expect(await screen.findByText("等待收尾")).toBeInTheDocument();
  const thaw = screen.getByRole("button", { name: "解冻" });
  expect(thaw).toBeDisabled();
  mock.command.mockClear();
  await user.click(thaw);
  expect(mock.command).not.toHaveBeenCalled();
  act(() =>
    mock.listeners.get("gateway-freeze")?.({
      ...idle,
      frozen: true,
      queued: 3,
    }),
  );
  expect(screen.getByText("可休眠")).toBeInTheDocument();
  expect(thaw).toBeEnabled();
  mock.command.mockResolvedValue(idle);
  await user.click(thaw);
  expect(mock.command).toHaveBeenLastCalledWith("set_gateway_frozen", {
    frozen: false,
  });
  expect(await screen.findByText("未冻结")).toBeInTheDocument();
});

it("keeps newer events when initial or command responses arrive late", async () => {
  let resolve!: (state: GatewayFreeze) => void;
  mock.command.mockImplementation(
    () =>
      new Promise<GatewayFreeze>((done) => {
        resolve = done;
      }),
  );
  const user = userEvent.setup();
  const view = render(<QuickFreeze />);
  await waitFor(() => expect(mock.listeners.has("gateway-freeze")).toBe(true));
  act(() => mock.listeners.get("gateway-freeze")?.({ ...idle, frozen: true }));
  await act(async () => resolve(idle));
  expect(screen.getByText("可休眠")).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "解冻" }));
  act(() => mock.listeners.get("gateway-freeze")?.(idle));
  await act(async () => resolve({ ...idle, frozen: true }));
  expect(screen.getByText("未冻结")).toBeInTheDocument();
  view.unmount();
  expect(mock.listeners.size).toBe(0);
});

it("retries failed reads and refreshes status when the panel reopens", async () => {
  mock.command.mockRejectedValueOnce(new Error("fixture failure"));
  const user = userEvent.setup();
  render(<QuickFreeze />);
  await screen.findByRole("alert");
  expect(screen.getByText("状态异常")).toBeInTheDocument();
  mock.command.mockResolvedValue({ ...idle, frozen: true });
  await user.click(screen.getByRole("button", { name: "重试" }));
  expect(await screen.findByText("可休眠")).toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  mock.command.mockResolvedValue(idle);
  act(() => mock.listeners.get("panel-visibility")?.(true));
  expect(await screen.findByText("未冻结")).toBeInTheDocument();
});

it("uses the same status in the main window and never replays ice after reopening", async () => {
  vi.spyOn(window, "matchMedia").mockReturnValue({
    ...window.matchMedia(""),
    matches: false,
  });
  const view = render(<QuickFreeze surface="main" />);
  act(() => mock.listeners.get("app-visibility")?.(true));
  await screen.findByText("未冻结");
  expect(view.container.querySelector(".quick-freeze-green")).not.toBeNull();
  act(() => mock.listeners.get("gateway-freeze")?.({ ...idle, frozen: true }));
  expect(view.container.querySelector(".freeze-effect-freeze")).not.toBeNull();
  expect(view.container.querySelector(".quick-freeze-ice")).not.toBeNull();
  act(() => mock.listeners.get("app-visibility")?.(false));
  expect(view.container.querySelector(".freeze-effect-freeze")).toBeNull();
  mock.command.mockResolvedValue({ ...idle, frozen: true });
  await act(async () => mock.listeners.get("app-visibility")?.(true));
  expect(view.container.querySelector(".freeze-effect-still")).not.toBeNull();
  act(() => mock.listeners.get("gateway-freeze")?.(idle));
  expect(view.container.querySelector(".freeze-effect-thaw")).not.toBeNull();
  expect(view.container.querySelector(".freeze-pulse")).not.toBeNull();
  expect(view.container.querySelectorAll(".freeze-shard-a")).toHaveLength(4);
});

it("completes dynamic thaw animation after 860ms and respects prefers-reduced-motion", async () => {
  vi.useFakeTimers();
  try {
    const media = {
      ...window.matchMedia(""),
      matches: false,
    };
    vi.spyOn(window, "matchMedia").mockImplementation(() => media);
    const view = render(<QuickFreeze />);
    await act(async () => {
      await Promise.resolve();
      await Promise.resolve();
    });
    expect(screen.getByText("未冻结")).toBeInTheDocument();

    act(() =>
      mock.listeners.get("gateway-freeze")?.({ ...idle, frozen: true }),
    );
    expect(
      view.container.querySelector(".freeze-effect-freeze"),
    ).not.toBeNull();
    act(() => vi.advanceTimersByTime(1500));
    expect(view.container.querySelector(".freeze-effect-still")).not.toBeNull();

    act(() => mock.listeners.get("gateway-freeze")?.(idle));
    expect(view.container.querySelector(".freeze-effect-thaw")).not.toBeNull();
    act(() => vi.advanceTimersByTime(859));
    expect(view.container.querySelector(".freeze-effect-thaw")).not.toBeNull();
    act(() => vi.advanceTimersByTime(1));
    expect(view.container.querySelector(".freeze-effect")).toBeNull();

    media.matches = true;
    act(() =>
      mock.listeners.get("gateway-freeze")?.({ ...idle, frozen: true }),
    );
    expect(view.container.querySelector(".freeze-effect-still")).not.toBeNull();
    act(() => mock.listeners.get("gateway-freeze")?.(idle));
    expect(view.container.querySelector(".freeze-effect")).toBeNull();
  } finally {
    vi.useRealTimers();
  }
});
