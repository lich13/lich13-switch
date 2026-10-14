import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
const command = vi.hoisted(() => vi.fn());
vi.mock("./bridge", () => ({ command }));
import StartupSettings from "./StartupSettings";
const initial = {
  launchOnBoot: false,
  restoreGateway: false,
  revision: "old",
};
beforeEach(() => command.mockReset());
describe("native startup settings", () => {
  it("uses the verified system state and versioned updates", async () => {
    command.mockResolvedValueOnce(initial).mockResolvedValueOnce({
      ...initial,
      launchOnBoot: true,
      revision: "verified",
    });
    render(<StartupSettings />);
    expect(screen.queryByLabelText("启动时恢复网关")).not.toBeInTheDocument();
    const box = await screen.findByLabelText("开机静默启动");
    await waitFor(() => expect(box).toBeEnabled());
    await userEvent.click(box);
    expect(command).toHaveBeenLastCalledWith("set_startup", {
      enabled: true,
      expectedRevision: "old",
    });
    await waitFor(() => expect(box).toBeChecked());
  });
  it("keeps the old checked value when system registration fails", async () => {
    command.mockResolvedValueOnce(initial).mockRejectedValueOnce({
      code: "STARTUP",
      message: "系统登录项操作失败",
    });
    render(<StartupSettings />);
    expect(screen.queryByLabelText("启动时恢复网关")).not.toBeInTheDocument();
    const box = await screen.findByLabelText("开机静默启动");
    await waitFor(() => expect(box).toBeEnabled());
    await userEvent.click(box);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "系统登录项操作失败",
    );
    expect(box).not.toBeChecked();
  });
});
