import {
  act,
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
beforeEach(() => {
  mock.command.mockReset();
  vi.spyOn(Date, "now").mockReturnValue(Date.now());
});
afterEach(() => {
  vi.useRealTimers();
  vi.restoreAllMocks();
});
it("keeps long compaction cancellable and shows elapsed time without a countdown", async () => {
  const start = Math.floor(Date.now() / 1000);
  mock.command.mockResolvedValueOnce(true);
  render(
    <QuickWaits
      gateway={{
        ...gatewayDemo,
        pendingRequests: [
          {
            requestId: "fixture-compaction",
            providerId: null,
            model: "fixture-model",
            startedAt: start - 601,
            elapsedSeconds: 601,
            remainingSeconds: null,
            phase: "compaction",
          },
        ],
      }}
      clientId="codex"
      visible
      notify={vi.fn()}
      error={vi.fn()}
    />,
  );
  expect(screen.getByText("压缩中")).toBeInTheDocument();
  expect(screen.getByText("已用 601 秒")).toBeInTheDocument();
  expect(screen.queryByText(/剩余/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "中止" }));
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("cancel_gateway_wait", {
      clientId: "codex",
      requestId: "fixture-compaction",
    }),
  );
});
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

it("anchors the countdown to the received budget and excludes frozen time", () => {
  vi.useFakeTimers();
  let now = 10_000_000;
  vi.spyOn(Date, "now").mockImplementation(() => now);
  const pending = {
    requestId: "fixture-queued",
    providerId: null,
    model: "fixture-model",
    startedAt: 1,
    elapsedSeconds: 61,
    remainingSeconds: 239 as number | null,
    phase: "admission",
  };
  const props = {
    clientId: "codex" as const,
    visible: true,
    notify: vi.fn(),
    error: vi.fn(),
  };
  const view = render(
    <QuickWaits
      {...props}
      gateway={{ ...gatewayDemo, pendingRequests: [pending] }}
    />,
  );
  expect(screen.getByText("等待准入")).toBeInTheDocument();
  expect(screen.getByText("剩余 239 秒")).toBeInTheDocument();
  act(() => {
    now += 10_000;
    vi.advanceTimersByTime(10_000);
  });
  expect(screen.getByText("剩余 229 秒")).toBeInTheDocument();
  view.rerender(
    <QuickWaits
      {...props}
      gateway={{
        ...gatewayDemo,
        pendingRequests: [
          {
            ...pending,
            phase: "frozen",
            elapsedSeconds: 71,
            remainingSeconds: null,
          },
        ],
      }}
    />,
  );
  act(() => {
    now += 600_000;
    vi.advanceTimersByTime(600_000);
  });
  expect(screen.getByText("已用 71 秒")).toBeInTheDocument();
  view.rerender(
    <QuickWaits
      {...props}
      gateway={{
        ...gatewayDemo,
        pendingRequests: [
          { ...pending, elapsedSeconds: 71, remainingSeconds: 229 },
        ],
      }}
    />,
  );
  expect(screen.getByText("剩余 229 秒")).toBeInTheDocument();
  act(() => {
    now += 1_000;
    vi.advanceTimersByTime(1_000);
  });
  expect(screen.getByText("剩余 228 秒")).toBeInTheDocument();
});
