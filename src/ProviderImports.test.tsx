import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, expect, it, vi } from "vitest";
import { gatewayDemo } from "./gateway-preview";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, () => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: () => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));
import ProviderImports from "./ProviderImports";
import type { PendingImport, LinkHandlers } from "./bridge-contracts";
const SELF = "com.lich13.gpt-switch";
const OTHER = "com.ccswitch.desktop";
const apps = [
  { id: SELF, name: "lich13-switch" },
  { id: OTHER, name: "CC Switch" },
];
let handlers: LinkHandlers;
let warning: string | null;
const queued = (p: typeof first): PendingImport => ({
  id: p.id,
  provider: { clientId: "codex", name: p.name, baseUrl: p.baseUrl },
  importError: null,
  dispatchError: null,
});

const waitForPreview = async (name: string) => {
  const dialog = await screen.findByRole("dialog", { name: "导入供应商" });
  await within(dialog).findByText(name);
  return dialog;
};

const first = {
  id: "first",
  name: "首个供应商",
  baseUrl: "https://first.example.invalid/v1",
};
const second = {
  id: "second",
  name: "第二个供应商",
  baseUrl: "https://second.example.invalid/v1",
};
let pending: PendingImport[];
let revision: string;
let confirmError: unknown;
let cancelError: unknown;
let confirmWait: Promise<void> | null;

beforeEach(() => {
  pending = [queued(first), queued(second)];
  handlers = { current: SELF, apps, remembered: SELF, systemPicker: false };
  warning = null;
  revision = "before-import";
  confirmError = null;
  cancelError = null;
  confirmWait = null;
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args?: { id: string; expectedRevision?: string }) => {
      if (name === "get_link_handler_state") return structuredClone(handlers);
      if (name === "get_provider_imports") return structuredClone(pending);
      if (name === "get_gateway")
        return { ...structuredClone(gatewayDemo), revision };
      if (name === "confirm_provider_import") {
        if (confirmError) throw confirmError;
        if (confirmWait) await confirmWait;
        pending = pending.filter((item) => item.id !== args?.id);
        return { warning };
      }
      if (name === "cancel_provider_import") {
        if (cancelError) throw cancelError;
        pending = pending.filter((item) => item.id !== args?.id);
        return;
      }
      throw new Error(`Unexpected command: ${name}`);
    },
  );
});

it("waits for an existing dialog and preserves its unsaved input", async () => {
  const user = userEvent.setup();
  function ExistingDialog() {
    const [editing, setEditing] = useState(true);
    return (
      <>
        {editing && (
          <dialog open aria-label="现有编辑表单">
            <label>
              正在编辑
              <input defaultValue="原始草稿" />
            </label>
            <button onClick={() => setEditing(false)}>关闭当前表单</button>
          </dialog>
        )}
        <ProviderImports notify={() => {}} />
      </>
    );
  }
  render(<ExistingDialog />);
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("get_provider_imports"),
  );
  await user.type(screen.getByLabelText("正在编辑"), "-未保存");
  await act(async () => mock.listeners.get("provider-imports")?.());
  expect(screen.getByLabelText("正在编辑")).toHaveValue("原始草稿-未保存");
  expect(
    screen.queryByRole("heading", { name: "导入供应商" }),
  ).not.toBeInTheDocument();
  expect(mock.command).not.toHaveBeenCalledWith("get_gateway");
  await user.click(screen.getByRole("button", { name: "关闭当前表单" }));
  const dialog = await waitForPreview(first.name);
  expect(within(dialog).getByText(first.name)).toBeInTheDocument();
  expect(screen.getAllByRole("dialog")).toHaveLength(1);
  expect(screen.getByText("••••••••")).toBeInTheDocument();
  expect(
    mock.command.mock.calls.some(
      ([name]) => name === "confirm_provider_import",
    ),
  ).toBe(false);
});

it("does not duplicate a pending import when events or clicks repeat", async () => {
  pending = [queued(first), queued(first)];
  let finish!: () => void;
  confirmWait = new Promise((resolve) => {
    finish = resolve;
  });
  const notify = vi.fn();
  const user = userEvent.setup();
  render(<ProviderImports notify={notify} />);
  const dialog = await waitForPreview(first.name);
  await act(async () => {
    mock.listeners.get("provider-imports")?.();
    mock.listeners.get("provider-imports")?.();
  });
  await waitForPreview(first.name);
  expect(screen.getAllByRole("dialog")).toHaveLength(1);
  const confirm = within(dialog).getByRole("button", { name: "导入" });
  await user.click(confirm);
  expect(confirm).toBeDisabled();
  expect(within(dialog).getByRole("button", { name: "取消" })).toBeDisabled();
  await user.click(confirm);
  await act(async () => mock.listeners.get("provider-imports")?.());
  expect(
    mock.command.mock.calls.filter(
      ([name]) => name === "confirm_provider_import",
    ),
  ).toHaveLength(1);
  await act(async () => finish());
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  await act(async () => mock.listeners.get("provider-imports")?.());
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(notify).toHaveBeenCalledExactlyOnceWith("已添加供应商");
});

it("keeps the queued preview after a save conflict and retries with the refreshed revision", async () => {
  const user = userEvent.setup();
  const notify = vi.fn();
  render(<ProviderImports notify={notify} />);
  let dialog = await waitForPreview(first.name);
  revision = "after-external-change";
  confirmError = { code: "CONFLICT", message: "设置已被其他窗口修改" };
  await user.click(within(dialog).getByRole("button", { name: "导入" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "设置已被其他窗口修改",
  );
  dialog = await waitForPreview(first.name);
  expect(within(dialog).getByText(first.name)).toBeInTheDocument();
  expect(within(dialog).getByText(first.baseUrl)).toBeInTheDocument();
  expect(screen.queryByText(second.name)).not.toBeInTheDocument();
  expect(notify).not.toHaveBeenCalled();
  expect(mock.command).toHaveBeenCalledWith("confirm_provider_import", {
    id: first.id,
    appId: SELF,
    remember: false,
    expectedRevision: "before-import",
  });
  await waitFor(() =>
    expect(within(dialog).getByRole("button", { name: "导入" })).toBeEnabled(),
  );
  confirmError = null;
  await user.click(within(dialog).getByRole("button", { name: "导入" }));
  dialog = await waitForPreview(second.name);
  expect(within(dialog).getByText(second.name)).toBeInTheDocument();
  expect(mock.command).toHaveBeenCalledWith("confirm_provider_import", {
    id: first.id,
    appId: SELF,
    remember: false,
    expectedRevision: "after-external-change",
  });
  expect(notify).toHaveBeenCalledExactlyOnceWith("已添加供应商");
});

it("keeps a failed cancellation visible and advances the queue only after cancellation succeeds", async () => {
  const notify = vi.fn();
  const user = userEvent.setup();
  render(<ProviderImports notify={notify} />);
  let dialog = await waitForPreview(first.name);
  cancelError = { code: "IO", message: "无法取消待导入项" };
  await user.click(within(dialog).getByRole("button", { name: "取消" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "无法取消待导入项",
  );
  dialog = await waitForPreview(first.name);
  expect(within(dialog).getByText(first.name)).toBeInTheDocument();
  cancelError = null;
  await user.click(within(dialog).getByRole("button", { name: "取消" }));
  dialog = await waitForPreview(second.name);
  expect(within(dialog).getByText(second.name)).toBeInTheDocument();
  expect(mock.command).toHaveBeenCalledWith("cancel_provider_import", {
    id: first.id,
  });
  expect(
    mock.command.mock.calls.some(
      ([name]) => name === "confirm_provider_import",
    ),
  ).toBe(false);
  expect(notify).not.toHaveBeenCalled();
});

it("selects the receiver and imports in the same dialog, remembering only on confirmation", async () => {
  handlers.remembered = null;
  pending = [queued(first)];
  const user = userEvent.setup();
  render(<ProviderImports notify={() => {}} />);
  const dialog = await screen.findByRole("dialog", { name: "选择接收应用" });
  expect(within(dialog).getByRole("button", { name: "打开" })).toBeDisabled();
  expect(screen.queryByText(first.name)).not.toBeInTheDocument();
  await user.selectOptions(screen.getByLabelText("接收应用"), SELF);
  expect(await waitForPreview(first.name)).toBe(dialog);
  await user.click(screen.getByLabelText("下次不再提示"));
  await user.click(screen.getByRole("button", { name: "导入" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  expect(mock.command).toHaveBeenCalledWith("confirm_provider_import", {
    id: first.id,
    appId: SELF,
    remember: true,
    expectedRevision: revision,
  });
});

it("forwards an unsupported local link without requesting gateway state", async () => {
  handlers.remembered = null;
  pending = [
    {
      id: "external",
      provider: null,
      importError: "不支持本地导入",
      dispatchError: null,
    },
  ];
  const user = userEvent.setup();
  render(<ProviderImports notify={() => {}} />);
  await screen.findByRole("dialog", { name: "选择接收应用" });
  await user.selectOptions(screen.getByLabelText("接收应用"), SELF);
  expect(screen.getByRole("alert")).toHaveTextContent("不支持本地导入");
  expect(screen.getByRole("button", { name: "导入" })).toBeDisabled();
  await user.selectOptions(screen.getByLabelText("接收应用"), OTHER);
  await user.click(screen.getByRole("button", { name: "打开" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  expect(mock.command).toHaveBeenCalledWith("confirm_provider_import", {
    id: "external",
    appId: OTHER,
    remember: false,
    expectedRevision: null,
  });
  expect(mock.command.mock.calls.some(([name]) => name === "get_gateway")).toBe(
    false,
  );
});

it("returns a failed remembered receiver to the chooser and preserves the link on retry failure", async () => {
  handlers.remembered = OTHER;
  pending = [{ ...queued(first), dispatchError: "接收应用不可用，请重新选择" }];
  const user = userEvent.setup();
  render(<ProviderImports notify={() => {}} />);
  await screen.findByRole("dialog", { name: "选择接收应用" });
  expect(screen.getByRole("alert")).toHaveTextContent("接收应用不可用");
  await user.selectOptions(screen.getByLabelText("接收应用"), OTHER);
  confirmError = { code: "IMPORT_LINK", message: "打开失败" };
  await user.click(screen.getByRole("button", { name: "打开" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("打开失败");
  expect(pending).toHaveLength(1);
  confirmError = null;
  await user.selectOptions(screen.getByLabelText("接收应用"), SELF);
  await user.click(await screen.findByRole("button", { name: "导入" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
});

it("reports a preference failure after delivery without offering to deliver the same link again", async () => {
  pending = [queued(first)];
  warning = "本次处理已完成，但未能记住接收应用，请在设置中重试";
  const notify = vi.fn();
  render(<ProviderImports notify={notify} />);
  await waitForPreview(first.name);
  await userEvent.click(screen.getByRole("button", { name: "导入" }));
  await waitFor(() =>
    expect(screen.queryByRole("dialog")).not.toBeInTheDocument(),
  );
  expect(notify).toHaveBeenCalledExactlyOnceWith(warning);
});
