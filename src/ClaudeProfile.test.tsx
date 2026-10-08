import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: any) => void>(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, callback: (value: any) => void) => {
    mock.listeners.set(name, callback);
    return () => mock.listeners.delete(name);
  }),
}));

import ClaudeProfile from "./ClaudeProfile";

type Profile = {
  mode: "official" | "api";
  revision: string;
  initialized: boolean;
  conflict: string | null;
  files: { role: string; exists: boolean; revision: string }[];
  warnings: string[];
};

type Login = { phase: string; authenticated: boolean; error: string | null };

let profile: Profile;

function state(mode: Profile["mode"] = "api"): Profile {
  return {
    mode,
    revision: `fixture-${mode}-revision`,
    initialized: true,
    conflict: null,
    files: [],
    warnings: [],
  };
}

const idle: Login = { phase: "idle", authenticated: false, error: null };

beforeEach(() => {
  profile = state();
  mock.listeners.clear();
  mock.command.mockReset();
  mock.command.mockImplementation(async (name: string, args?: any) => {
    if (name === "get_claude_profile") return structuredClone(profile);
    if (name === "claude_login_status") return { ...idle };
    if (name === "switch_claude_profile") {
      profile = {
        ...profile,
        mode: args.mode,
        revision: `fixture-${args.mode}-saved`,
      };
      return structuredClone(profile);
    }
    if (name === "start_claude_login")
      return { phase: "waiting", authenticated: false, error: null };
    if (name === "cancel_claude_login") return undefined;
    if (name === "recover_claude_profile") return structuredClone(profile);
    throw new Error(`Unexpected fixture command: ${name}`);
  });
});

afterEach(() => mock.listeners.clear());

describe("Claude profile controls", () => {
  it("switches to the official profile without starting OAuth", async () => {
    const user = userEvent.setup();
    const notify = vi.fn();
    const onChanged = vi.fn();
    render(<ClaudeProfile notify={notify} onChanged={onChanged} />);

    await screen.findByRole("button", { name: "API" });
    await user.click(screen.getByRole("button", { name: "官方" }));

    await screen.findByRole("button", { name: "官方登录" });
    expect(mock.command).toHaveBeenCalledWith("switch_claude_profile", {
      mode: "official",
      expectedRevision: "fixture-api-revision",
    });
    expect(mock.command).not.toHaveBeenCalledWith("start_claude_login");
    expect(onChanged).toHaveBeenCalledOnce();
    expect(notify).toHaveBeenCalledWith(
      "Claude 官方配置已启用，请重新打开 Claude Code",
    );
  });

  it("keeps a switch error visible after refreshing state and receiving an event", async () => {
    const user = userEvent.setup();
    mock.command.mockImplementation(async (name: string) => {
      if (name === "get_claude_profile") return structuredClone(profile);
      if (name === "claude_login_status") return { ...idle };
      if (name === "switch_claude_profile")
        throw {
          code: "CLAUDE_PROFILE_CONFLICT",
          message: "fixture conflict; retry safely",
        };
      throw new Error(`Unexpected fixture command: ${name}`);
    });
    render(<ClaudeProfile notify={() => {}} />);

    await screen.findByRole("button", { name: "API" });
    await user.click(screen.getByRole("button", { name: "官方" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "fixture conflict; retry safely",
    );
    expect(mock.command).toHaveBeenCalledWith("get_claude_profile");

    act(() => mock.listeners.get("claude-profile-state")?.(state("api")));
    expect(screen.getByRole("alert")).toHaveTextContent(
      "fixture conflict; retry safely",
    );
  });

  it("requires the caller to confirm discarding a draft before switching", async () => {
    const user = userEvent.setup();
    const beforeChange = vi.fn().mockResolvedValue(false);
    const { rerender } = render(
      <ClaudeProfile notify={() => {}} beforeChange={beforeChange} />,
    );
    await screen.findByRole("button", { name: "API" });

    await user.click(screen.getByRole("button", { name: "官方" }));
    expect(beforeChange).toHaveBeenCalledOnce();
    expect(mock.command).not.toHaveBeenCalledWith(
      "switch_claude_profile",
      expect.anything(),
    );

    beforeChange.mockResolvedValue(true);
    rerender(<ClaudeProfile notify={() => {}} beforeChange={beforeChange} />);
    await user.click(screen.getByRole("button", { name: "官方" }));
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("switch_claude_profile", {
        mode: "official",
        expectedRevision: "fixture-api-revision",
      }),
    );
  });

  it("synchronizes profile and login events and launches login only on explicit action", async () => {
    const user = userEvent.setup();
    render(<ClaudeProfile notify={() => {}} />);
    await screen.findByRole("button", { name: "API" });

    act(() => mock.listeners.get("claude-profile-state")?.(state("official")));
    await screen.findByRole("button", { name: "官方登录" });
    expect(mock.command).not.toHaveBeenCalledWith("start_claude_login");

    await user.click(screen.getByRole("button", { name: "官方登录" }));
    expect(await screen.findByRole("status")).toHaveTextContent(
      "等待浏览器授权",
    );
    expect(mock.command).toHaveBeenCalledWith("start_claude_login");

    act(() =>
      mock.listeners.get("claude-login-state")?.({
        phase: "complete",
        authenticated: true,
        error: null,
      } satisfies Login),
    );
    await screen.findByRole("button", { name: "已登录官方账号" });
  });
});
