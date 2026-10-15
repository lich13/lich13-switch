import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, expect, it, vi } from "vitest";
import type { GrokImportPreview } from "./bridge-contracts";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));

import GrokImport from "./GrokImport";
import { grokGatewayDemo } from "./gateway-preview";

const importToken = "fixture-grok-import-transaction";
const missingCredentialPreview: GrokImportPreview = {
  token: null,
  revision: "fixture-grok-revision",
  selected: "fixture-model",
  models: [{
    key: "fixture-model",
    model: "fixture-model",
    protocol: "responses",
    address: "https://api.example.invalid/v1",
    addressSource: "modelApi",
    credentialAvailable: false,
    missingEnvironment: ["XAI_API_KEY"],
    error: null,
  }],
  environmentAvailable: true,
  environmentLoaded: false,
  clearEnvironment: ["GROK_MODELS_BASE_URL", "XAI_API_KEY"],
  error: "未读取到 API 凭据；可确认读取 endpoint.env",
};
const credentialPreview: GrokImportPreview = {
  ...missingCredentialPreview,
  token: importToken,
  environmentLoaded: true,
  models: [{
    ...missingCredentialPreview.models[0],
    credentialAvailable: true,
    missingEnvironment: [],
  }],
  error: null,
};
const credentialPreviewWithoutEnvironment: GrokImportPreview = {
  ...credentialPreview,
  environmentLoaded: false,
};

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_grok_import") return structuredClone(missingCredentialPreview);
    if (name === "cancel_grok_import") return;
    if (name === "commit_grok_import") return structuredClone(grokGatewayDemo);
    throw new Error(`Unexpected fixture command: ${name}`);
  });
});

it("previews without reading endpoint.env by default and never displays its token", async () => {
  render(<GrokImport close={vi.fn()} update={vi.fn()} notify={vi.fn()} />);
  const dialog = await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });

  await within(dialog).findByRole("checkbox", { name: "读取 endpoint.env 中的简单赋值" });
  expect(mock.command).toHaveBeenCalledExactlyOnceWith("preview_grok_import", {
    selected: undefined,
    useEnvironment: false,
  });
  expect(within(dialog).getByRole("checkbox", { name: "读取 endpoint.env 中的简单赋值" })).not.toBeChecked();
  expect(dialog).not.toHaveTextContent(importToken);
  expect(within(dialog).getByRole("option", { name: "fixture-model" })).toBeInTheDocument();
});

it("does not allow committing a preview with missing credentials", async () => {
  const user = userEvent.setup();
  render(<GrokImport close={vi.fn()} update={vi.fn()} notify={vi.fn()} />);
  const dialog = await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });

  const confirm = await within(dialog).findByRole("button", { name: "确认导入" });
  expect(confirm).toBeDisabled();
  expect(within(dialog).getByRole("alert")).toHaveTextContent("未读取到 API 凭据");
  await user.click(confirm);
  expect(mock.command).not.toHaveBeenCalledWith("commit_grok_import", expect.anything());
});

it("imports only after the user selects endpoint.env and shows its cleanup instructions", async () => {
  mock.command.mockImplementation(async (name: string, args?: Record<string, unknown>) => {
    if (name === "preview_grok_import")
      return structuredClone(args?.useEnvironment ? credentialPreview : missingCredentialPreview);
    if (name === "commit_grok_import") return structuredClone(grokGatewayDemo);
    if (name === "cancel_grok_import") return;
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  const user = userEvent.setup();
  const update = vi.fn();
  const notify = vi.fn();
  render(<GrokImport close={vi.fn()} update={update} notify={notify} />);
  const dialog = await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });
  const loadEnvironment = await within(dialog).findByRole("checkbox", {
    name: "读取 endpoint.env 中的简单赋值",
  });
  expect(loadEnvironment).not.toBeChecked();

  await user.click(loadEnvironment);
  await waitFor(() => expect(loadEnvironment).toBeChecked());
  expect(mock.command).toHaveBeenCalledWith("preview_grok_import", {
    selected: "fixture-model",
    useEnvironment: true,
  });
  expect(dialog).not.toHaveTextContent(importToken);
  expect(within(dialog).getByText("••••••••")).toBeInTheDocument();
  await user.click(within(dialog).getByRole("button", { name: "确认导入" }));

  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("commit_grok_import", {
      token: importToken,
      expectedRevision: "fixture-grok-revision",
    }),
  );
  expect(update).toHaveBeenCalledWith(grokGatewayDemo);
  expect(notify).toHaveBeenCalledWith("Grok API 配置已导入");
  expect(within(dialog).getByText("unset GROK_MODELS_BASE_URL XAI_API_KEY")).toBeInTheDocument();
  expect(dialog).toHaveTextContent(/请新开终端，或执行以下命令后再启动 Grok：/);
  expect(within(dialog).getByText("grok login")).toBeInTheDocument();
  expect(dialog).not.toHaveTextContent(importToken);
});

it("disables confirmation after an endpoint.env preview fails instead of committing the previous preview", async () => {
  mock.command.mockImplementation(async (name: string, args?: Record<string, unknown>) => {
    if (name === "preview_grok_import") {
      if (args?.useEnvironment) throw new Error("fixture endpoint.env read failed");
      return structuredClone(credentialPreviewWithoutEnvironment);
    }
    if (name === "commit_grok_import") return structuredClone(grokGatewayDemo);
    if (name === "cancel_grok_import") return;
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  const user = userEvent.setup();
  render(<GrokImport close={vi.fn()} update={vi.fn()} notify={vi.fn()} />);
  const dialog = await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });
  const loadEnvironment = await within(dialog).findByRole("checkbox", {
    name: "读取 endpoint.env 中的简单赋值",
  });
  const confirm = within(dialog).getByRole("button", { name: "确认导入" });

  await waitFor(() => expect(confirm).toBeEnabled());
  await user.click(loadEnvironment);
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("preview_grok_import", {
      selected: "fixture-model",
      useEnvironment: true,
    }),
  );
  expect(await within(dialog).findByRole("alert")).toHaveTextContent(
    "fixture endpoint.env read failed",
  );
  await waitFor(() => expect(confirm).toBeDisabled());

  await user.click(confirm);
  expect(mock.command).not.toHaveBeenCalledWith("commit_grok_import", expect.anything());
});

it("cancels the active preview when the user closes the dialog", async () => {
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_grok_import") return structuredClone(credentialPreview);
    if (name === "cancel_grok_import") return;
    throw new Error(`Unexpected fixture command: ${name}`);
  });
  function Harness() {
    const [open, setOpen] = useState(true);
    return open ? (
      <GrokImport close={() => setOpen(false)} update={vi.fn()} notify={vi.fn()} />
    ) : null;
  }
  const user = userEvent.setup();
  render(<Harness />);
  const dialog = await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });
  await within(dialog).findByRole("button", { name: "取消" });
  await user.click(within(dialog).getByRole("button", { name: "取消" }));

  await waitFor(() => {
    expect(screen.queryByRole("dialog", { name: "导入 Grok 当前配置" })).not.toBeInTheDocument();
    expect(mock.command).toHaveBeenCalledWith("cancel_grok_import", { token: importToken });
  });
});

it("cancels a preview token that arrives after the dialog unmounts", async () => {
  let resolvePreview!: (value: GrokImportPreview) => void;
  const pendingPreview = new Promise<GrokImportPreview>((resolve) => {
    resolvePreview = resolve;
  });
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_grok_import") return pendingPreview;
    if (name === "cancel_grok_import") return;
    throw new Error(`Unexpected fixture command: ${name}`);
  });

  const { unmount } = render(
    <GrokImport close={vi.fn()} update={vi.fn()} notify={vi.fn()} />,
  );
  await screen.findByRole("dialog", { name: "导入 Grok 当前配置" });
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("preview_grok_import", {
      selected: undefined,
      useEnvironment: false,
    }),
  );
  unmount();
  await act(async () => resolvePreview(credentialPreview));

  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("cancel_grok_import", { token: importToken }),
  );
});
