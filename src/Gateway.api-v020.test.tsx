import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { GatewayState } from "./types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (state: GatewayState) => void>(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (state: GatewayState) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));

import Gateway from "./Gateway";
import { claudeGatewayDemo, gatewayDemo } from "./gateway-preview";

let codexState: GatewayState;
let claudeState: GatewayState;

function stateFor(clientId: unknown) {
  return clientId === "claude" ? claudeState : codexState;
}

beforeEach(() => {
  localStorage.clear();
  codexState = { ...structuredClone(gatewayDemo), connectionMode: "bearer" };
  claudeState = structuredClone(claudeGatewayDemo);
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      const current = stateFor(args.clientId);
      if (name === "get_gateway") return structuredClone(current);
      if (name === "query_provider_quota") return undefined;
      if (name === "update_gateway") {
        const edit = args.edit as { op?: string; mode?: "bearer" | "apiKey" };
        if (edit.op === "connection") {
          current.connectionMode = edit.mode;
          current.revision = `saved-${current.clientId}`;
        }
        return structuredClone(current);
      }
      return undefined;
    },
  );
});

async function openAdvanced(user: ReturnType<typeof userEvent.setup>) {
  await screen.findByRole("button", { name: "添加" });
  await user.click(screen.getByText("高级设置", { selector: "summary" }));
}

async function openProviderEditor(
  user: ReturnType<typeof userEvent.setup>,
  action: "add" | "edit",
) {
  if (action === "add") {
    await user.click(screen.getByRole("button", { name: "添加" }));
    return screen.getByRole("dialog", { name: "添加供应商" });
  }
  const provider = codexState.providers[0];
  const operations = screen.getByLabelText(`${provider.name} 操作`);
  await user.click(operations);
  await user.click(
    within(operations.closest("details")!).getByRole("button", {
      name: "编辑 API",
    }),
  );
  return screen.getByRole("dialog", { name: "编辑供应商" });
}

describe("Codex v0.20.0 API login gateway controls", () => {
  it("persists the Codex connection mode with the current revision", async () => {
    const user = userEvent.setup();
    const notify = vi.fn();
    render(<Gateway notify={notify} />);
    await openAdvanced(user);

    const mode = screen.getByRole("combobox", { name: "Codex 连接方式" });
    expect(mode).toHaveValue("bearer");
    expect(
      within(mode).getByRole("option", { name: "独立 Bearer Token" }),
    ).toHaveValue("bearer");
    expect(
      within(mode).getByRole("option", { name: "普通 API 登录" }),
    ).toHaveValue("apiKey");

    await user.selectOptions(mode, "apiKey");
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("update_gateway", {
        clientId: "codex",
        edit: { op: "connection", mode: "apiKey" },
        expectedRevision: "preview-gateway",
      }),
    );
    expect(codexState.connectionMode).toBe("apiKey");
    expect(notify).toHaveBeenCalledWith("连接方式已保存");
  });

  it.each([
    ["running", { running: true, recoveryPending: false }],
    ["recovery pending", { running: false, recoveryPending: true }],
  ] as const)("disables connection mode while the gateway is %s", async (_label, state) => {
    codexState = { ...codexState, ...state };
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await openAdvanced(user);

    const mode = screen.getByRole("combobox", { name: "Codex 连接方式" });
    expect(mode).toBeDisabled();
    expect(mock.command.mock.calls.some(([name]) => name === "update_gateway")).toBe(
      false,
    );
  });

  it("hides Codex connection mode from Claude", async () => {
    localStorage.setItem("lich13-switch.main.client", "claude");
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    await user.click(screen.getByText("高级设置", { selector: "summary" }));

    expect(
      screen.queryByRole("combobox", { name: "Codex 连接方式" }),
    ).not.toBeInTheDocument();
  });

  it.each([
    ["bearer", "experimental_bearer_token"],
    ["apiKey", "OPENAI_API_KEY"],
  ] as const)("uses %s credential labels for adding and editing suppliers", async (connectionMode, label) => {
    codexState.connectionMode = connectionMode;
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });

    const addDialog = await openProviderEditor(user, "add");
    expect(within(addDialog).getByLabelText(label)).toHaveAttribute(
      "type",
      "password",
    );
    expect(
      within(addDialog).queryByLabelText(
        connectionMode === "apiKey"
          ? "experimental_bearer_token"
          : "OPENAI_API_KEY",
      ),
    ).not.toBeInTheDocument();
    await user.click(within(addDialog).getByRole("button", { name: "取消" }));

    const editDialog = await openProviderEditor(user, "edit");
    expect(within(editDialog).getByLabelText(label)).toHaveAttribute(
      "type",
      "password",
    );
    expect(
      within(editDialog).queryByLabelText(
        connectionMode === "apiKey"
          ? "experimental_bearer_token"
          : "OPENAI_API_KEY",
      ),
    ).not.toBeInTheDocument();
  });

  it("keeps the API supplier draft and authoritative state unchanged after a failed save", async () => {
    codexState.connectionMode = "apiKey";
    const original = structuredClone(codexState);
    mock.command.mockImplementation(
      async (name: string, args: Record<string, unknown> = {}) => {
        if (name === "get_gateway") return structuredClone(codexState);
        if (name === "query_provider_quota") return undefined;
        if (name === "update_gateway") {
          const edit = args.edit as { op?: string };
          if (edit.op === "saveProvider") {
            throw { code: "WRITE", message: "供应商保存失败" };
          }
          return structuredClone(codexState);
        }
        return undefined;
      },
    );
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    const dialog = await openProviderEditor(user, "add");
    await user.type(within(dialog).getByLabelText("base_url"), "https://api.fixture.invalid/v1");
    await user.type(within(dialog).getByLabelText("OPENAI_API_KEY"), "fixture-api-key");
    await user.click(within(dialog).getByRole("button", { name: "保存" }));

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "供应商保存失败",
    );
    expect(within(dialog).getByLabelText("base_url")).toHaveValue(
      "https://api.fixture.invalid/v1",
    );
    expect(within(dialog).getByLabelText("OPENAI_API_KEY")).toHaveValue(
      "fixture-api-key",
    );
    expect(codexState).toEqual(original);
    expect(mock.command).toHaveBeenCalledWith("update_gateway", {
      clientId: "codex",
      edit: {
        op: "saveProvider",
        id: null,
        name: "",
        baseUrl: "https://api.fixture.invalid/v1",
        token: "fixture-api-key",
      },
      expectedRevision: original.revision,
    });
  });

  it("keeps a supplier draft when a background gateway refresh arrives", async () => {
    codexState.connectionMode = "apiKey";
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    const dialog = await openProviderEditor(user, "add");
    await user.type(within(dialog).getByLabelText("base_url"), "https://draft.fixture.invalid/v1");
    await user.type(within(dialog).getByLabelText("OPENAI_API_KEY"), "draft-api-key");

    act(() =>
      mock.listeners.get("gateway-state")!({
        ...codexState,
        revision: "background-refresh",
        activeConnections: 2,
      }),
    );

    expect(within(dialog).getByLabelText("base_url")).toHaveValue(
      "https://draft.fixture.invalid/v1",
    );
    expect(within(dialog).getByLabelText("OPENAI_API_KEY")).toHaveValue(
      "draft-api-key",
    );
  });
});
