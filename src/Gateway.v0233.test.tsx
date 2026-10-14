import {
  act,
  fireEvent,
  render,
  screen,
  within,
  waitFor,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { ClientId, GatewayState } from "./types";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, Set<(value: unknown) => void>>(),
  confirmAction: vi.fn(async () => true),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async <T,>(name: string, listener: (value: T) => void) => {
    let listeners = mock.listeners.get(name);
    if (!listeners) {
      listeners = new Set();
      mock.listeners.set(name, listeners);
    }
    const wrapped = (value: unknown) => listener(value as T);
    listeners.add(wrapped);
    return () => listeners?.delete(wrapped);
  }),
}));
vi.mock("./confirmation", () => ({ confirmAction: mock.confirmAction }));
import Gateway from "./Gateway";
import { claudeGatewayDemo, gatewayDemo } from "./gateway-preview";

type CommandArgs = {
  clientId?: ClientId;
  providerId?: string;
  expectedRevision?: string | null;
  edit?: Record<string, unknown>;
};

let gateways: Record<ClientId, GatewayState>;
let revealToken: (clientId: ClientId) => Promise<string>;
let nextUpdateError: unknown | null;

const tokenLabel = (clientId: ClientId) =>
  clientId === "claude" ? "ANTHROPIC_AUTH_TOKEN" : "experimental_bearer_token";
const baseUrlLabel = (clientId: ClientId) =>
  clientId === "claude" ? "ANTHROPIC_BASE_URL" : "base_url";

function emit(name: string, value: unknown) {
  act(() => {
    for (const listener of mock.listeners.get(name) ?? []) listener(value);
  });
}

async function openProviderEditor(
  user: ReturnType<typeof userEvent.setup>,
  clientId: ClientId,
) {
  const provider = gateways[clientId].providers[0];
  await screen.findByRole("button", { name: "添加" });
  const menuSummary = screen.getByLabelText(`${provider.name} 操作`);
  const menu = menuSummary.closest("details");
  if (!menu) throw new Error("供应商操作菜单不存在");
  await user.click(menuSummary);
  await user.click(within(menu).getByRole("button", { name: "编辑 API" }));
  await screen.findByRole("dialog", { name: "编辑供应商" });
  return provider;
}

function selectInitialClient(clientId: ClientId) {
  localStorage.setItem("lich13-switch.main.client", clientId);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

beforeEach(() => {
  localStorage.clear();
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
  gateways = {
    codex: structuredClone(gatewayDemo),
    claude: structuredClone(claudeGatewayDemo),
  };
  revealToken = async (clientId) => `fixture-stored-${clientId}-token`;
  nextUpdateError = null;
  mock.command.mockReset();
  mock.listeners.clear();
  mock.confirmAction.mockReset().mockResolvedValue(true);
  mock.command.mockImplementation(
    async (name: string, args?: CommandArgs) => {
      const clientId = args?.clientId ?? "codex";
      const current = gateways[clientId];
      if (name === "get_gateway") return structuredClone(current);
      if (name === "reveal_provider_token") return revealToken(clientId);
      if (name === "update_gateway") {
        if (nextUpdateError) {
          const error = nextUpdateError;
          nextUpdateError = null;
          current.revision = "fixture-fresh-revision";
          throw error;
        }
        return structuredClone(current);
      }
    },
  );
});

afterEach(() => {
  localStorage.clear();
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});

describe("gateway provider token visibility", () => {
  it.each(["codex", "claude"] as const)(
    "keeps a new %s credential masked and submits only user input",
    async (clientId) => {
      selectInitialClient(clientId);
      const user = userEvent.setup();
      render(<Gateway notify={() => {}} />);

      await user.click(await screen.findByRole("button", { name: "添加" }));
      await screen.findByRole("dialog", { name: "添加供应商" });
      const token = screen.getByLabelText(tokenLabel(clientId));
      expect(token).toHaveAttribute("type", "password");

      const show = screen.getByRole("button", { name: "显示 Token" });
      expect(show.textContent).toBe("");
      await user.click(show);
      expect(token).toHaveAttribute("type", "text");
      await user.type(token, `fixture-user-${clientId}-token`);
      expect(token).toHaveValue(`fixture-user-${clientId}-token`);
      expect(
        mock.command.mock.calls.some(([name]) => name === "reveal_provider_token"),
      ).toBe(false);

      const hide = screen.getByRole("button", { name: "隐藏 Token" });
      expect(hide.textContent).toBe("");
      await user.click(hide);
      expect(token).toHaveAttribute("type", "password");
      expect(token).toHaveValue(`fixture-user-${clientId}-token`);

      const name = `fixture ${clientId} provider`;
      const baseUrl = "https://new.example.invalid/v1";
      await user.type(screen.getByLabelText("名称"), name);
      await user.type(screen.getByLabelText(baseUrlLabel(clientId)), baseUrl);
      await user.click(screen.getByRole("button", { name: "保存" }));

      await waitFor(() =>
        expect(mock.command).toHaveBeenCalledWith("update_gateway", {
          clientId,
          edit: {
            op: "saveProvider",
            id: null,
            baseUrl,
            token: `fixture-user-${clientId}-token`,
            name,
          },
          expectedRevision: gateways[clientId].revision,
        }),
      );
    },
  );

  it.each(["codex", "claude"] as const)(
    "reveals an existing %s credential only on request and never saves the revealed value",
    async (clientId) => {
      selectInitialClient(clientId);
      const user = userEvent.setup();
      const dirty = vi.fn();
      render(<Gateway notify={() => {}} onDirtyChange={dirty} />);
      const provider = await openProviderEditor(user, clientId);
      const token = screen.getByLabelText(tokenLabel(clientId));
      expect(token).toHaveAttribute("type", "password");
      expect(token).toHaveValue("");

      const show = screen.getByRole("button", { name: "显示 Token" });
      expect(show.textContent).toBe("");
      await user.click(show);
      await waitFor(() =>
        expect(token).toHaveValue(`fixture-stored-${clientId}-token`),
      );
      expect(mock.command).toHaveBeenCalledWith("reveal_provider_token", {
        clientId,
        providerId: provider.id,
        expectedRevision: gateways[clientId].revision,
      });
      expect(dirty).not.toHaveBeenCalledWith(true);
      expect(
        mock.command.mock.calls.some(([name]) => name === "update_gateway"),
      ).toBe(false);

      await user.click(screen.getByRole("button", { name: "保存" }));
      await waitFor(() =>
        expect(mock.command).toHaveBeenCalledWith(
          "update_gateway",
          expect.objectContaining({
            clientId,
            expectedRevision: gateways[clientId].revision,
            edit: expect.objectContaining({
              op: "saveProvider",
              id: provider.id,
              baseUrl: provider.baseUrl,
              token: "",
            }),
          }),
        ),
      );
    },
  );

  it("keeps a revealed value separate from a typed replacement across a save conflict and retry", async () => {
    selectInitialClient("codex");
    nextUpdateError = {
      code: "CONFLICT",
      message: "网关设置已变化，请使用最新状态重试",
    };
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    const provider = await openProviderEditor(user, "codex");
    const token = screen.getByLabelText(tokenLabel("codex"));

    await user.click(screen.getByRole("button", { name: "显示 Token" }));
    await waitFor(() =>
      expect(token).toHaveValue("fixture-stored-codex-token"),
    );
    await user.clear(token);
    await user.type(token, "fixture-user-replacement-token");
    await user.click(screen.getByRole("button", { name: "保存" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "网关设置已变化",
    );
    expect(token).toHaveValue("fixture-user-replacement-token");
    await user.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "编辑供应商" }),
      ).not.toBeInTheDocument(),
    );

    const updates = mock.command.mock.calls.filter(
      ([name]) => name === "update_gateway",
    );
    expect(updates).toHaveLength(2);
    expect(updates[0][1]).toMatchObject({
      clientId: "codex",
      expectedRevision: "preview-gateway",
      edit: expect.objectContaining({
        id: provider.id,
        token: "fixture-user-replacement-token",
      }),
    });
    expect(updates[1][1]).toMatchObject({
      clientId: "codex",
      expectedRevision: "fixture-fresh-revision",
      edit: expect.objectContaining({
        id: provider.id,
        token: "fixture-user-replacement-token",
      }),
    });
  });

  it.each([
    ["read failure", { code: "TOKEN", message: "读取凭据失败" }],
    ["revision conflict", { code: "CONFLICT", message: "网关设置已变化" }],
  ])("keeps the credential hidden after a reveal %s", async (_label, error) => {
    selectInitialClient("codex");
    revealToken = async () => {
      throw error;
    };
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await openProviderEditor(user, "codex");
    const token = screen.getByLabelText(tokenLabel("codex"));

    await user.click(screen.getByRole("button", { name: "显示 Token" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(error.message);
    expect(token).toHaveAttribute("type", "password");
    expect(token).toHaveValue("");
    expect(
      mock.command.mock.calls.some(([name]) => name === "update_gateway"),
    ).toBe(false);
  });

  it.each(["document visibilitychange", "native app-visibility"] as const)(
    "clears a revealed value and keeps a typed draft when the %s reports hidden",
    async (visibilitySource) => {
      selectInitialClient("codex");
      const user = userEvent.setup();
      render(<Gateway notify={() => {}} />);
      await openProviderEditor(user, "codex");
      const token = screen.getByLabelText(tokenLabel("codex"));
      await user.click(screen.getByRole("button", { name: "显示 Token" }));
      await waitFor(() =>
        expect(token).toHaveValue("fixture-stored-codex-token"),
      );
      await user.clear(token);
      await user.type(token, "fixture-user-draft-token");

      if (visibilitySource === "document visibilitychange") {
        Object.defineProperty(document, "visibilityState", {
          configurable: true,
          value: "hidden",
        });
        act(() => document.dispatchEvent(new Event("visibilitychange")));
      } else {
        emit("app-visibility", false);
      }

      expect(token).toHaveAttribute("type", "password");
      expect(token).toHaveValue("fixture-user-draft-token");
    },
  );

  it("clears the revealed value when explicitly hidden while retaining typed input", async () => {
    selectInitialClient("codex");
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await openProviderEditor(user, "codex");
    const token = screen.getByLabelText(tokenLabel("codex"));

    await user.click(screen.getByRole("button", { name: "显示 Token" }));
    await waitFor(() =>
      expect(token).toHaveValue("fixture-stored-codex-token"),
    );
    await user.click(screen.getByRole("button", { name: "隐藏 Token" }));
    expect(token).toHaveAttribute("type", "password");
    expect(token).toHaveValue("");

    await user.click(screen.getByRole("button", { name: "显示 Token" }));
    await waitFor(() =>
      expect(token).toHaveValue("fixture-stored-codex-token"),
    );
    await user.clear(token);
    await user.type(token, "fixture-user-draft-token");
    await user.click(screen.getByRole("button", { name: "隐藏 Token" }));
    expect(token).toHaveAttribute("type", "password");
    expect(token).toHaveValue("fixture-user-draft-token");
  });

  it.each(["close", "client switch"] as const)(
    "ignores a late reveal response after %s",
    async (exitAction) => {
      selectInitialClient("codex");
      const pending = deferred<string>();
      revealToken = () => pending.promise;
      const user = userEvent.setup();
      render(<Gateway notify={() => {}} />);
      await openProviderEditor(user, "codex");
      await user.click(screen.getByRole("button", { name: "显示 Token" }));
      await waitFor(() =>
        expect(mock.command).toHaveBeenCalledWith(
          "reveal_provider_token",
          expect.objectContaining({ clientId: "codex" }),
        ),
      );

      if (exitAction === "close") {
        await user.click(screen.getByRole("button", { name: "关闭" }));
        await waitFor(() =>
          expect(
            screen.queryByRole("dialog", { name: "编辑供应商" }),
          ).not.toBeInTheDocument(),
        );
      } else {
        fireEvent.click(screen.getByRole("button", { name: "Claude Code" }));
        await screen.findByRole("button", { name: "添加" });
        expect(mock.confirmAction).not.toHaveBeenCalled();
      }

      await act(async () => {
        pending.resolve("fixture-late-codex-token");
        await pending.promise;
      });
      expect(
        screen.queryByDisplayValue("fixture-late-codex-token"),
      ).not.toBeInTheDocument();

      if (exitAction === "close") {
        await user.click(await screen.findByRole("button", { name: "添加" }));
        await screen.findByRole("dialog", { name: "添加供应商" });
      } else {
        fireEvent.click(screen.getByRole("button", { name: "Codex" }));
        await openProviderEditor(user, "codex");
      }
      const token = screen.getByLabelText(tokenLabel("codex"));
      expect(token).toHaveAttribute("type", "password");
      expect(token).toHaveValue("");
    },
  );
});
