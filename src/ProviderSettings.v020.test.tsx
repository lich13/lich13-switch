import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { ModelCatalog } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));

import ProviderSettings from "./ProviderSettings";
import { gatewayDemo } from "./gateway-preview";

function catalog(models = ["fixture-listed", "fixture-other"]): ModelCatalog {
  return {
    providerId: gatewayDemo.providers[0].id,
    version: "fixture-catalog-v020",
    models,
    checkedAt: null,
    stale: false,
    error: null,
    retryAt: null,
  };
}

function props() {
  const provider = structuredClone(gatewayDemo.providers[0]);
  return {
    provider,
    runtime: provider,
    clientId: "codex" as const,
    revision: "fixture-v020-revision",
    save: vi.fn().mockResolvedValue(undefined),
    close: vi.fn(),
    onDirtyChange: vi.fn(),
  };
}

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockResolvedValue(catalog());
});

it("saves the two handoff controls with transport and model policy in one transaction", async () => {
  const user = userEvent.setup();
  const p = props();
  render(<ProviderSettings {...p} />);

  const handoff = await screen.findByRole("checkbox", { name: "压缩后接管" });
  const newThreads = screen.getByRole("checkbox", { name: "新线程接管" });
  expect(handoff).toBeChecked();
  expect(newThreads).not.toBeChecked();

  await user.click(screen.getByRole("checkbox", { name: "原生 WebSocket" }));
  await user.click(handoff);
  await user.click(newThreads);
  await user.click(screen.getByRole("checkbox", { name: "fixture-listed" }));
  await user.click(screen.getByRole("button", { name: "保存" }));

  await waitFor(() => expect(p.close).toHaveBeenCalledOnce());
  expect(p.save).toHaveBeenCalledExactlyOnceWith(
    {
      op: "policyProvider",
      id: p.provider.id,
      supportsWebsocket: false,
      handoffAfterCompaction: false,
      takeNewThreads: true,
      allowedModels: ["fixture-listed"],
    },
    "fixture-v020-revision",
  );
});

it("keeps both handoff drafts through a conflict and retries with the refreshed revision", async () => {
  const user = userEvent.setup();
  const p = props();
  p.save.mockRejectedValueOnce({ message: "网关设置已变化" });
  const { rerender } = render(<ProviderSettings {...p} />);

  await user.click(await screen.findByRole("checkbox", { name: "原生 WebSocket" }));
  await user.click(screen.getByRole("checkbox", { name: "压缩后接管" }));
  await user.click(screen.getByRole("checkbox", { name: "新线程接管" }));
  await user.click(screen.getByRole("checkbox", { name: "fixture-listed" }));

  rerender(
    <ProviderSettings
      {...p}
      runtime={{
        ...p.provider,
        supportsWebsocket: true,
        handoffAfterCompaction: true,
        takeNewThreads: false,
        allowedModels: ["fixture-other"],
      }}
      revision="fixture-external-revision"
    />,
  );
  expect(screen.getByRole("checkbox", { name: "原生 WebSocket" })).not.toBeChecked();
  expect(screen.getByRole("checkbox", { name: "压缩后接管" })).not.toBeChecked();
  expect(screen.getByRole("checkbox", { name: "新线程接管" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "fixture-listed" })).toBeChecked();

  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
  expect(p.save).toHaveBeenLastCalledWith(
    {
      op: "policyProvider",
      id: p.provider.id,
      supportsWebsocket: false,
      handoffAfterCompaction: false,
      takeNewThreads: true,
      allowedModels: ["fixture-listed"],
    },
    "fixture-v020-revision",
  );

  rerender(<ProviderSettings {...p} revision="fixture-latest-revision" />);
  await user.click(screen.getByRole("button", { name: "重试" }));
  await waitFor(() => expect(p.close).toHaveBeenCalledOnce());
  expect(p.save).toHaveBeenLastCalledWith(
    {
      op: "policyProvider",
      id: p.provider.id,
      supportsWebsocket: false,
      handoffAfterCompaction: false,
      takeNewThreads: true,
      allowedModels: ["fixture-listed"],
    },
    null,
  );
});

it("uses the migrated defaults when an older provider omits handoff fields", async () => {
  const p = props();
  delete p.provider.handoffAfterCompaction;
  delete p.provider.takeNewThreads;
  render(<ProviderSettings {...p} runtime={p.provider} />);

  expect(await screen.findByRole("checkbox", { name: "压缩后接管" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "新线程接管" })).not.toBeChecked();
});

it("does not expose either handoff control for Claude", async () => {
  const p = props();
  render(<ProviderSettings {...p} clientId="claude" />);

  expect(screen.queryByRole("checkbox", { name: "压缩后接管" })).not.toBeInTheDocument();
  expect(screen.queryByRole("checkbox", { name: "新线程接管" })).not.toBeInTheDocument();
  expect(await screen.findByRole("checkbox", { name: "fixture-listed" })).toBeEnabled();
});
