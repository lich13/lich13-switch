import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { LinkHandlers } from "./bridge-contracts";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (visible: boolean) => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(
    async (name: string, listener: (visible: boolean) => void) => {
      mock.listeners.set(name, listener);
      return () => mock.listeners.delete(name);
    },
  ),
}));
import LinkSettings from "./LinkSettings";
const SELF = "com.lich13.gpt-switch",
  OTHER = "com.ccswitch.desktop";
let state: LinkHandlers, settingResult: LinkHandlers, settingError: unknown;
let settingWait: Promise<void> | null;
beforeEach(() => {
  state = {
    current: OTHER,
    remembered: null,
    systemPicker: false,
    apps: [
      { id: SELF, name: "lich13-switch" },
      { id: OTHER, name: "CC Switch" },
    ],
  };
  settingResult = state;
  settingError = null;
  settingWait = null;
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args?: { appId: string | null }) => {
      if (name === "get_link_handler_state") return structuredClone(state);
      if (name === "get_startup_error") return null;
      if (name === "enable_link_picker" || name === "set_link_receiver") {
        if (settingError) throw settingError;
        if (settingWait) await settingWait;
        if (name === "set_link_receiver")
          state = { ...state, remembered: args!.appId };
        return structuredClone(
          name === "enable_link_picker" ? settingResult : state,
        );
      }
      throw new Error(`Unexpected command: ${name}`);
    },
  );
});
it("defaults to asking and independently remembers, changes and clears the receiver", async () => {
  const user = userEvent.setup();
  render(<LinkSettings />);
  const select = screen.getByRole("combobox", { name: "接收应用" });
  await waitFor(() => expect(select).toBeEnabled());
  expect(select).toHaveValue("");
  expect(screen.getByLabelText("下次不再提示")).not.toBeChecked();
  await user.selectOptions(select, OTHER);
  expect(mock.command).not.toHaveBeenCalledWith(
    "set_link_receiver",
    expect.anything(),
  );
  await user.click(screen.getByLabelText("下次不再提示"));
  await waitFor(() =>
    expect(screen.getByLabelText("下次不再提示")).toBeChecked(),
  );
  expect(mock.command).toHaveBeenCalledWith("set_link_receiver", {
    appId: OTHER,
  });
  await user.selectOptions(select, SELF);
  await waitFor(() => expect(select).toHaveValue(SELF));
  expect(mock.command).toHaveBeenCalledWith("set_link_receiver", {
    appId: SELF,
  });
  await user.click(screen.getByLabelText("下次不再提示"));
  await waitFor(() => expect(select).toHaveValue(""));
  expect(mock.command).toHaveBeenCalledWith("set_link_receiver", {
    appId: null,
  });
  expect(
    mock.command.mock.calls.some(([n]) => n === "enable_link_picker"),
  ).toBe(false);
});
it("reads back association changes and never assumes enabling the picker succeeded", async () => {
  let finish!: () => void;
  settingWait = new Promise((r) => {
    finish = r;
  });
  const user = userEvent.setup();
  render(<LinkSettings />);
  const button = await screen.findByRole("button", { name: "启用链接选择" });
  await user.click(button);
  expect(button).toBeDisabled();
  await act(async () => finish());
  expect(await screen.findByRole("alert")).toHaveTextContent("默认应用未更改");
  settingWait = null;
  settingResult = { ...state, current: SELF };
  await user.click(button);
  await waitFor(() =>
    expect(
      screen.queryByRole("button", { name: "启用链接选择" }),
    ).not.toBeInTheDocument(),
  );
});
it("refreshes the real Windows association after returning from system settings", async () => {
  state.systemPicker = true;
  settingResult = state;
  render(<LinkSettings />);
  await userEvent.click(
    await screen.findByRole("button", { name: "启用链接选择" }),
  );
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  state = { ...state, current: SELF };
  fireEvent(window, new Event("focus"));
  await waitFor(() =>
    expect(
      screen.queryByRole("button", { name: "启用链接选择" }),
    ).not.toBeInTheDocument(),
  );
  state = { ...state, current: OTHER };
  act(() => mock.listeners.get("app-visibility")?.(true));
  expect(
    await screen.findByRole("button", { name: "启用链接选择" }),
  ).toBeInTheDocument();
});
it("keeps a missing receiver visible and retains the saved checkbox after a write failure", async () => {
  state.remembered = "com.lich13.studio";
  settingError = { code: "IO", message: "无法保存设置" };
  render(<LinkSettings />);
  expect(
    await screen.findByRole("option", { name: "已记住的应用不可用" }),
  ).toBeInTheDocument();
  await userEvent.click(screen.getByLabelText("下次不再提示"));
  expect(await screen.findByRole("alert")).toHaveTextContent("无法保存设置");
  expect(screen.getByLabelText("下次不再提示")).toBeChecked();
  expect(screen.getByLabelText("下次不再提示")).toBeEnabled();
});
