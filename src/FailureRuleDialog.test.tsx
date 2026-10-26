import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import FailureRuleDialog from "./FailureRuleDialog";
import { failureCatalogPreview, gatewayDemo } from "./gateway-preview";
it("preserves a rule draft on conflict and retries only the rule edit", async () => {
  let writes = 0;
  mock.command.mockImplementation(async (name: string) => {
    if (name === "get_failure_policy_catalog") return failureCatalogPreview;
    if (name === "get_gateway")
      return { ...gatewayDemo, revision: writes ? "fresh" : "original" };
    if (name === "update_gateway") {
      if (writes++ === 0) throw new Error("版本冲突");
      return gatewayDemo;
    }
  });
  const user = userEvent.setup(),
    close = vi.fn();
  render(
    <FailureRuleDialog
      clientId="claude"
      initial={{ value: "fixture_overloaded", category: "concurrency" }}
      close={close}
    />,
  );
  await user.click(await screen.findByRole("button", { name: "保存规则" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("版本冲突");
  expect(screen.getByLabelText("错误码")).toHaveValue("fixture_overloaded");
  await user.click(screen.getByRole("button", { name: "重试" }));
  await waitFor(() => expect(close).toHaveBeenCalledOnce());
  expect(mock.command).toHaveBeenLastCalledWith("update_gateway", {
    clientId: "claude",
    expectedRevision: "fresh",
    edit: {
      op: "upsertFailureRule",
      rule: {
        id: expect.any(String),
        value: "fixture_overloaded",
        category: "concurrency",
        matchKind: "code",
      },
    },
  });
});
