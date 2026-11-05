import {
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import QuickWaits from "./QuickWaits";
import { gatewayDemo } from "./gateway-preview";
import type { ActiveRequest, GatewayState } from "./types";
const request: ActiveRequest = {
  requestId: "fixture-active",
  providerId: null,
  model: "fixture-model",
  startedAt: 1000,
  elapsedSeconds: 0,
  phase: "admission",
  tokensPerSecond: null,
  waitId: "fixture-wait",
};
const pending = {
  requestId: "fixture-wait",
  providerId: null,
  model: "fixture-model",
  startedAt: 1000,
  elapsedSeconds: 0,
  remainingSeconds: 300 as number | null,
  phase: "admission",
};
const props = {
  clientId: "claude" as const,
  notify: vi.fn(),
  error: vi.fn(),
};
const gateway = (active: ActiveRequest, waits = [pending]): GatewayState => ({
  ...gatewayDemo,
  activeRequests: [active],
  pendingRequests: waits,
});
beforeEach(() => {
  vi.clearAllMocks();
  vi.spyOn(Date, "now").mockReturnValue(1_000_000);
});
afterEach(() => {
  vi.restoreAllMocks();
});
it("shows a request immediately and cancels only its matching wait on its platform", async () => {
  mock.command.mockResolvedValueOnce(false);
  render(<QuickWaits {...props} gateway={gateway(request)} />);
  expect(screen.getByText("fixture-model")).toBeInTheDocument();
  expect(screen.getByText("等待准入")).toBeInTheDocument();
  expect(screen.getByText("—")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "中止" }));
  await waitFor(() => expect(props.notify).toHaveBeenCalledWith("等待已结束"));
  expect(mock.command).toHaveBeenCalledWith("cancel_gateway_wait", {
    clientId: "claude",
    requestId: "fixture-wait",
  });
});
it("keeps compaction cancellable and shows elapsed time without a countdown", () => {
  render(
    <QuickWaits
      {...props}
      gateway={gateway(
        { ...request, phase: "compaction", elapsedSeconds: 601 },
        [{ ...pending, phase: "compaction", remainingSeconds: null }],
      )}
    />,
  );
  expect(screen.getByText("压缩中")).toBeInTheDocument();
  expect(screen.getByText("已用 601 秒")).toBeInTheDocument();
  expect(screen.queryByText(/剩余/)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "中止" })).toBeEnabled();
});
it("updates each streaming rate, shows zero during a pause, and removes completed requests", () => {
  const active = {
    ...request,
    phase: "streaming",
    waitId: null,
    tokensPerSecond: 28.5,
  };
  const state = {
    ...gateway(active, []),
    activeRequests: [
      active,
      {
        ...active,
        requestId: "second",
        model: "second-model",
        tokensPerSecond: 3,
      },
    ],
  };
  const view = render(<QuickWaits {...props} gateway={state} />);
  expect(screen.getByText("≈28.5 tok/s")).toBeInTheDocument();
  const row = screen
    .getByText("second-model")
    .closest(".gateway-request") as HTMLElement;
  expect(within(row).getByText("≈3 tok/s")).toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "中止" }),
  ).not.toBeInTheDocument();
  view.rerender(
    <QuickWaits
      {...props}
      gateway={gateway({ ...active, tokensPerSecond: 0 }, [])}
    />,
  );
  expect(screen.getByText("≈0 tok/s")).toBeInTheDocument();
  view.rerender(
    <QuickWaits {...props} gateway={{ ...gatewayDemo, activeRequests: [] }} />,
  );
  expect(screen.queryByRole("region")).not.toBeInTheDocument();
});
it("anchors countdowns to budget snapshots and keeps frozen elapsed time unchanged", () => {
  const state = gateway({ ...request, elapsedSeconds: 61 }, [
    { ...pending, elapsedSeconds: 61, remainingSeconds: 239 },
  ]);
  const view = render(<QuickWaits {...props} gateway={state} />);
  expect(screen.getByText("剩余 239 秒")).toBeInTheDocument();
  vi.mocked(Date.now).mockReturnValue(1_010_000);
  view.rerender(
    <QuickWaits
      {...props}
      gateway={{
        ...state,
        activeRequests: [{ ...request, elapsedSeconds: 71 }],
      }}
    />,
  );
  expect(screen.getByText("剩余 229 秒")).toBeInTheDocument();
  const frozen = gateway({ ...request, phase: "frozen", elapsedSeconds: 71 }, [
    { ...pending, phase: "frozen", remainingSeconds: null },
  ]);
  view.rerender(<QuickWaits {...props} gateway={frozen} />);
  vi.mocked(Date.now).mockReturnValue(1_610_000);
  view.rerender(
    <QuickWaits
      {...props}
      gateway={{ ...frozen, activeRequests: [...frozen.activeRequests!] }}
    />,
  );
  expect(screen.getByText("已用 71 秒")).toBeInTheDocument();
});
