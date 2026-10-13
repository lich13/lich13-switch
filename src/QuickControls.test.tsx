import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  },
}));
import QuickControls from "./QuickControls";

const original = {
  supported: true,
  enabled: false,
  batterySleep: 7,
  helper: "ready",
  ownership: "none",
  revision: "fixture-before",
};
const enabled = {
  ...original,
  enabled: true,
  batterySleep: 0,
  ownership: "application",
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
      if (name === "get_clamshell_state") return Promise.resolve({ ...original, helper });
      if (name === "set_clamshell_awake") return pending.promise;
      throw new Error(`Unexpected fixture command: ${name}`);
    });
    render(<QuickControls visible notify={notify} error={error} />);
    const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
    fireEvent.click(toggle);
    fireEvent.click(toggle);
    expect(toggle).toBeDisabled();
    expect(toggle).not.toBeChecked();
    expect(mock.command.mock.calls.filter(([name]) => name === "set_clamshell_awake")).toEqual([
      ["set_clamshell_awake", { enabled: true, expectedRevision: "fixture-before" }],
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
])("%s leaves the switch unchanged and reports the backend error once", async (code, message) => {
  const notify = vi.fn();
  const error = vi.fn();
  mock.command.mockImplementation((name: string) => {
    if (name === "get_clamshell_state") return Promise.resolve({ ...original, helper: "needsRepair" });
    if (name === "set_clamshell_awake") return Promise.reject({ code, message });
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  render(<QuickControls visible notify={notify} error={error} />);
  const toggle = await screen.findByRole("switch", { name: "合盖不休眠" });
  fireEvent.click(toggle);
  await waitFor(() => expect(error).toHaveBeenCalledExactlyOnceWith(message));
  await waitFor(() => expect(toggle).toBeEnabled());
  expect(toggle).not.toBeChecked();
  expect(notify).not.toHaveBeenCalled();
  expect(mock.command.mock.calls.map(([name]) => name)).toEqual([
    "get_clamshell_state",
    "set_clamshell_awake",
    "get_clamshell_state",
  ]);
});

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
  current = { ...original, enabled: true, ownership: "external", revision: "fixture-external" };
  await act(async () => {
    mock.listeners.get("clamshell-state")?.({ ...current, externalChanged: true });
    pending.reject({ code: "CONFLICT", message: "修复期间电源状态已变化，请重新操作" });
  });
  expect(toggle).toBeChecked();
  expect(toggle).toBeEnabled();
  expect(error).toHaveBeenCalledExactlyOnceWith("修复期间电源状态已变化，请重新操作");
  expect(notify).not.toHaveBeenCalled();
  expect(mock.command.mock.calls.filter(([name]) => name === "set_clamshell_awake")).toHaveLength(1);
});

it("uses the last confirmed revision and helper result when disabling application ownership", async () => {
  const restored = { ...original, batterySleep: 9, revision: "fixture-restored" };
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
