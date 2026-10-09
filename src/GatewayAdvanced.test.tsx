import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { GatewaySettings, GatewayState, ViewState } from "./types";

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
import QuotaRefreshSettings from "./QuotaRefreshSettings";
import { claudeGatewayDemo, gatewayDemo } from "./gateway-preview";

let codexState: GatewayState;
let claudeState: GatewayState;

beforeEach(() => {
  localStorage.clear();
  codexState = structuredClone(gatewayDemo);
  claudeState = structuredClone(claudeGatewayDemo);
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(async (name: string, args: Record<string, unknown> = {}) => {
    if (name === "get_gateway") {
      return structuredClone(args.clientId === "claude" ? claudeState : codexState);
    }
    if (name === "update_gateway") {
      const edit = args.edit as { op?: string; settings?: GatewaySettings };
      if (edit.op === "settings" && edit.settings) {
        if (args.clientId === "claude") {
          claudeState = { ...claudeState, settings: edit.settings, revision: "saved-claude" };
          return structuredClone(claudeState);
        }
        codexState = { ...codexState, settings: edit.settings, revision: "saved-codex" };
        return structuredClone(codexState);
      }
    }
    return undefined;
  });
});

describe("gateway advanced settings", () => {
  it("shows the Codex WebSocket retry default and persists edits", async () => {
    localStorage.setItem("lich13-switch.main.client", "codex");
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    await user.click(screen.getByText("高级设置"));

    const handoff = screen.getByRole("checkbox", { name: "压缩后接管" });
    expect(handoff).toBeChecked();
    await user.click(handoff);
    const retry = screen.getByRole("spinbutton", { name: "WebSocket 断开等待 / 秒" });
    expect(retry).toHaveValue(60);
    await user.clear(retry);
    await user.type(retry, "75");
    await user.click(screen.getByRole("button", { name: "保存参数" }));

    await waitFor(() => {
      expect(mock.command).toHaveBeenCalledWith(
        "update_gateway",
        expect.objectContaining({
          clientId: "codex",
          edit: {
            op: "settings",
            settings: expect.objectContaining({
              websocketRetrySeconds: 75,
              handoffAfterCompaction: false,
            }),
          },
        }),
      );
    });
  });

  it("does not expose WebSocket retry settings to Claude", async () => {
    localStorage.setItem("lich13-switch.main.client", "claude");
    const user = userEvent.setup();
    render(<Gateway notify={() => {}} />);
    await screen.findByRole("button", { name: "添加" });
    await user.click(screen.getByText("高级设置"));

    expect(screen.queryByLabelText("WebSocket 断开等待 / 秒")).not.toBeInTheDocument();
    expect(screen.queryByRole("checkbox", { name: "压缩后接管" })).not.toBeInTheDocument();
  });
});

describe("quota refresh settings", () => {
  it("saves only the quota refresh interval with its expected value", async () => {
    const user = userEvent.setup();
    const state = { preferences: { quotaRefreshSeconds: 120 } } as ViewState;
    mock.command.mockResolvedValue(state);
    render(<QuotaRefreshSettings seconds={60} />);

    const interval = screen.getByRole("spinbutton", { name: "额度刷新间隔" });
    expect(interval).toHaveValue(60);
    await user.clear(interval);
    await user.type(interval, "120");
    await user.click(screen.getByRole("button", { name: "保存刷新设置" }));

    await waitFor(() => {
      expect(mock.command).toHaveBeenCalledWith("set_quota_refresh", {
        seconds: 120,
        expectedSeconds: 60,
      });
    });
    expect(mock.command).toHaveBeenCalledTimes(1);
  });

  it("disables automatic refresh using zero seconds", async () => {
    const user = userEvent.setup();
    mock.command.mockResolvedValue({ preferences: { quotaRefreshSeconds: 0 } });
    render(<QuotaRefreshSettings seconds={120} />);

    await user.click(screen.getByRole("checkbox", { name: "额度自动刷新" }));
    await user.click(screen.getByRole("button", { name: "保存刷新设置" }));

    await waitFor(() => {
      expect(mock.command).toHaveBeenCalledWith("set_quota_refresh", {
        seconds: 0,
        expectedSeconds: 120,
      });
    });
    expect(mock.command).toHaveBeenCalledTimes(1);
  });

  it.each(["9", "86401", "10.5"])("rejects invalid enabled interval %s before submitting", async (value) => {
    const user = userEvent.setup();
    render(<QuotaRefreshSettings seconds={60} />);
    const interval = screen.getByRole("spinbutton", { name: "额度刷新间隔" });
    await user.clear(interval);
    await user.type(interval, value);
    await user.click(screen.getByRole("button", { name: "保存刷新设置" }));

    expect(interval).toBeInvalid();
    expect(mock.command).not.toHaveBeenCalled();
  });
});
