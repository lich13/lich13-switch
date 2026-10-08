import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));

import NotificationPermission from "./NotificationPermission";

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockResolvedValue({ permission: "granted", error: null });
});

describe("notification permission", () => {
  it("renders the actual granted or denied state and follows native events", async () => {
    const view = render(<NotificationPermission />);
    expect(await screen.findByRole("status")).toHaveTextContent("已允许");
    act(() =>
      mock.listeners.get("notification-state")?.({
        permission: "denied",
        error: null,
      }),
    );
    expect(screen.getByRole("status")).toHaveTextContent("系统已关闭");
    view.unmount();
    mock.command.mockResolvedValueOnce({ permission: "unknown", error: null });
    render(<NotificationPermission />);
    expect(await screen.findByRole("status")).toHaveTextContent("无法确认");
  });

  it("requests prompt permission and updates to granted without a second prompt", async () => {
    const user = userEvent.setup();
    mock.command
      .mockResolvedValueOnce({ permission: "prompt", error: null })
      .mockResolvedValueOnce({ permission: "granted", error: null });
    render(<NotificationPermission />);
    expect(await screen.findByRole("status")).toHaveTextContent("未授权");
    await user.click(screen.getByRole("button", { name: "授权" }));
    expect(mock.command).toHaveBeenLastCalledWith("notification_permission", {
      request: true,
    });
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("已允许"));
    expect(screen.queryByRole("button", { name: "授权" })).not.toBeInTheDocument();
  });

  it("shows native read and request failures as actionable status text", async () => {
    mock.command.mockRejectedValueOnce({
      code: "NOTIFICATION",
      message: "通知权限读取失败",
    });
    render(<NotificationPermission />);
    expect(await screen.findByRole("status")).toHaveTextContent("通知权限读取失败");

    mock.command.mockReset();
    mock.command.mockResolvedValueOnce({ permission: "prompt", error: null });
    render(<NotificationPermission />);
    await screen.findByRole("button", { name: "授权" });
    mock.command.mockRejectedValueOnce({
      code: "NOTIFICATION",
      message: "通知授权尚未完成",
    });
    await userEvent.click(screen.getAllByRole("button", { name: "授权" })[0]);
    expect(await screen.findAllByRole("status")).toContainEqual(
      expect.objectContaining({ textContent: "通知授权尚未完成" }),
    );
  });

  it("reports test delivery failure and acceptance, shows pending delivery, and opens system settings", async () => {
    const user = userEvent.setup();
    render(<NotificationPermission />);
    expect(await screen.findByRole("status")).toHaveTextContent("已允许");
    const send = screen.getByRole("button", { name: "发送测试通知" });

    mock.command.mockRejectedValueOnce({
      code: "NOTIFICATION",
      message: "测试通知发送失败",
    });
    await user.click(send);
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("测试通知发送失败"));

    mock.command.mockResolvedValueOnce({
      permission: "granted",
      delivery: "accepted",
      error: null,
    });
    await user.click(send);
    await waitFor(() => expect(screen.getByRole("status")).toHaveTextContent("系统已接受"));

    act(() =>
      mock.listeners.get("notification-state")?.({
        permission: "granted",
        delivery: "pending",
        error: null,
      }),
    );
    expect(screen.getByRole("status")).toHaveTextContent("等待发送");
    expect(send).toBeDisabled();

    await user.click(screen.getByRole("button", { name: "系统通知设置" }));
    expect(mock.command).toHaveBeenLastCalledWith("open_notification_settings", {});
  });
});
