import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import AntigravityAccounts from "./AntigravityAccounts";
import type { AntigravityAuthState } from "./bridge-contracts";

const mockCommand = vi.fn();

vi.mock("./bridge", () => ({
  command: (...args: unknown[]) => mockCommand(...args),
}));

const initialState: AntigravityAuthState = {
  revision: "rev-1",
  mode: "oauth",
  enableAdc: false,
  oauthPresent: true,
  oauthSaved: true,
  adcStatus: "disabled",
  adcSaved: true,
  adcType: "authorized_user",
  adcProject: "demo-project",
  backupCount: 2,
  accounts: [
    {
      id: "oauth-1",
      name: "OAuth 主号",
      kind: "oauth",
      email: "user1@example.invalid",
      projectId: null,
      sourceType: "json",
      updatedAt: 1710000000,
      current: true,
      credentialRevision: "cred-1",
    },
    {
      id: "oauth-2",
      name: "OAuth 备号",
      kind: "oauth",
      email: "user2@example.invalid",
      projectId: null,
      sourceType: "key",
      updatedAt: 1710000010,
      current: false,
      credentialRevision: "cred-2",
    },
    {
      id: "adc-1",
      name: "ADC 生产环境",
      kind: "adc",
      email: null,
      projectId: "demo-project",
      sourceType: "authorized_user",
      updatedAt: 1710000020,
      current: false,
      credentialRevision: "cred-3",
    },
  ],
};

describe("AntigravityAccounts", () => {
  afterEach(() => {
    cleanup();
    mockCommand.mockReset();
  });

  it("renders multi-OAuth and multi-ADC credentials and switches modes and accounts", async () => {
    const notify = vi.fn();
    const setError = vi.fn();

    mockCommand.mockImplementation(async (name: string) => {
      if (name === "get_antigravity_auth_state") {
        return initialState;
      }
      if (name === "set_antigravity_auth_mode") {
        return {
          ...initialState,
          revision: "rev-2",
          mode: "both",
          enableAdc: true,
          adcStatus: "active",
        };
      }
      if (name === "switch_antigravity_oauth") {
        return {
          ...initialState,
          revision: "rev-3",
          accounts: initialState.accounts.map((a) => ({
            ...a,
            current: a.id === "oauth-2",
          })),
        };
      }
      return initialState;
    });

    render(<AntigravityAccounts notify={notify} setError={setError} />);

    expect(await screen.findByText("OAuth 主号")).toBeTruthy();
    expect(screen.getByText("OAuth 备号")).toBeTruthy();
    expect(screen.getByText("ADC 生产环境")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "混合模式" }));
    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("set_antigravity_auth_mode", {
        mode: "both",
        expectedRevision: "rev-1",
      });
    });

    fireEvent.click(screen.getByRole("button", { name: "切换到 OAuth 备号" }));
    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("switch_antigravity_oauth", {
        id: "oauth-2",
        expectedRevision: "rev-2",
        disableAdc: false,
      });
    });
  });

  it("adds OAuth and ADC credentials via both JSON import and Key input", async () => {
    const notify = vi.fn();
    const setError = vi.fn();

    mockCommand.mockResolvedValue(initialState);

    render(<AntigravityAccounts notify={notify} setError={setError} />);
    await screen.findByText("OAuth 主号");

    fireEvent.click(screen.getByRole("button", { name: "添加 OAuth" }));
    fireEvent.change(screen.getByLabelText("凭据 JSON 内容"), {
      target: { value: '{"access_token":"tok-json"}' },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("add_antigravity_oauth", {
        input: {
          name: null,
          inputMode: "json",
          jsonText: '{"access_token":"tok-json"}',
          key: null,
          refreshToken: null,
          email: null,
          activate: true,
        },
      });
    });

    fireEvent.click(screen.getByRole("button", { name: "添加 ADC" }));
    fireEvent.click(screen.getByRole("tab", { name: "输入 Key" }));
    fireEvent.change(screen.getByLabelText("ADC Key"), {
      target: { value: "1//adc-refresh-key" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));

    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("add_antigravity_adc", {
        input: {
          name: null,
          inputMode: "key",
          jsonText: null,
          key: "1//adc-refresh-key",
          clientId: null,
          clientSecret: null,
          projectId: null,
          clientEmail: null,
          activate: true,
        },
      });
    });
  });
});
