import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import { grokGatewayDemo } from "./gateway-preview";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
vi.mock("./confirmation", () => ({ confirmAction: vi.fn(async () => true) }));
import DshModelMapping from "./DshModelMapping";
let official: { id: string; name: string }[];
let catalogError: string | null;
beforeEach(() => {
  official = [
    { id: "deepseek-flash", name: "Flash" },
    { id: "deepseek-v4-pro", name: "Pro" },
  ];
  catalogError = null;
  mock.command
    .mockReset()
    .mockImplementation(async (name) =>
      name === "dsh_official_models"
        ? { models: official, error: catalogError }
        : { models: ["DeepSeek-V4.1-Flash", "deepseek-v4-pro"], error: null },
    );
});
function mount(
  mapping: Record<string, string | null> = {
    "deepseek-flash": "manual-flash",
    "deepseek-v4-pro": null,
  },
) {
  const save = vi.fn(async () => {}),
    close = vi.fn();
  render(
    <DshModelMapping
      provider={{
        ...grokGatewayDemo.providers[0],
        id: "fixture-provider",
        dshModels: mapping,
      }}
      revision="fixture-revision"
      save={save}
      close={close}
    />,
  );
  return { save, close, user: userEvent.setup() };
}
it("uses dropdowns, keeps saved values absent from discovery, and saves only this provider", async () => {
  const { save, close, user } = mount();
  const select = await screen.findByRole("combobox", {
    name: "Flash 上游模型",
  });
  expect(select).toHaveValue("manual-flash");
  expect(screen.getByLabelText("Pro 上游模型")).toHaveValue("");
  await user.selectOptions(select, "DeepSeek-V4.1-Flash");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(save).toHaveBeenCalledWith(
    {
      op: "dshModelsProvider",
      id: "fixture-provider",
      mapping: {
        "deepseek-flash": "DeepSeek-V4.1-Flash",
        "deepseek-v4-pro": null,
      },
    },
    "fixture-revision",
  );
  expect(close).toHaveBeenCalledOnce();
});
it("automatically refreshes the official catalog and keeps new IDs unassigned", async () => {
  official.push({ id: "deepseek-future", name: "Future" });
  mount();
  expect(await screen.findByLabelText("Future 上游模型")).toHaveValue("");
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("dsh_official_models", {
      refresh: true,
    }),
  );
  expect(screen.getByLabelText("Flash 上游模型")).toHaveValue("manual-flash");
});
it("keeps a manual draft and explicit empty mapping after a failed refresh", async () => {
  const { save, user } = mount();
  const select = await screen.findByLabelText("Flash 上游模型");
  await user.selectOptions(select, "__manual_model__");
  await user.clear(screen.getByLabelText("Flash 自定义模型 ID"));
  await user.type(
    screen.getByLabelText("Flash 自定义模型 ID"),
    "custom-future",
  );
  catalogError = "官方目录暂时不可用";
  await user.click(screen.getByRole("button", { name: "刷新官方模型" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(catalogError);
  expect(screen.getByLabelText("Flash 自定义模型 ID")).toHaveValue(
    "custom-future",
  );
  expect(screen.getByLabelText("Pro 上游模型")).toHaveValue("");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(save).toHaveBeenCalledWith(
    expect.objectContaining({
      mapping: {
        "deepseek-flash": "custom-future",
        "deepseek-v4-pro": null,
      },
    }),
    "fixture-revision",
  );
});
