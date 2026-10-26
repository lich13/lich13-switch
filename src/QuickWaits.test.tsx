import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import QuickWaits from "./QuickWaits";
import { gatewayDemo } from "./gateway-preview";
beforeEach(() => mock.command.mockReset());
it("shows only long waits and cancels the selected request on its platform", async () => {
  const start = Math.floor(Date.now() / 1000);
  const gateway = {
    ...gatewayDemo,
    pendingRequests: [59, 60, 90].map((seconds, i) => ({
      requestId: `fixture-${i}`,
      providerId: "primary",
      model: `model-${i}`,
      startedAt: start - seconds,
      elapsedSeconds: seconds,
      remainingSeconds: 300 - seconds,
      phase: "headers",
    })),
  };
  mock.command.mockResolvedValueOnce(false);
  const notify = vi.fn();
  const { rerender } = render(
    <QuickWaits
      gateway={gateway}
      clientId="claude"
      visible
      notify={notify}
      error={vi.fn()}
    />,
  );
  expect(screen.queryByText("model-0")).not.toBeInTheDocument();
  const row = screen
    .getByText("model-2")
    .closest(".quick-wait-row") as HTMLElement;
  fireEvent.click(within(row).getByRole("button", { name: "中止" }));
  await waitFor(() => expect(notify).toHaveBeenCalledWith("等待已结束"));
  expect(mock.command).toHaveBeenCalledWith("cancel_gateway_wait", {
    clientId: "claude",
    requestId: "fixture-2",
  });
  rerender(
    <QuickWaits
      gateway={{ ...gateway, pendingRequests: [] }}
      clientId="claude"
      visible
      notify={notify}
      error={vi.fn()}
    />,
  );
  expect(screen.queryByRole("region")).not.toBeInTheDocument();
});
