import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
import { mock } from "./test-bridge";

vi.mock("./bridge", async () => (await import("./test-bridge")).bridge);
import QuickControls from "./QuickControls";

const original = {
  supported: true,
  enabled: false,
  batterySleep: 7,
  helper: "ready",
  revision: "fixture-before",
};
const enabled = {
  ...original,
  enabled: true,
  batterySleep: 0,
  revision: "fixture-after",
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
});

it.each(["missing", "needsRepair"])(
  "%s helper keeps the switch unchanged while one toggle authorizes and repairs",
  async (helper) => {
    const pending = deferred<typeof enabled>();
    const notify = vi.fn();
    const error = vi.fn();
    mock.command.mockImplementation((name: string) => {
      if (name === "get_clamshell_state")
        return Promise.resolve({ ...original, helper });
      if (name === "set_clamshell_awake") return pending.promise;
      throw new Error(`Unexpected fixture command: ${name}`);
    });
    render(<QuickControls visible notify={notify} error={error} />);
    const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
    fireEvent.click(toggle);
    fireEvent.click(toggle);
    expect(toggle).toBeDisabled();
    expect(toggle).not.toBeChecked();
    expect(
      mock.command.mock.calls.filter(
        ([name]) => name === "set_clamshell_awake",
      ),
    ).toEqual([
      [
        "set_clamshell_awake",
        { enabled: true, expectedRevision: "fixture-before" },
      ],
    ]);
    expect(notify).not.toHaveBeenCalled();
    await act(async () => pending.resolve(enabled));
    expect(toggle).toBeChecked();
    expect(toggle).toBeEnabled();
    expect(notify).toHaveBeenCalledWith("已开启合盖不休眠");
    expect(error).not.toHaveBeenCalled();
    expect(mock.command.mock.calls.map(([name]) => name)).toEqual([
      "get_clamshell_state",
      "set_clamshell_awake",
    ]);
  },
);

it.each([
  ["POWER_AUTH", "已取消系统授权"],
  ["POWER_HELPER", "助手修复失败"],
  ["FIXTURE_UNKNOWN", "助手事务失败"],
])(
  "%s leaves the switch unchanged and reports the backend error once",
  async (code, message) => {
    const notify = vi.fn();
    const error = vi.fn();
    mock.command.mockImplementation((name: string) => {
      if (name === "get_clamshell_state")
        return Promise.resolve({ ...original, helper: "needsRepair" });
      if (name === "set_clamshell_awake")
        return Promise.reject({ code, message });
      throw new Error(`Unexpected fixture command: ${name}`);
    });
    render(<QuickControls visible notify={notify} error={error} />);
    const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
    fireEvent.click(toggle);
    expect(await screen.findByRole("alert")).toHaveTextContent(message);
    expect(error).not.toHaveBeenCalled();
    fireEvent.click(screen.getByRole("button", { name: "关闭电源告警" }));
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    await waitFor(() => expect(toggle).toBeEnabled());
    expect(toggle).not.toBeChecked();
    expect(notify).not.toHaveBeenCalled();
    expect(mock.command.mock.calls.map(([name]) => name)).toEqual([
      "get_clamshell_state",
      "set_clamshell_awake",
    ]);
  },
);

it("refreshes external changes after a repair conflict without replaying the toggle", async () => {
  const pending = deferred<typeof enabled>();
  const notify = vi.fn();
  const error = vi.fn();
  let current = original;
  mock.command.mockImplementation((name: string) => {
    if (name === "get_clamshell_state") return Promise.resolve(current);
    if (name === "set_clamshell_awake") return pending.promise;
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  render(<QuickControls visible notify={notify} error={error} />);
  const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
  fireEvent.click(toggle);
  current = {
    ...original,
    enabled: true,
    revision: "fixture-external",
  };
  await act(async () => {
    mock.listeners.get("clamshell-state")?.(current);
    pending.reject({
      code: "CONFLICT",
      message: "修复期间电源状态已变化，请重新操作",
    });
  });
  expect(toggle).toBeChecked();
  expect(toggle).toBeEnabled();
  expect(screen.getByRole("alert")).toHaveTextContent(
    "修复期间电源状态已变化，请重新操作",
  );
  expect(error).not.toHaveBeenCalled();
  expect(notify).not.toHaveBeenCalled();
  expect(
    mock.command.mock.calls.filter(([name]) => name === "set_clamshell_awake"),
  ).toHaveLength(1);
});

it("uses the last confirmed revision and helper result when disabling", async () => {
  const restored = {
    ...original,
    batterySleep: 9,
    revision: "fixture-restored",
  };
  const notify = vi.fn();
  mock.command.mockImplementation((name: string) => {
    if (name === "get_clamshell_state") return Promise.resolve(enabled);
    if (name === "set_clamshell_awake") return Promise.resolve(restored);
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  render(<QuickControls visible notify={notify} error={vi.fn()} />);
  const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
  expect(toggle).toBeChecked();
  fireEvent.click(toggle);
  await waitFor(() => expect(toggle).not.toBeChecked());
  expect(mock.command).toHaveBeenCalledWith("set_clamshell_awake", {
    enabled: false,
    expectedRevision: "fixture-after",
  });
  expect(notify).toHaveBeenCalledExactlyOnceWith("已恢复电池休眠设置");
});

it("quits only the chosen platform and blocks duplicate operations", async () => {
  const pending = deferred<{ terminated: number; failed: number }>();
  mock.command.mockImplementation((name: string) =>
    name === "get_clamshell_state"
      ? Promise.resolve(original)
      : pending.promise,
  );
  render(<QuickControls visible notify={vi.fn()} error={vi.fn()} />);
  const codex = screen.getByRole("button", { name: "退出 ChatGPT / Codex" });
  const claude = screen.getByRole("button", { name: "退出 Claude Code" });
  fireEvent.click(claude);
  expect(mock.command).toHaveBeenCalledWith("force_quit_agent_clients", {
    target: "claude",
  });
  expect(codex).toBeDisabled();
  expect(claude).toBeDisabled();
  await act(async () => pending.resolve({ terminated: 1, failed: 0 }));
  fireEvent.click(codex);
  expect(mock.command).toHaveBeenCalledWith("force_quit_agent_clients", {
    target: "codex",
  });
});

it("keeps power alerts separate, dismisses them, and clears them after explicit success", async () => {
  const error = vi.fn();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "get_clamshell_state") return enabled;
    throw { message: "Fixture quit error" };
  });
  render(<QuickControls visible notify={vi.fn()} error={error} />);
  await screen.findByRole("switch");
  fireEvent.click(screen.getByRole("button", { name: "退出 Claude Code" }));
  await waitFor(() => expect(error).toHaveBeenCalledWith("Fixture quit error"));
  act(() =>
    mock.listeners.get("clamshell-error")?.({
      code: "POWER_AUTH",
      message: "已取消系统授权",
    }),
  );
  expect(screen.getByRole("alert")).toHaveTextContent("已取消系统授权");
  fireEvent.click(screen.getByRole("button", { name: "关闭电源告警" }));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  act(() =>
    mock.listeners.get("clamshell-error")?.({
      code: "POWER_HELPER",
      message: "助手修复失败",
    }),
  );
  act(() => mock.listeners.get("clamshell-state")?.(original));
  expect(screen.getByRole("alert")).toHaveTextContent("助手修复失败");
  act(() => mock.listeners.get("clamshell-error")?.(null));
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(error).toHaveBeenCalledTimes(1);
});

it("keeps a failed enable visible across polling and reopening", async () => {
  vi.useFakeTimers();
  try {
    mock.command.mockImplementation(async (name: string) => {
      if (name === "get_clamshell_state") return original;
      throw { code: "POWER_AUTH", message: "已取消系统授权" };
    });
    const props = { notify: vi.fn(), error: vi.fn() };
    const view = render(<QuickControls {...props} visible />);
    await act(async () => {});
    await act(async () => {
      fireEvent.click(screen.getByRole("switch", { name: "合盖不休眠" }));
    });
    expect(screen.getByRole("alert")).toHaveTextContent("已取消系统授权");
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5000);
    });
    expect(screen.getByRole("alert")).toHaveTextContent("已取消系统授权");
    view.rerender(<QuickControls {...props} visible={false} />);
    view.rerender(<QuickControls {...props} visible />);
    await act(async () => {});
    expect(screen.getByRole("alert")).toHaveTextContent("已取消系统授权");
    view.unmount();
  } finally {
    vi.useRealTimers();
  }
});
