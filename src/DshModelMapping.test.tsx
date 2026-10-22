import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import { grokGatewayDemo } from "./gateway-preview";

const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
vi.mock("./confirmation", () => ({ confirmAction: vi.fn(async () => true) }));
import DshModelMapping from "./DshModelMapping";

beforeEach(() =>
  mock.command
    .mockReset()
    .mockResolvedValue({ models: ["DeepSeek-V4.1-Flash", "deepseek-v4-pro"] }),
);

it("edits only this provider and preserves a deliberately blank Pro mapping", async () => {
  const save = vi.fn(async () => {}),
    close = vi.fn();
  const provider = {
    ...grokGatewayDemo.providers[0],
    id: "fixture-provider",
    dshModels: { flash: "manual-flash", pro: null },
  };
  render(
    <DshModelMapping
      provider={provider}
      revision="fixture-revision"
      save={save}
      close={close}
    />,
  );
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "刷新模型列表" })).toBeEnabled(),
  );
  expect(screen.getByLabelText("Pro 上游模型")).toHaveValue("");
  const user = userEvent.setup();
  await user.clear(screen.getByLabelText("Flash 上游模型"));
  await user.type(screen.getByLabelText("Flash 上游模型"), "custom-flash");
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(save).toHaveBeenCalledWith(
    {
      op: "dshModelsProvider",
      id: "fixture-provider",
      mapping: { flash: "custom-flash", pro: null },
    },
    "fixture-revision",
  );
  expect(close).toHaveBeenCalledOnce();
});

it("prefills only a unique model and keeps an ambiguous entry empty", async () => {
  mock.command.mockResolvedValue({
    models: ["deepseek-flash", "deepseek-v4.1-flash", "deepseek-v4-pro"],
  });
  render(
    <DshModelMapping
      provider={{ ...grokGatewayDemo.providers[0], dshModels: null }}
      revision="fixture-revision"
      save={vi.fn(async () => {})}
      close={vi.fn()}
    />,
  );
  await waitFor(() =>
    expect(screen.getByLabelText("Pro 上游模型")).toHaveValue(
      "deepseek-v4-pro",
    ),
  );
  expect(screen.getByLabelText("Flash 上游模型")).toHaveValue("");
});
