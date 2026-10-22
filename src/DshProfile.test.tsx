import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { DshPreview } from "./bridge-contracts";
import { grokGatewayDemo } from "./gateway-preview";
import type { GatewayState } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import DshProfile, { connectDsh } from "./DshProfile";

const state: GatewayState = {
  ...grokGatewayDemo,
  clientId: "dsh",
  running: false,
  dshProfile: null,
};
beforeEach(() => {
  localStorage.clear();
  mock.command.mockReset().mockImplementation(async (name, args) => {
    if (name === "preview_dsh_profile")
      return {
        profiles: ["fixture"],
        profile: "fixture",
        protocol: args.protocol,
        revision: "fixture-preview",
        models: ["fixture-model"],
        attached: false,
        model: "fixture-model",
        makeDefault: true,
        warning: null,
        current: {
          kind: "unmatched",
          model: "fixture-model",
          modelSource: "profile fixture",
          providerName: "Fixture",
          warning: null,
        },
        states: [],
        providers: [
          {
            profile: "fixture",
            name: "Fixture",
            models: ["deepseek-flash"],
            error: null,
          },
        ],
      } satisfies DshPreview;
    if (name === "attach_dsh_profiles")
      return {
        state: { ...state, running: true, mode: "auto" },
        results: [{ profile: "fixture", status: "success", error: null }],
      };
    throw new Error(`Unexpected command: ${name}`);
  });
});

it("defaults to desktop, fixes the protocol and imports all profiles in one action", async () => {
  const update = vi.fn(),
    close = vi.fn();
  const user = userEvent.setup();
  render(
    <DshProfile
      state={state}
      update={update}
      close={close}
      notify={() => {}}
    />,
  );
  await screen.findByRole("option", { name: "fixture" });
  expect(mock.command).toHaveBeenCalledWith("preview_dsh_profile", {
    profile: "desktop",
    protocol: "chat_completions",
  });
  expect(screen.queryByLabelText("API 协议")).not.toBeInTheDocument();
  expect(screen.getByLabelText("DSH 配置来源")).toHaveValue("fixture");
  expect(screen.getByLabelText("新会话默认模型")).toHaveValue("deepseek-flash");
  await user.selectOptions(
    screen.getByLabelText("新会话默认模型"),
    "deepseek-v4-pro",
  );
  expect(
    mock.command.mock.calls.every(([name]) => name === "preview_dsh_profile"),
  ).toBe(true);
  await user.click(screen.getByRole("button", { name: "导入并接入" }));
  await waitFor(() => expect(update).toHaveBeenCalledOnce());
  expect(close).toHaveBeenCalledOnce();
  expect(mock.command).toHaveBeenCalledWith("attach_dsh_profiles", {
    all: true,
    options: {
      profile: "fixture",
      protocol: "chat_completions",
      model: "deepseek-v4-pro",
      makeDefault: true,
    },
    expectedRevision: state.revision,
    previewRevision: "fixture-preview",
  });
  expect(update).toHaveBeenCalledWith(
    expect.objectContaining({ running: true, mode: "auto" }),
  );
});

it("preserves manual mode on ordinary sync and enables auto only when connecting", async () => {
  const preview = (await mock.command("preview_dsh_profile", {
    profile: "desktop",
    protocol: "chat_completions",
  })) as DshPreview;
  mock.command.mockImplementation(async (name, args) => {
    if (name === "attach_dsh_profiles")
      return {
        state: { ...state, running: true, mode: "manual" },
        results: [{ profile: "fixture", status: "success", error: null }],
      };
    if (name === "update_gateway")
      return { ...state, running: true, mode: args.edit.mode };
    throw Error(name);
  });
  expect((await connectDsh({ ...state, running: true }, preview)).mode).toBe(
    "manual",
  );
  expect(
    mock.command.mock.calls.filter(([name]) => name === "update_gateway"),
  ).toHaveLength(0);
  expect((await connectDsh(state, preview)).mode).toBe("auto");
  expect(
    mock.command.mock.calls.filter(([name]) => name === "update_gateway"),
  ).toHaveLength(1);
});
