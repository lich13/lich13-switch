import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { DshPreview } from "./bridge-contracts";
import { grokGatewayDemo } from "./gateway-preview";
import type { GatewayState } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import DshProfile from "./DshProfile";

const state: GatewayState = {
  ...grokGatewayDemo,
  clientId: "dsh",
  running: false,
  dshProfile: null,
};
beforeEach(() => {
  mock.command.mockReset().mockImplementation(async (name, args) => {
    if (name === "preview_dsh_profile")
      return {
        profiles: ["fixture"],
        profile: args.profile,
        protocol: args.protocol,
        revision: "fixture-preview",
        models: ["fixture-model"],
        attached: false,
        model: null,
        makeDefault: false,
        warning: null,
      } satisfies DshPreview;
    if (name === "attach_dsh_profiles")
      return {
        state: { ...state, running: true },
        results: [{ profile: "fixture", status: "success", error: null }],
      };
    throw new Error(`Unexpected command: ${name}`);
  });
});

it("previews profiles without writing and applies the selected protocol only on explicit attachment", async () => {
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
  expect(screen.getByLabelText("API 协议")).toHaveValue("chat_completions");
  expect(
    screen.getByRole("checkbox", { name: "设为默认（仅新会话）" }),
  ).not.toBeChecked();
  await user.selectOptions(screen.getByLabelText("现有 profile"), "fixture");
  await user.selectOptions(screen.getByLabelText("API 协议"), "messages");
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "接入 profile" })).toBeEnabled(),
  );
  expect(
    mock.command.mock.calls.every(([name]) => name === "preview_dsh_profile"),
  ).toBe(true);
  await user.click(
    screen.getByRole("checkbox", { name: "设为默认（仅新会话）" }),
  );
  await user.type(screen.getByLabelText("模型 ID"), "fixture-model");
  await user.click(screen.getByRole("button", { name: "接入 profile" }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(mock.command).toHaveBeenCalledWith("attach_dsh_profiles", {
    all: false,
    options: {
      profile: "fixture",
      protocol: "messages",
      model: "fixture-model",
      makeDefault: true,
    },
    expectedRevision: state.revision,
    previewRevision: "fixture-preview",
  });
  expect(update).toHaveBeenCalledWith(
    expect.objectContaining({ running: true }),
  );
});
