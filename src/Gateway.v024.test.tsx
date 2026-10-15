import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ClientId, GatewayState } from "./types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  confirmAction: vi.fn(async () => true),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async () => () => {}),
}));
vi.mock("./confirmation", () => ({ confirmAction: mock.confirmAction }));

import Gateway from "./Gateway";
import { gatewayDemo, grokGatewayDemo } from "./gateway-preview";

let codex: GatewayState;
let grok: GatewayState;

beforeEach(() => {
  localStorage.clear();
  codex = { ...structuredClone(gatewayDemo), providers: [] };
  grok = { ...structuredClone(grokGatewayDemo), providers: [] };
  mock.command.mockReset();
  mock.confirmAction.mockReset().mockResolvedValue(true);
  mock.command.mockImplementation(
    async (name: string, args?: { clientId?: ClientId }) => {
      const state = args?.clientId === "grok" ? grok : codex;
      if (name === "get_gateway" || name === "update_gateway")
        return structuredClone(state);
      throw new Error(`Unexpected fixture command: ${name}`);
    },
  );
});

afterEach(() => localStorage.clear());

it("switches to Grok and saves a provider with the selected upstream protocol", async () => {
  const user = userEvent.setup();
  render(<Gateway notify={() => {}} />);

  await screen.findByRole("button", { name: "添加" });
  expect(
    within(screen.getByRole("navigation", { name: "客户端" })).getByRole(
      "button",
      { name: "Codex" },
    ),
  ).toHaveAttribute("aria-pressed", "true");
  await user.click(
    within(screen.getByRole("navigation", { name: "客户端" })).getByRole(
      "button",
      { name: "Grok Build" },
    ),
  );
  await waitFor(() =>
    expect(
      within(screen.getByRole("navigation", { name: "客户端" })).getByRole(
        "button",
        { name: "Grok Build" },
      ),
    ).toHaveAttribute("aria-pressed", "true"),
  );
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("get_gateway", {
      clientId: "grok",
    }),
  );
  expect(localStorage.getItem("lich13-switch.main.client")).toBe("grok");

  await user.click(await screen.findByRole("button", { name: "添加" }));
  const dialog = await screen.findByRole("dialog", { name: "添加供应商" });
  const protocol = within(dialog).getByLabelText("API 协议");
  expect(protocol).toHaveValue("responses");
  await user.selectOptions(protocol, "messages");
  await user.type(within(dialog).getByLabelText("名称"), "fixture grok provider");
  await user.type(
    within(dialog).getByLabelText("base_url"),
    "https://api.example.invalid/v1",
  );
  await user.type(within(dialog).getByLabelText("api_key"), "fixture-token");
  await user.click(within(dialog).getByRole("button", { name: "保存" }));

  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith(
      "update_gateway",
      expect.objectContaining({
        clientId: "grok",
        expectedRevision: grok.revision,
        edit: {
          op: "saveGrokProvider",
          protocol: "messages",
          id: null,
          baseUrl: "https://api.example.invalid/v1",
          token: "fixture-token",
          name: "fixture grok provider",
        },
      }),
    ),
  );
});
