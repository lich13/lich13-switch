import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import type { GatewayModels as View } from "./bridge-contracts";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import GatewayModels from "./GatewayModels";

const initial: View = {
  clientId: "dsh",
  passthrough: false,
  hint: "同步后写入 profile",
  models: [],
  providers: [
    {
      id: "fixture",
      name: "Fixture",
      count: 0,
      checkedAt: null,
      stale: true,
      error: null,
    },
  ],
  modelsDev: {
    updatedAt: 1,
    version: "fixture",
    providers: 2,
    models: 2,
    error: null,
  },
  error: null,
};
let view: View;
beforeEach(() => {
  view = structuredClone(initial);
  mock.command.mockReset().mockImplementation(async (name, args) => {
    if (name === "get_gateway_models" || name === "refresh_gateway_models")
      return structuredClone(view);
    if (name === "search_models_dev")
      return {
        hits: ["maker", "relay"].map((provider) => ({
          source: `${provider}/fixture-model`,
          provider,
          providerName: provider,
          model: "fixture-model",
          name: "Fixture",
          context: 100000,
          output: null,
          cost: null,
        })),
        error: null,
      };
    if (name === "edit_model_list") {
      if (args.op === "add")
        view.models.push({
          id: args.id,
          providers: [],
          manual: true,
          source: args.source ?? null,
          pinned: null,
          metadata: null,
          price: null,
        });
      if (args.op === "pin")
        view.models[0].pinned = view.models[0].source = args.source;
      if (args.op === "remove") view.models = [];
      return structuredClone(view);
    }
    throw Error("unexpected fixture command");
  });
});
afterEach(() => vi.useRealTimers());

it("keeps source-qualified search results separate and edits only on explicit actions", async () => {
  const user = userEvent.setup();
  render(<GatewayModels clientId="dsh" />);
  await user.click(await screen.findByRole("button", { name: "添加模型" }));
  await screen.findByRole("button", { name: "添加 ID" });
  await user.type(
    screen.getByPlaceholderText("直接输入模型 ID"),
    "manual-model",
  );
  await user.click(screen.getByRole("button", { name: "添加 ID" }));
  await screen.findByText("未声明支持");
  expect(mock.command).toHaveBeenCalledWith("edit_model_list", {
    clientId: "dsh",
    op: "add",
    id: "manual-model",
    source: undefined,
  });
  await user.click(screen.getByRole("button", { name: "指定来源" }));
  const actions = await screen.findAllByRole("button", { name: "使用此来源" });
  expect(actions).toHaveLength(2);
  await user.click(actions[1]);
  expect(mock.command).toHaveBeenCalledWith("edit_model_list", {
    clientId: "dsh",
    op: "pin",
    id: "manual-model",
    source: "relay/fixture-model",
  });
  await user.click(screen.getByRole("button", { name: "移除" }));
  expect(
    within(screen.getByRole("table")).queryByText("manual-model"),
  ).not.toBeInTheDocument();
});

it("debounces searches for 250ms, cancels older queries and distinguishes an unavailable index", async () => {
  render(<GatewayModels clientId="dsh" />);
  await screen.findByRole("button", { name: "添加模型" });
  vi.useFakeTimers();
  fireEvent.click(screen.getByRole("button", { name: "添加模型" }));
  fireEvent.change(screen.getByRole("textbox", { name: "搜索 models.dev" }), {
    target: { value: "first" },
  });
  await act(() => vi.advanceTimersByTimeAsync(200));
  fireEvent.change(screen.getByRole("textbox", { name: "搜索 models.dev" }), {
    target: { value: "second" },
  });
  await act(() => vi.advanceTimersByTimeAsync(249));
  expect(
    mock.command.mock.calls.some(([name]) => name === "search_models_dev"),
  ).toBe(false);
  mock.command.mockImplementation(async (name) =>
    name === "search_models_dev" ? { hits: [], error: "索引不可用" } : view,
  );
  await act(() => vi.advanceTimersByTimeAsync(1));
  expect(mock.command).toHaveBeenCalledWith("search_models_dev", {
    query: "second",
    limit: 50,
  });
  expect(screen.getByRole("alert")).toHaveTextContent("索引不可用");
});

it("keeps Codex readonly and retains visible data on a failed refresh", async () => {
  view.passthrough = true;
  view.clientId = "codex";
  render(<GatewayModels clientId="codex" />);
  await screen.findByText("0 个模型 · 1 家在队供应商");
  expect(
    screen.queryByRole("button", { name: "添加模型" }),
  ).not.toBeInTheDocument();
  mock.command.mockRejectedValue({ message: "刷新失败" });
  fireEvent.click(screen.getByRole("button", { name: "刷新全部供应商" }));
  await waitFor(() =>
    expect(screen.getByRole("alert")).toHaveTextContent("刷新失败"),
  );
  expect(screen.getByText("配置已变更，需刷新")).toBeInTheDocument();
});
