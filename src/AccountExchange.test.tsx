import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import type { ViewState } from "./types";
import type { PreparedTemplate } from "./TemplateChoice";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));

import AccountExchange from "./AccountExchange";

const importPreview = {
  token: "fixture-import-token",
  revision: "fixture-import-revision",
  rows: [
    {
      id: 0,
      name: "Fixture new Codex",
      format: "codex",
      identityHint: "fixture-codex-identity",
      duplicate: false,
      error: null,
    },
    {
      id: 1,
      name: "Fixture existing Sub2API",
      format: "sub2api",
      identityHint: "fixture-existing-identity",
      duplicate: true,
      error: null,
    },
    {
      id: 2,
      name: "Fixture missing credentials",
      format: "cliproxyapi",
      identityHint: null,
      duplicate: false,
      error: "缺少完整 OAuth 凭据",
    },
    {
      id: 3,
      name: "Fixture new CLIProxyAPI",
      format: "cliproxyapi",
      identityHint: "fixture-cli-identity",
      duplicate: false,
      error: null,
    },
  ],
};

const exportPreview = {
  revision: "fixture-export-revision",
  rows: [
    { id: "fixture-account-a", name: "Fixture saved A", error: null },
    { id: "fixture-account-b", name: "Fixture saved B", error: null },
    {
      id: "fixture-account-invalid",
      name: "Fixture invalid saved account",
      error: "缺少完整 OAuth 凭据",
    },
  ],
};

const template: PreparedTemplate = {
  ready: false,
  token: "fixture-oauth-template-token",
  rows: [
    { id: 2, name: "Fixture OAuth prototype A" },
    { id: 5, name: "Fixture OAuth prototype B" },
  ],
};
const readyTemplate: PreparedTemplate = { ready: true, token: null, rows: [] };

const savedState: ViewState = {
  accounts: [],
  authRevision: "fixture-auth-revision",
  configRevision: "fixture-config-revision",
  currentState: "missing",
  preferences: { codexHome: "/fixture/codex", cliPath: "", theme: "system" },
  authSource: {
    provider: "openai",
    credentialStore: "file",
    inlineToken: false,
    envKey: false,
    commandAuth: false,
    requiresOpenaiAuth: true,
    warning: null,
  },
  error: null,
};

async function defaultCommand(name: string) {
  switch (name) {
    case "preview_account_import":
      return structuredClone(importPreview);
    case "preview_account_export":
      return structuredClone(exportPreview);
    case "commit_account_import":
      return structuredClone(savedState);
    case "prepare_account_template":
      return structuredClone(readyTemplate);
    case "select_account_template":
    case "cancel_account_exchange":
      return null;
    case "export_accounts":
      return true;
    default:
      throw new Error(`Unexpected fixture command: ${name}`);
  }
}

beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(defaultCommand);
});

function mount(mode: "import" | "export") {
  const close = vi.fn();
  const update = vi.fn();
  const notify = vi.fn();
  const view = render(
    <AccountExchange
      mode={mode}
      close={close}
      update={update}
      notify={notify}
    />,
  );
  return { ...view, close, update, notify };
}

function calls(name: string) {
  return mock.command.mock.calls.filter(([command]) => command === name);
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => {
    resolve = done;
  });
  return { promise, resolve };
}

it("disables missing credentials, skips duplicates by default, and explicitly updates one selected duplicate", async () => {
  const user = userEvent.setup();
  const view = mount("import");
  const dialog = await screen.findByRole("dialog", { name: "导入账号" });
  const first = await within(dialog).findByRole("checkbox", {
    name: /Fixture new Codex/,
  });
  const duplicate = within(dialog).getByRole("checkbox", {
    name: /Fixture existing Sub2API/,
  });
  const missing = within(dialog).getByRole("checkbox", {
    name: /Fixture missing credentials/,
  });
  const second = within(dialog).getByRole("checkbox", {
    name: /Fixture new CLIProxyAPI/,
  });
  expect(first).toBeChecked();
  expect(second).toBeChecked();
  expect(duplicate).not.toBeChecked();
  expect(missing).toBeDisabled();
  expect(missing).not.toBeChecked();
  expect(
    within(dialog).getByText("重复账号默认跳过，勾选后更新其凭据。"),
  ).toBeInTheDocument();

  await user.click(missing);
  await user.click(second);
  await user.click(duplicate);
  await user.click(
    within(dialog).getByRole("button", { name: "保存所选账号" }),
  );

  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(calls("commit_account_import")).toEqual([
    [
      "commit_account_import",
      {
        token: importPreview.token,
        expectedRevision: importPreview.revision,
        selected: [
          { id: 0, update: false },
          { id: 1, update: true },
        ],
      },
    ],
  ]);
  expect(view.update).toHaveBeenCalledWith(savedState);
  expect(view.notify).toHaveBeenCalledWith("已保存 2 个账号");
  view.unmount();
  expect(calls("cancel_account_exchange")).toHaveLength(0);
});

it("selects only new valid accounts when select-all is used after an individual duplicate selection", async () => {
  const user = userEvent.setup();
  mount("import");
  await screen.findByRole("checkbox", { name: /Fixture new Codex/ });
  const all = screen.getByRole("checkbox", { name: /全选新账号/ });
  const duplicate = screen.getByRole("checkbox", {
    name: /Fixture existing Sub2API/,
  });
  expect(all).toBeChecked();
  await user.click(all);
  expect(screen.getByRole("button", { name: "保存所选账号" })).toBeDisabled();
  await user.click(duplicate);
  expect(duplicate).toBeChecked();
  expect(all).not.toBeChecked();
  await user.click(all);
  expect(all).toBeChecked();
  expect(duplicate).not.toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: /Fixture missing credentials/ }),
  ).not.toBeChecked();
  await user.click(screen.getByRole("button", { name: "保存所选账号" }));
  await waitFor(() => expect(calls("commit_account_import")).toHaveLength(1));
  expect(calls("commit_account_import")[0][1]).toEqual({
    token: importPreview.token,
    expectedRevision: importPreview.revision,
    selected: [
      { id: 0, update: false },
      { id: 3, update: false },
    ],
  });
});

it("does not submit after the import picker is cancelled and allows another file selection", async () => {
  const user = userEvent.setup();
  mock.command.mockImplementationOnce(async () => null);
  const view = mount("import");
  const choose = await screen.findByRole("button", { name: "选择文件" });
  await waitFor(() => expect(choose).toBeEnabled());
  expect(screen.getByRole("button", { name: "保存所选账号" })).toBeDisabled();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(calls("commit_account_import")).toHaveLength(0);
  expect(view.close).not.toHaveBeenCalled();

  await user.click(choose);
  expect(
    await screen.findByRole("checkbox", { name: /Fixture new Codex/ }),
  ).toBeChecked();
  expect(calls("preview_account_import")).toHaveLength(2);
});

it("cancels an uncommitted preview when the parent closes the dialog", async () => {
  const user = userEvent.setup();
  const view = mount("import");
  await screen.findByRole("checkbox", { name: /Fixture new Codex/ });
  await user.click(screen.getByRole("button", { name: "取消" }));
  expect(view.close).toHaveBeenCalledOnce();
  view.unmount();
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: importPreview.token }],
  ]);
  expect(calls("commit_account_import")).toHaveLength(0);
  expect(view.update).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();
});

it("releases an import token returned after the dialog has unmounted", async () => {
  const pending = deferred<typeof importPreview>();
  mock.command.mockImplementationOnce(() => pending.promise);
  const view = mount("import");
  expect(screen.getByRole("dialog", { name: "导入账号" })).toHaveAttribute(
    "aria-busy",
    "true",
  );
  view.unmount();
  await act(async () => pending.resolve(structuredClone(importPreview)));
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: importPreview.token }],
  ]);
  expect(view.update).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();
  expect(view.close).not.toHaveBeenCalled();
});

it("uses a newly chosen preview and its duplicate classification after an import conflict", async () => {
  const user = userEvent.setup();
  const latest = {
    ...structuredClone(importPreview),
    token: "fixture-repreview-token",
    revision: "fixture-repreview-revision",
  };
  latest.rows[0].duplicate = true;
  let previews = 0;
  let commits = 0;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_account_import")
      return structuredClone(previews++ ? latest : importPreview);
    if (name === "commit_account_import" && commits++ === 0)
      throw {
        code: "CONFLICT",
        message: "账号库或凭据已变化，请重新预览后选择",
      };
    return defaultCommand(name);
  });
  const view = mount("import");
  await screen.findByRole("checkbox", { name: /Fixture new Codex/ });
  await user.click(screen.getByRole("button", { name: "保存所选账号" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "请重新预览后选择",
  );
  expect(view.close).not.toHaveBeenCalled();
  expect(view.update).not.toHaveBeenCalled();
  expect(screen.getByRole("button", { name: "保存所选账号" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "保存所选账号" }));
  expect(calls("commit_account_import")).toHaveLength(1);

  await user.click(screen.getByRole("button", { name: "选择文件" }));
  await waitFor(() =>
    expect(
      screen.getByRole("checkbox", { name: /Fixture new Codex/ }),
    ).not.toBeChecked(),
  );
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "保存所选账号" })).toBeEnabled();
  await user.click(screen.getByRole("button", { name: "保存所选账号" }));
  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(calls("commit_account_import")[1]).toEqual([
    "commit_account_import",
    {
      token: latest.token,
      expectedRevision: latest.revision,
      selected: [{ id: 3, update: false }],
    },
  ]);
});

it("selects a Sub2API OAuth prototype before exporting multiple valid accounts with metadata-only IPC", async () => {
  const user = userEvent.setup();
  let prepared = false;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "prepare_account_template")
      return structuredClone(prepared ? readyTemplate : template);
    if (name === "select_account_template") prepared = true;
    return defaultCommand(name);
  });
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  expect(screen.getByRole("combobox", { name: /导出格式/ })).toHaveValue(
    "sub2api",
  );
  expect(
    screen.getByRole("checkbox", { name: /Fixture invalid saved account/ }),
  ).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  const choice = await screen.findByRole("group", {
    name: "选择一个账号作为导出原型",
  });
  expect(screen.getByRole("button", { name: "导出所选" })).toBeDisabled();
  expect(calls("export_accounts")).toHaveLength(0);
  await user.click(
    within(choice).getByRole("radio", { name: "Fixture OAuth prototype B" }),
  );
  await user.click(within(choice).getByRole("button", { name: "使用此原型" }));
  await waitFor(() =>
    expect(screen.queryByRole("group")).not.toBeInTheDocument(),
  );
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved A/ }),
  ).toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved B/ }),
  ).toBeChecked();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(mock.command.mock.calls).toEqual([
    ["preview_account_export"],
    [
      "prepare_account_template",
      {
        ids: ["fixture-account-a", "fixture-account-b"],
        replaceTemplate: false,
      },
    ],
    ["select_account_template", { token: template.token, id: 5 }],
    [
      "prepare_account_template",
      {
        ids: ["fixture-account-a", "fixture-account-b"],
        replaceTemplate: false,
      },
    ],
    [
      "export_accounts",
      {
        ids: ["fixture-account-a", "fixture-account-b"],
        expectedRevision: exportPreview.revision,
        format: "sub2api",
      },
    ],
  ]);
  expect(view.notify).toHaveBeenCalledWith("已导出 2 个账号");
  expect(view.update).not.toHaveBeenCalled();
  view.unmount();
  expect(calls("cancel_account_exchange")).toHaveLength(0);
});

it("exports the selected CLIProxyAPI subset without preparing an OAuth template", async () => {
  const user = userEvent.setup();
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.selectOptions(
    screen.getByRole("combobox", { name: /导出格式/ }),
    "cliproxyapi",
  );
  await user.click(screen.getByRole("checkbox", { name: /Fixture saved B/ }));
  expect(
    screen.queryByRole("button", { name: "更换 OAuth 模板" }),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(mock.command.mock.calls).toEqual([
    ["preview_account_export"],
    [
      "export_accounts",
      {
        ids: ["fixture-account-a"],
        expectedRevision: exportPreview.revision,
        format: "cliproxyapi",
      },
    ],
  ]);
  expect(view.notify).toHaveBeenCalledWith("已导出 1 个账号");
});

it("cancels an unselected OAuth prototype before switching to CLIProxyAPI and exports the selected accounts", async () => {
  const user = userEvent.setup();
  const cancellation = deferred<null>();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "prepare_account_template") return structuredClone(template);
    if (name === "cancel_account_exchange") return cancellation.promise;
    return defaultCommand(name);
  });
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("checkbox", { name: /Fixture saved B/ }));
  await user.click(screen.getByRole("button", { name: "更换 OAuth 模板" }));
  const choice = await screen.findByRole("group", {
    name: "选择一个账号作为导出原型",
  });
  for (const prototype of within(choice).getAllByRole("radio"))
    expect(prototype).not.toBeChecked();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeDisabled();

  const format = screen.getByRole("combobox", { name: /导出格式/ });
  await user.selectOptions(format, "cliproxyapi");
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: template.token }],
  ]);
  expect(format).toBeDisabled();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeDisabled();
  expect(calls("export_accounts")).toHaveLength(0);

  await act(async () => cancellation.resolve(null));
  expect(format).toHaveValue("cliproxyapi");
  expect(format).toBeEnabled();
  expect(screen.queryByRole("group")).not.toBeInTheDocument();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved A/ }),
  ).toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved B/ }),
  ).not.toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: /Fixture invalid saved account/ }),
  ).toBeDisabled();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeEnabled();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(calls("export_accounts")).toEqual([
    [
      "export_accounts",
      {
        ids: ["fixture-account-a"],
        expectedRevision: exportPreview.revision,
        format: "cliproxyapi",
      },
    ],
  ]);
  expect(calls("prepare_account_template")).toHaveLength(1);
  expect(calls("select_account_template")).toHaveLength(0);
  expect(view.notify).toHaveBeenCalledWith("已导出 1 个账号");
  view.unmount();
  expect(calls("cancel_account_exchange")).toHaveLength(1);
});

it("retains the OAuth prototype when cancellation fails and allows retrying the CLIProxyAPI switch", async () => {
  const user = userEvent.setup();
  let cancellations = 0;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "prepare_account_template") return structuredClone(template);
    if (name === "cancel_account_exchange" && cancellations++ === 0)
      throw { code: "IO", message: "Fixture template cancellation failed" };
    return defaultCommand(name);
  });
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  const choice = await screen.findByRole("group", {
    name: "选择一个账号作为导出原型",
  });
  const format = screen.getByRole("combobox", { name: /导出格式/ });
  await user.selectOptions(format, "cliproxyapi");
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Fixture template cancellation failed",
  );
  expect(format).toHaveValue("sub2api");
  expect(format).toBeEnabled();
  expect(choice).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeDisabled();
  expect(calls("export_accounts")).toHaveLength(0);
  expect(view.close).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();

  await user.selectOptions(format, "cliproxyapi");
  await waitFor(() => expect(format).toHaveValue("cliproxyapi"));
  expect(screen.queryByRole("group")).not.toBeInTheDocument();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved A/ }),
  ).toBeChecked();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved B/ }),
  ).toBeChecked();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeEnabled();
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: template.token }],
    ["cancel_account_exchange", { token: template.token }],
  ]);
  expect(calls("select_account_template")).toHaveLength(0);
  view.unmount();
  expect(calls("cancel_account_exchange")).toHaveLength(2);
});

it("keeps the dialog open without export when the OAuth template picker is cancelled", async () => {
  const user = userEvent.setup();
  mock.command.mockImplementation(async (name: string) =>
    name === "prepare_account_template" ? null : defaultCommand(name),
  );
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  expect(calls("prepare_account_template")).toHaveLength(1);
  expect(calls("export_accounts")).toHaveLength(0);
  expect(screen.getByRole("button", { name: "导出所选" })).toBeEnabled();
  expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(view.close).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();
});

it("does not report a successful export when the destination picker is cancelled", async () => {
  const user = userEvent.setup();
  mock.command.mockImplementation(async (name: string) =>
    name === "export_accounts" ? false : defaultCommand(name),
  );
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  expect(calls("export_accounts")).toHaveLength(1);
  expect(view.close).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved A/ }),
  ).toBeChecked();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeEnabled();
});

it("replaces the OAuth template explicitly and cleans up its unselected token on close", async () => {
  const user = userEvent.setup();
  mock.command.mockImplementation(async (name: string) =>
    name === "prepare_account_template"
      ? structuredClone(template)
      : defaultCommand(name),
  );
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("checkbox", { name: /Fixture saved B/ }));
  await user.click(screen.getByRole("button", { name: "更换 OAuth 模板" }));
  await screen.findByRole("group", { name: "选择一个账号作为导出原型" });
  expect(calls("prepare_account_template")).toEqual([
    [
      "prepare_account_template",
      { ids: ["fixture-account-a"], replaceTemplate: true },
    ],
  ]);
  await user.click(screen.getByRole("button", { name: "取消" }));
  expect(view.close).toHaveBeenCalledOnce();
  view.unmount();
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: template.token }],
  ]);
  expect(calls("export_accounts")).toHaveLength(0);
});

it("releases a template token returned after unmount without starting an export", async () => {
  const user = userEvent.setup();
  const pending = deferred<PreparedTemplate>();
  mock.command.mockImplementation(async (name: string) =>
    name === "prepare_account_template"
      ? pending.promise
      : defaultCommand(name),
  );
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  expect(screen.getByRole("button", { name: "取消" })).toBeDisabled();
  view.unmount();
  await act(async () => pending.resolve(structuredClone(template)));
  expect(calls("cancel_account_exchange")).toEqual([
    ["cancel_account_exchange", { token: template.token }],
  ]);
  expect(calls("export_accounts")).toHaveLength(0);
  expect(view.close).not.toHaveBeenCalled();
  expect(view.notify).not.toHaveBeenCalled();
});

it("refreshes export eligibility and revision after a conflict before exporting again", async () => {
  const user = userEvent.setup();
  const latest = {
    revision: "fixture-new-export-revision",
    rows: [
      exportPreview.rows[0],
      { ...exportPreview.rows[1], error: "缺少完整 OAuth 凭据" },
    ],
  };
  let previews = 0;
  let exports = 0;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "preview_account_export")
      return structuredClone(previews++ ? latest : exportPreview);
    if (name === "export_accounts" && exports++ === 0)
      throw {
        code: "CONFLICT",
        message: "账号库或凭据已变化，请重新预览后选择",
      };
    return defaultCommand(name);
  });
  const view = mount("export");
  await screen.findByRole("checkbox", { name: /Fixture saved A/ });
  await user.selectOptions(
    screen.getByRole("combobox", { name: /导出格式/ }),
    "cliproxyapi",
  );
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "请重新预览后选择",
  );
  expect(view.notify).not.toHaveBeenCalled();
  expect(view.close).not.toHaveBeenCalled();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  expect(calls("export_accounts")).toHaveLength(1);
  await user.click(screen.getByRole("button", { name: "刷新账号列表" }));
  await waitFor(() =>
    expect(
      screen.getByRole("checkbox", { name: /Fixture saved B/ }),
    ).toBeDisabled(),
  );
  expect(
    screen.getByRole("checkbox", { name: /Fixture saved B/ }),
  ).not.toBeChecked();
  expect(screen.getByRole("button", { name: "导出所选" })).toBeEnabled();
  await user.click(screen.getByRole("button", { name: "导出所选" }));
  await waitFor(() => expect(view.close).toHaveBeenCalledOnce());
  expect(calls("export_accounts")[1]).toEqual([
    "export_accounts",
    {
      ids: ["fixture-account-a"],
      expectedRevision: latest.revision,
      format: "cliproxyapi",
    },
  ]);
});
