import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { GatewayState } from "./types";
import { gatewayDemo } from "./gateway-preview";
import type { PreparedTemplate } from "./TemplateChoice";

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
    if (name === "prepare_sub2api_export")
      return { ready: true, token: null, rows: [] };
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

const pendingTemplate: PreparedTemplate = {
  ready: false,
  token: "fixture-provider-template-token",
  rows: [
    { id: 0, name: "Fixture provider prototype A" },
    { id: 4, name: "Fixture provider prototype B" },
  ],
};

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

it.each(["import", "export"] as const)(
  "requires a single explicit prototype before %s and keeps provider multi-selection",
  async (mode) => {
    const user = userEvent.setup();
    const fallback = mock.command.getMockImplementation()!;
    mock.command.mockImplementation(
      async (name: string, args?: Record<string, unknown>) => {
        if (name === "preview_sub2api_import")
          return structuredClone({ ...preview, template: pendingTemplate });
        if (name === "prepare_sub2api_export")
          return structuredClone(pendingTemplate);
        if (name === "select_sub2api_template") return null;
        return fallback(name, args);
      },
    );
    const close = vi.fn();
    const current = state();
    render(
      <ProviderExchange
        mode={mode}
        state={current}
        clientId="codex"
        close={close}
        update={vi.fn()}
        notify={vi.fn()}
      />,
    );
    const choice = await screen.findByRole("group", {
      name: "选择一个账号作为导出原型",
    });
    const submitName = mode === "import" ? "导入所选" : "导出所选";
    expect(screen.getByRole("button", { name: submitName })).toBeDisabled();
    expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
    await user.click(
      within(choice).getByRole("radio", {
        name: "Fixture provider prototype B",
      }),
    );
    await user.click(
      within(choice).getByRole("button", { name: "使用此原型" }),
    );
    const all = await screen.findByRole("checkbox", { name: /全选/ });
    expect(all).toBeChecked();
    expect(mock.command).toHaveBeenCalledWith("select_sub2api_template", {
      clientId: "codex",
      token: pendingTemplate.token,
      id: 4,
    });
    const checkboxes = screen.getAllByRole("checkbox");
    expect(
      checkboxes.filter((checkbox) => (checkbox as HTMLInputElement).checked),
    ).toHaveLength(3);
    await user.click(checkboxes[mode === "import" ? 3 : 2]);
    await user.click(screen.getByRole("button", { name: submitName }));
    await waitFor(() => expect(close).toHaveBeenCalledOnce());
    expect(mock.command).toHaveBeenCalledWith(
      mode === "import" ? "commit_sub2api_import" : "export_sub2api_providers",
      mode === "import"
        ? {
            clientId: "codex",
            token: preview.token,
            ids: [0],
            expectedRevision: preview.revision,
          }
        : {
            clientId: "codex",
            ids: [current.providers[0].id],
            expectedRevision: current.revision,
          },
    );
  },
);

it.each(["initial", "replacement"] as const)(
  "releases a late %s export template token after unmount",
  async (stage) => {
    const user = userEvent.setup();
    const pending = deferred<PreparedTemplate>();
    const fallback = mock.command.getMockImplementation()!;
    let preparations = 0;
    mock.command.mockImplementation(
      async (name: string, args?: Record<string, unknown>) => {
        if (name === "prepare_sub2api_export") {
          preparations++;
          if (stage === "initial" || preparations === 2) return pending.promise;
        }
        return fallback(name, args);
      },
    );
    const close = vi.fn();
    const notify = vi.fn();
    const { unmount } = render(
      <ProviderExchange
        mode="export"
        state={state()}
        clientId="claude"
        close={close}
        update={vi.fn()}
        notify={notify}
      />,
    );
    if (stage === "replacement") {
      await screen.findByRole("checkbox", { name: /全选/ });
      await user.click(screen.getByRole("button", { name: "更换模板" }));
      expect(mock.command).toHaveBeenCalledWith("prepare_sub2api_export", {
        clientId: "claude",
        replaceTemplate: true,
      });
    }
    unmount();
    await act(async () => pending.resolve(structuredClone(pendingTemplate)));
    expect(
      mock.command.mock.calls.filter(
        ([name]) => name === "cancel_sub2api_import",
      ),
    ).toEqual([
      [
        "cancel_sub2api_import",
        { clientId: "claude", token: pendingTemplate.token },
      ],
    ]);
    expect(
      mock.command.mock.calls.filter(
        ([name]) => name === "export_sub2api_providers",
      ),
    ).toHaveLength(0);
    expect(close).not.toHaveBeenCalled();
    expect(notify).not.toHaveBeenCalled();
  },
);

it("releases both late import and prototype tokens when the dialog has already unmounted", async () => {
  const next = { ...preview, template: pendingTemplate };
  const pending = deferred<typeof next>();
  mock.command.mockImplementationOnce(() => pending.promise);
  const { unmount } = render(
    <ProviderExchange
      mode="import"
      state={state()}
      clientId="codex"
      close={vi.fn()}
      update={vi.fn()}
      notify={vi.fn()}
    />,
  );
  unmount();
  await act(async () => pending.resolve(structuredClone(next)));
  const cancelled = mock.command.mock.calls.filter(
    ([name]) => name === "cancel_sub2api_import",
  );
  expect(cancelled).toHaveLength(2);
  expect(cancelled).toEqual(expect.arrayContaining([
    ["cancel_sub2api_import", { clientId: "codex", token: preview.token }],
    [
      "cancel_sub2api_import",
      { clientId: "codex", token: pendingTemplate.token },
    ],
  ]));
});
