import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listener: (_s: unknown) => {},
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (_name: string, listener: (s: unknown) => void) => {
    mock.listener = listener;
    return () => {};
  }),
}));
import PowerSettings from "./PowerSettings";
const initial = {
  supported: true,
  enabled: false,
  batterySleep: 0,
  revision: "initial",
  helper: "notInstalled",
};
beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(async () => initial);
});
it("keeps state and controls after authorization is cancelled, then accepts helper events", async () => {
  render(<PowerSettings />);
  await screen.findByText("未安装");
  mock.command.mockRejectedValueOnce({
    code: "POWER_AUTH",
    message: "已取消系统授权",
  });
  fireEvent.click(screen.getByRole("button", { name: "安装" }));
  await screen.findByRole("alert");
  expect(screen.getByText("未安装")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "移除" })).toBeDisabled();
  mock.command.mockResolvedValueOnce({ ...initial, helper: "ready" });
  fireEvent.click(screen.getByRole("button", { name: "安装" }));
  await screen.findByText("已就绪");
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  act(() => mock.listener({ ...initial, helper: "needsRepair" }));
  expect(screen.getByText("需要修复")).toBeInTheDocument();
  mock.command.mockRejectedValueOnce({
    code: "CONFLICT",
    message: "电源状态已被外部修改",
  });
  mock.command.mockResolvedValueOnce({ ...initial, helper: "needsRepair" });
  fireEvent.click(screen.getByRole("button", { name: "移除" }));
  await screen.findByText("电源状态已被外部修改");
  expect(screen.getByText("需要修复")).toBeInTheDocument();
});
it("refreshes actual helper status after a failed installation", async () => {
  render(<PowerSettings />);
  await screen.findByText("未安装");
  mock.command.mockRejectedValueOnce({
    code: "POWER_INSTALL_REGISTER",
    message: "服务注册失败；原注册状态恢复失败，请重新修复助手",
  });
  mock.command.mockResolvedValueOnce({ ...initial, helper: "needsRepair" });
  fireEvent.click(screen.getByRole("button", { name: "安装" }));
  await screen.findByText("需要修复");
  expect(screen.getByRole("alert")).toHaveTextContent("服务注册失败");
  expect(screen.getByRole("button", { name: "修复" })).toBeEnabled();
});
it("disables management for an isolated native app and hides unsupported devices", async () => {
  mock.command.mockResolvedValueOnce({ ...initial, helper: "isolated" });
  const view = render(<PowerSettings />);
  await screen.findByText("隔离运行");
  expect(screen.getByRole("button", { name: "修复" })).toBeDisabled();
  view.unmount();
  mock.command.mockResolvedValueOnce({ ...initial, supported: false });
  render(<PowerSettings />);
  await waitFor(() =>
    expect(screen.queryByText("电源助手")).not.toBeInTheDocument(),
  );
});
it("shows and clears external changes from the actual helper state without issuing a power write", async () => {
  mock.command.mockResolvedValueOnce({
    ...initial,
    helper: "ready",
    ownership: "mixed",
    externalChanged: true,
  });
  render(<PowerSettings />);
  await screen.findByText("外部已修改");
  expect(screen.getByText("已就绪")).toBeInTheDocument();
  expect(mock.command).toHaveBeenCalledTimes(1);
  expect(mock.command).toHaveBeenCalledWith("get_clamshell_state");

  act(() => mock.listener({
    ...initial,
    helper: "ready",
    ownership: "application",
    externalChanged: false,
  }));
  expect(screen.queryByText("外部已修改")).not.toBeInTheDocument();
  expect(screen.getByText("已就绪")).toBeInTheDocument();
  expect(mock.command).toHaveBeenCalledTimes(1);
});
