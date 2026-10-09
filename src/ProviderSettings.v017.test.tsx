import { act, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { ModelCatalog } from "./types";

const mock = vi.hoisted(() => ({ command: vi.fn(), confirm: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
vi.mock("./confirmation", () => ({ confirmAction: mock.confirm }));

import ProviderSettings from "./ProviderSettings";
import { gatewayDemo } from "./gateway-preview";

function catalog(models = ["fixture-listed", "fixture-other"]): ModelCatalog {
  return {
    providerId: gatewayDemo.providers[0].id,
    version: "fixture-catalog-version",
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
    revision: "fixture-open-revision",
    save: vi.fn().mockResolvedValue(undefined),
    close: vi.fn(),
    onDirtyChange: vi.fn(),
  };
}

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockResolvedValue(catalog());
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(false);
});

it("saves WebSocket and an exact model whitelist as one explicit provider policy", async () => {
  const user = userEvent.setup();
  const p = props();
  render(<ProviderSettings {...p} />);

  await user.click(await screen.findByRole("checkbox", { name: "fixture-listed" }));
  await user.click(screen.getByRole("checkbox", { name: "原生 WebSocket" }));
  await user.type(screen.getByRole("textbox", { name: "手动添加模型 ID" }), "fixture-custom, fixture-listed");
  await user.click(screen.getByRole("button", { name: "添加到白名单" }));

  expect(p.save).not.toHaveBeenCalled();
  expect(screen.getByRole("checkbox", { name: "fixture-custom" })).toBeChecked();
  expect(screen.getByRole("button", { name: "白名单" })).toHaveAttribute("aria-pressed", "true");
  await user.click(screen.getByRole("button", { name: "保存" }));
  await waitFor(() => expect(p.close).toHaveBeenCalledOnce());
  expect(p.save).toHaveBeenCalledExactlyOnceWith(
    { op: "policyProvider", id: p.provider.id, supportsWebsocket: false, allowedModels: ["fixture-listed", "fixture-custom"] },
    "fixture-open-revision",
  );
});

it("keeps both policy drafts through background catalog failure and a save conflict", async () => {
  const user = userEvent.setup();
  const p = props();
  p.save.mockRejectedValueOnce({ message: "网关设置已变化" });
  const { rerender } = render(<ProviderSettings {...p} />);
  await user.click(await screen.findByRole("checkbox", { name: "fixture-listed" }));
  await user.click(screen.getByRole("checkbox", { name: "原生 WebSocket" }));

  mock.command.mockRejectedValueOnce({ message: "模型列表暂不可用" });
  rerender(<ProviderSettings {...p} runtime={{ ...p.provider, activeRequests: 3, quotaVersion: "fixture-new-catalog", allowedModels: ["fixture-other"] }} revision="fixture-external-revision" />);
  expect(await screen.findByRole("status")).toHaveTextContent("模型列表暂不可用");
  expect(screen.getByRole("checkbox", { name: "fixture-listed" })).toBeChecked();
  expect(screen.getByRole("checkbox", { name: "原生 WebSocket" })).not.toBeChecked();
  await user.type(screen.getByRole("textbox", { name: "手动添加模型 ID" }), "fixture-manual");
  await user.click(screen.getByRole("button", { name: "添加到白名单" }));
  await user.click(screen.getByRole("button", { name: "保存" }));

  expect(await screen.findByRole("alert")).toHaveTextContent("网关设置已变化");
  expect(p.close).not.toHaveBeenCalled();
  const policy = { op: "policyProvider", id: p.provider.id, supportsWebsocket: false, allowedModels: ["fixture-listed", "fixture-manual"] };
  expect(p.save).toHaveBeenLastCalledWith(policy, "fixture-open-revision");
  rerender(<ProviderSettings {...p} revision="fixture-latest-revision" />);
  expect(screen.getByRole("checkbox", { name: "fixture-manual" })).toBeChecked();
  await user.click(screen.getByRole("button", { name: "重试" }));
  expect(p.save).toHaveBeenLastCalledWith(policy, null);
  await waitFor(() => expect(p.close).toHaveBeenCalledOnce());
});

it("keeps a draft when abandoning is cancelled and validates pending or wildcard models", async () => {
  const user = userEvent.setup();
  const p = props();
  render(<ProviderSettings {...p} />);
  const manual = screen.getByRole("textbox", { name: "手动添加模型 ID" });
  await user.type(manual, "fixture-*");
  await user.click(screen.getByRole("button", { name: "添加到白名单" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("不支持通配符");
  expect(manual).toHaveValue("fixture-*");
  await user.click(screen.getByRole("button", { name: "取消" }));
  expect(mock.confirm).toHaveBeenCalledOnce();
  expect(p.close).not.toHaveBeenCalled();
  expect(manual).toHaveValue("fixture-*");

  await user.clear(manual);
  await user.type(manual, "fixture-unadded");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("请先添加手填模型");
  expect(p.save).not.toHaveBeenCalled();
  await user.clear(manual);
  await user.click(screen.getByRole("button", { name: "白名单" }));
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("白名单至少选择一个模型");
  expect(p.save).not.toHaveBeenCalled();
});

it("saves Claude model restrictions without exposing a WebSocket control", async () => {
  const user = userEvent.setup();
  const p = props();
  p.provider.allowedModels = ["fixture-listed"];
  p.provider.supportsWebsocket = false;
  let finish!: () => void;
  p.save.mockImplementation(() => new Promise<void>((resolve) => { finish = resolve; }));
  render(<ProviderSettings {...p} clientId="claude" />);
  expect(screen.queryByRole("checkbox", { name: "原生 WebSocket" })).not.toBeInTheDocument();
  expect(await screen.findByRole("checkbox", { name: "fixture-listed" })).toBeChecked();
  await user.click(screen.getByRole("button", { name: "不限模型" }));
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(p.save).toHaveBeenCalledExactlyOnceWith(
    { op: "policyProvider", id: p.provider.id, supportsWebsocket: false, allowedModels: null },
    "fixture-open-revision",
  );
  expect(screen.getByRole("button", { name: "保存中…" })).toBeDisabled();
  expect(screen.getByRole("checkbox", { name: "fixture-listed" })).toBeDisabled();
  expect(screen.getByRole("textbox", { name: "手动添加模型 ID" })).toBeDisabled();
  await act(async () => finish());
  expect(p.close).toHaveBeenCalledOnce();
});
