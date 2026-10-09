import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import ProviderSettings from "./ProviderSettings";
import { gatewayDemo } from "./gateway-preview";

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockResolvedValue({
    providerId: gatewayDemo.providers[0].id,
    version: "fixture-catalog",
    models: ["fixture-model"],
    checkedAt: null,
    stale: false,
    error: null,
    retryAt: null,
  });
});

it("preserves a transport draft across events and retries a conflict with the refreshed revision", async () => {
  const user = userEvent.setup();
  const provider = structuredClone(gatewayDemo.providers[0]);
  const save = vi
    .fn()
    .mockRejectedValueOnce({ message: "网关设置已变化" })
    .mockResolvedValue(undefined);
  const dirty = vi.fn();
  const props = {
    provider,
    runtime: provider,
    clientId: "codex" as const,
    revision: "before",
    save,
    close: vi.fn(),
    onDirtyChange: dirty,
  };
  const { rerender } = render(<ProviderSettings {...props} />);
  const checkbox = screen.getByRole("checkbox", { name: "原生 WebSocket" });
  await user.click(checkbox);
  expect(save).not.toHaveBeenCalled();
  rerender(
    <ProviderSettings
      {...props}
      runtime={{ ...provider, activeRequests: 2 }}
      revision="external"
    />,
  );
  expect(checkbox).not.toBeChecked();
  expect(checkbox).toHaveFocus();
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
  expect(save).toHaveBeenLastCalledWith(
    { op: "policyProvider", id: provider.id, supportsWebsocket: false, allowedModels: null },
    "before",
  );
  expect(checkbox).not.toBeChecked();
  rerender(<ProviderSettings {...props} revision="refreshed" />);
  await user.click(screen.getByRole("button", { name: "重试" }));
  expect(save).toHaveBeenLastCalledWith(
    { op: "policyProvider", id: provider.id, supportsWebsocket: false, allowedModels: null },
    null,
  );
  rerender(
    <ProviderSettings
      {...props}
      runtime={{ ...provider, supportsWebsocket: false }}
      revision="saved"
    />,
  );
  await waitFor(() =>
    expect(screen.queryByRole("alert")).not.toBeInTheDocument(),
  );
  expect(dirty).toHaveBeenLastCalledWith(false);
});

it("blocks duplicate transport saves and keeps the draft until completion", async () => {
  const user = userEvent.setup();
  const provider = structuredClone(gatewayDemo.providers[0]);
  let resolve!: () => void;
  const save = vi.fn(
    () =>
      new Promise<void>((done) => {
        resolve = done;
      }),
  );
  render(
    <ProviderSettings
      provider={provider}
      runtime={provider}
      clientId="codex"
      revision="1"
      save={save}
      close={() => {}}
    />,
  );
  await user.click(screen.getByRole("checkbox", { name: "原生 WebSocket" }));
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(screen.getByRole("checkbox", { name: "原生 WebSocket" })).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "fixture-model" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "保存中…" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "关闭" })).toBeDisabled();
  expect(save).toHaveBeenCalledTimes(1);
  await act(async () => resolve());
});

it("does not expose transport controls for Claude", async () => {
  const provider = structuredClone(gatewayDemo.providers[0]);
  render(
    <ProviderSettings
      provider={provider}
      runtime={provider}
      clientId="claude"
      revision="1"
      save={vi.fn()}
      close={() => {}}
    />,
  );
  expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
  expect(await screen.findByRole("checkbox", { name: "fixture-model" })).toBeEnabled();
});
