import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import AgyLaunch from "./AgyLaunch";
import { agyGatewayDemo } from "./gateway-preview";
beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "get_agy_launch")
      return { installed: true, model: "gemini-fixture" };
    if (name === "choose_agy_directory") return "/fixture/project's $() 中文";
    if (name === "list_provider_models")
      return { models: ["gemini-other"], error: null };
  });
});
it("launches with an explicit model and directory without a credential payload", async () => {
  const user = userEvent.setup(),
    close = vi.fn();
  render(
    <AgyLaunch
      state={{ ...agyGatewayDemo, running: true, revision: "fixture-revision" }}
      close={close}
    />,
  );
  await waitFor(() =>
    expect(screen.getByLabelText("模型")).toHaveValue("gemini-fixture"),
  );
  await user.click(screen.getByRole("button", { name: "读取模型" }));
  await waitFor(() =>
    expect(
      document.querySelector('datalist option[value="gemini-other"]'),
    ).not.toBeNull(),
  );
  await user.click(screen.getByRole("button", { name: "选择工作目录" }));
  await user.click(screen.getByRole("button", { name: "启动" }));
  expect(mock.command).toHaveBeenCalledWith("launch_agy", {
    model: "gemini-fixture",
    directory: "/fixture/project's $() 中文",
    expectedRevision: "fixture-revision",
  });
  expect(close).toHaveBeenCalledOnce();
});
it("shows missing CLI and disabled gateway errors without launching", async () => {
  mock.command.mockResolvedValue({ installed: false, model: null });
  render(
    <AgyLaunch
      state={{ ...agyGatewayDemo, running: false }}
      close={() => {}}
    />,
  );
  await waitFor(() =>
    expect(screen.getByRole("alert")).toHaveTextContent("未找到 agy"),
  );
  expect(screen.getByRole("button", { name: "启动" })).toBeDisabled();
  expect(mock.command.mock.calls.some(([name]) => name === "launch_agy")).toBe(
    false,
  );
});
