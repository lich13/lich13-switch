import {
  act,
  cleanup,
  fireEvent,
  render,
  screen,
  waitFor,
} from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import AntigravityAccounts from "./AntigravityAccounts";
import { subscribe } from "./bridge";
import type { AntigravityAuthState } from "./bridge-contracts";

const mockCommand = vi.fn();
const listeners = new Map<string, (payload: unknown) => void>();

vi.mock("./bridge", () => ({
  command: (...args: unknown[]) => mockCommand(...args),
  subscribe: vi.fn(async (event: string, cb: (payload: unknown) => void) => {
    listeners.set(event, cb);
    return () => {
      listeners.delete(event);
    };
  }),
}));

const initialState: AntigravityAuthState = {
  revision: "rev-1",
  mode: "oauth",
  enableAdc: false,
  oauthPresent: true,
  oauthEmail: "user1@example.invalid",
  oauthAuthMethod: "google-oauth",
  adcStatus: "disabled",
  adcType: "authorized_user",
  adcEmail: "user1@example.invalid",
  adcProject: "demo-project",
  appRestarted: false,
  accounts: [
    {
      id: "oauth-1",
      name: "OAuth 主号",
      kind: "oauth",
      email: "user1@example.invalid",
      projectId: null,
      sourceType: "google-oauth",
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
      sourceType: "jetski-sso",
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
    listeners.clear();
  });

  it("renders multi-OAuth and multi-ADC credentials and switches modes and accounts without redundant manual buttons", async () => {
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
          mode: "adc",
          enableAdc: true,
          adcStatus: "active",
          appRestarted: true,
        };
      }
      if (name === "switch_antigravity_account") {
        return {
          ...initialState,
          revision: "rev-3",
          mode: "oauth",
          enableAdc: false,
          appRestarted: true,
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
    expect(screen.queryByRole("button", { name: "备份当前" })).toBeNull();
    expect(screen.queryByRole("button", { name: "读取当前 OAuth" })).toBeNull();
    expect(screen.queryByRole("button", { name: "保存当前 ADC" })).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "ADC 模式" }));
    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("set_antigravity_auth_mode", {
        mode: "adc",
        expectedRevision: "rev-1",
      });
    });

    fireEvent.click(screen.getByRole("button", { name: "切换到 OAuth 备号" }));
    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("switch_antigravity_account", {
        id: "oauth-2",
        expectedRevision: "rev-2",
      });
    });
  });

  it("auto-updates accounts from antigravity-auth-state events and validates ADC JSON before submission", async () => {
    const notify = vi.fn();
    const setError = vi.fn();

    mockCommand.mockResolvedValue(initialState);

    render(<AntigravityAccounts notify={notify} setError={setError} />);
    await screen.findByText("OAuth 主号");

    act(() => {
      listeners.get("antigravity-auth-state")?.({
        ...initialState,
        revision: "rev-auto",
        mode: "mixed_adc_file_without_flag",
        accounts: [
          ...initialState.accounts,
          {
            id: "oauth-3",
            name: "user3@example.invalid",
            kind: "oauth",
            email: "user3@example.invalid",
            projectId: null,
            sourceType: "google-oauth",
            updatedAt: 1710000099,
            current: true,
            credentialRevision: "cred-auto",
          },
        ],
      });
    });
    expect(
      (await screen.findAllByText("user3@example.invalid")).length,
    ).toBeGreaterThan(0);
    expect(
      screen.getByText(/不一致（存在 ADC 文件但未启用设置）/),
    ).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "导入账号" }));
    const submitBtn = screen.getByRole("button", { name: "保存" });
    expect(submitBtn).toBeDisabled();

    fireEvent.change(screen.getByLabelText("凭据 JSON 内容"), {
      target: {
        value: JSON.stringify({
          type: "authorized_user",
          client_id: "cid.apps.googleusercontent.com",
          client_secret: "GOCSPX-secret",
          refresh_token: "1//refresh-token",
          quota_project_id: "my-gcp-project",
        }),
      },
    });
    expect(submitBtn).not.toBeDisabled();
    fireEvent.click(submitBtn);

    await waitFor(() => {
      expect(mockCommand).toHaveBeenCalledWith("add_antigravity_adc", {
        input: {
          name: null,
          jsonText: JSON.stringify({
            type: "authorized_user",
            client_id: "cid.apps.googleusercontent.com",
            client_secret: "GOCSPX-secret",
            refresh_token: "1//refresh-token",
            quota_project_id: "my-gcp-project",
          }),
          quotaProjectId: null,
          activate: true,
        },
      });
    });
  });

  it("imports a masked Vertex key with editable models and keeps errors in the form", async () => {
    mockCommand.mockImplementation(async (name: string) => {
      if (name === "save_antigravity_vertex_key")
        throw new Error("地区格式无效");
      return initialState;
    });
    render(<AntigravityAccounts notify={vi.fn()} setError={vi.fn()} />);
    await screen.findByText("OAuth 主号");
    fireEvent.click(screen.getByRole("button", { name: "导入账号" }));
    fireEvent.click(screen.getByRole("tab", { name: "Vertex API Key" }));
    expect(screen.getByLabelText("API Key")).toHaveAttribute(
      "type",
      "password",
    );
    expect(screen.getByLabelText("地区")).toHaveValue("global");
    expect(screen.getByLabelText("模型")).toHaveValue(
      "gemini-3-flash-preview\ngemini-3.1-pro-preview\ngemini-2.5-flash",
    );
    fireEvent.change(screen.getByLabelText("名称"), {
      target: { value: "Fixture Vertex" },
    });
    fireEvent.change(screen.getByLabelText("API Key"), {
      target: { value: "fixture-key" },
    });
    fireEvent.change(screen.getByLabelText("模型"), {
      target: { value: "gemini-2.5-flash" },
    });
    fireEvent.click(screen.getByLabelText("立即启用"));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("地区格式无效");
    expect(screen.getByLabelText("API Key")).toHaveValue("fixture-key");
    expect(mockCommand).toHaveBeenCalledWith("save_antigravity_vertex_key", {
      input: {
        id: null,
        name: "Fixture Vertex",
        apiKey: "fixture-key",
        projectId: null,
        location: "global",
        models: ["gemini-2.5-flash"],
        activate: false,
        expectedRevision: "rev-1",
      },
    });
  });

  it("edits Vertex metadata without reading or resubmitting the stored key", async () => {
    const vertex = {
      ...initialState.accounts[2],
      id: "vertex-1",
      name: "Fixture Vertex",
      kind: "vertex_key" as const,
      sourceType: "vertex_key",
      current: true,
      vertex: { location: "global", models: ["gemini-2.5-flash"] },
    };
    mockCommand.mockResolvedValue({
      ...initialState,
      mode: "vertex_key",
      accounts: [vertex],
    });
    render(<AntigravityAccounts notify={vi.fn()} setError={vi.fn()} />);
    await screen.findByText("Fixture Vertex");
    fireEvent.click(screen.getByLabelText("管理 Fixture Vertex"));
    fireEvent.click(screen.getByRole("button", { name: "编辑凭据" }));
    expect(screen.getByLabelText("API Key")).toHaveValue("");
    expect(screen.getByLabelText("立即启用")).toBeDisabled();
    fireEvent.change(screen.getByLabelText("地区"), {
      target: { value: "us-central1" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    await waitFor(() =>
      expect(mockCommand).toHaveBeenCalledWith("save_antigravity_vertex_key", {
        input: expect.objectContaining({
          id: "vertex-1",
          apiKey: "",
          location: "us-central1",
          activate: true,
        }),
      }),
    );
    await waitFor(() =>
      expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
    );
  });

  it("keeps newer events when the initial state read finishes late", async () => {
    let finish!: (state: AntigravityAuthState) => void;
    mockCommand.mockReturnValue(
      new Promise<AntigravityAuthState>((resolve) => {
        finish = resolve;
      }),
    );
    render(<AntigravityAccounts notify={vi.fn()} setError={vi.fn()} />);
    act(() =>
      listeners.get("antigravity-auth-state")?.({
        ...initialState,
        accounts: [{ ...initialState.accounts[0], name: "最新账号" }],
      }),
    );
    await act(async () => {
      finish(initialState);
    });
    expect(screen.getByText("最新账号")).toBeInTheDocument();
    expect(screen.queryByText("OAuth 主号")).not.toBeInTheDocument();
  });

  it("disposes event registration that finishes after unmount", async () => {
    let registered!: (clean: () => void) => void;
    vi.mocked(subscribe).mockImplementationOnce(
      () =>
        new Promise((resolve) => {
          registered = resolve;
        }),
    );
    mockCommand.mockResolvedValue(initialState);
    const view = render(
      <AntigravityAccounts notify={vi.fn()} setError={vi.fn()} />,
    );
    await screen.findByText("OAuth 主号");
    view.unmount();
    const clean = vi.fn();
    await act(async () => {
      registered(clean);
    });
    expect(clean).toHaveBeenCalledTimes(1);
  });
});
