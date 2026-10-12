import { render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { GatewayState } from "./types";
import { gatewayDemo } from "./gateway-preview";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));

import ProviderExchange from "./ProviderExchange";

const preview = {
  token: "fixture-preview-token",
  revision: "fixture-revision",
  unsupportedProxies: 1,
  rows: [
    {
      id: 0,
      name: "Fixture imported provider",
      baseUrl: "https://import.example.invalid/v1",
      concurrency: 3,
      error: null,
    },
    {
      id: 1,
      name: "Fixture OAuth account",
      baseUrl: null,
      concurrency: null,
      error: "仅支持 API Key，OAuth 账号不支持",
    },
    {
      id: 2,
      name: "Fixture second provider",
      baseUrl: "https://second.example.invalid/v1",
      concurrency: 0,
      error: null,
    },
  ],
};

function state(): GatewayState {
  return structuredClone(gatewayDemo);
}

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_sub2api_import") return structuredClone(preview);
    if (name === "commit_sub2api_import") return structuredClone(gatewayDemo);
    if (name === "cancel_sub2api_import") return null;
    if (name === "prepare_sub2api_export") return true;
    if (name === "export_sub2api_providers") return true;
    throw new Error(`Unexpected fixture command: ${name}`);
  });
});

it("preselects valid imports, disables rejected rows, and commits the chosen subset", async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  const update = vi.fn();
  const notify = vi.fn();
  render(
    <ProviderExchange
      mode="import"
      state={state()}
      clientId="codex"
      close={close}
      update={update}
      notify={notify}
    />,
  );

  const dialog = await screen.findByRole("dialog", { name: "导入 Sub2API" });
  expect(within(dialog).getByRole("status")).toHaveTextContent(
    "1 条代理记录不支持导入",
  );
  const rows = within(dialog).getAllByRole("checkbox");
  expect(rows).toHaveLength(4);
  expect(rows[0]).toBeChecked();
  expect(rows[1]).toBeChecked();
  expect(rows[2]).toBeDisabled();
  expect(rows[3]).toBeChecked();

  await user.click(rows[3]);
  await user.click(within(dialog).getByRole("button", { name: "导入所选" }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(mock.command).toHaveBeenCalledWith("commit_sub2api_import", {
    clientId: "codex",
    token: "fixture-preview-token",
    ids: [0],
    expectedRevision: "fixture-revision",
  });
  expect(update).toHaveBeenCalledWith(gatewayDemo);
  expect(notify).toHaveBeenCalledWith("已导入 1 个供应商");
});

it("keeps the selected preview after a conflict and retries with the refreshed revision", async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  const update = vi.fn();
  const notify = vi.fn();
  let conflict = true;
  const latest = { ...state(), revision: "latest-revision" };
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_sub2api_import") return structuredClone(preview);
    if (name === "commit_sub2api_import" && conflict) {
      conflict = false;
      throw { code: "CONFLICT", message: "网关设置已变化，请使用最新状态重试" };
    }
    if (name === "commit_sub2api_import") return structuredClone(latest);
    if (name === "get_gateway") return structuredClone(latest);
    if (name === "cancel_sub2api_import") return null;
    throw new Error(`Unexpected fixture command: ${name}`);
  });

  render(
    <ProviderExchange
      mode="import"
      state={state()}
      clientId="codex"
      close={close}
      update={update}
      notify={notify}
    />,
  );
  const dialog = await screen.findByRole("dialog", { name: "导入 Sub2API" });
  await user.click(within(dialog).getByRole("button", { name: "导入所选" }));
  expect(await within(dialog).findByRole("alert")).toHaveTextContent(
    "网关设置已变化，请使用最新状态重试",
  );
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("get_gateway", {
      clientId: "codex",
    }),
  );
  expect(within(dialog).getByRole("checkbox", { name: /全选/ })).toBeChecked();

  await user.click(within(dialog).getByRole("button", { name: "重试" }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(mock.command).toHaveBeenLastCalledWith("commit_sub2api_import", {
    clientId: "codex",
    token: "fixture-preview-token",
    ids: [0, 2],
    expectedRevision: "latest-revision",
  });
  expect(notify).toHaveBeenCalledWith("已导入 2 个供应商");
});

it("exports only the checked provider with the authoritative revision", async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  const update = vi.fn();
  const notify = vi.fn();
  const current = state();
  render(
    <ProviderExchange
      mode="export"
      state={current}
      clientId="claude"
      close={close}
      update={update}
      notify={notify}
    />,
  );

  const dialog = await screen.findByRole("dialog", { name: "导出 Sub2API" });
  const checkboxes = within(dialog).getAllByRole("checkbox");
  expect(checkboxes).toHaveLength(current.providers.length + 1);
  expect(checkboxes[0]).toBeChecked();
  expect(checkboxes[1]).toBeChecked();
  await user.click(checkboxes[2]);
  await user.click(within(dialog).getByRole("button", { name: "导出所选" }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(mock.command).toHaveBeenCalledWith("export_sub2api_providers", {
    clientId: "claude",
    ids: [current.providers[0].id],
    expectedRevision: current.revision,
  });
  expect(notify).toHaveBeenCalledWith("已导出 1 个供应商");
});
