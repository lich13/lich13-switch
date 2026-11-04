import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => mock);
import UsageBill, { billTime, localSecond } from "./UsageBill";
beforeEach(() => mock.command.mockReset());
it("exports the selected clients at second boundaries and distinguishes cleanup range", async () => {
  mock.command.mockResolvedValue({
    requests: 28,
    attempts: 30,
    cleaned: 40,
    retained: 0,
    cleanupError: null,
  });
  const saved = vi.fn();
  render(
    <UsageBill
      start={1_740_000_001_123}
      end={1_740_000_099_999}
      close={vi.fn()}
      saved={saved}
    />,
  );
  expect(screen.getByLabelText("开始")).toHaveAttribute("step", "1");
  expect(screen.getByText(/已导出明细与统计汇总/)).toBeVisible();
  expect(
    screen.queryByText(/同步清理该范围的明细与统计汇总/),
  ).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "导出并清理" }));
  await waitFor(() => expect(saved).toHaveBeenCalled());
  expect(mock.command).toHaveBeenCalledWith("export_usage_bill", {
    request: {
      clients: ["claude", "antigravity"],
      start: 1_740_000_001_000,
      end: 1_740_000_099_000,
      timezone: expect.any(String),
    },
  });
});
it("preserves the dialog after native cancellation or failed save", async () => {
  mock.command
    .mockResolvedValueOnce(null)
    .mockRejectedValueOnce({ message: "保存失败，未清理" });
  const saved = vi.fn(),
    close = vi.fn();
  render(<UsageBill close={close} saved={saved} />);
  await userEvent.selectOptions(screen.getByLabelText("客户端"), "claude");
  await userEvent.click(screen.getByRole("button", { name: "导出并清理" }));
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "导出并清理" })).toBeEnabled(),
  );
  expect(saved).not.toHaveBeenCalled();
  expect(close).not.toHaveBeenCalled();
  await userEvent.click(screen.getByRole("button", { name: "导出并清理" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("保存失败");
  expect(saved).not.toHaveBeenCalled();
});
it("rejects invalid local times while retaining seconds and fuzzy date/minute bounds", () => {
  expect(billTime("not-a-date")).toBeNull();
  const at = 1_740_000_023_000;
  expect(billTime(localSecond(at))).toBe(at);
  expect(billTime("2026-11-03", "start")).toBe(
    new Date("2026-11-03T00:00:00").getTime(),
  );
  expect(billTime("2026-11-03", "end")).toBe(
    new Date("2026-11-03T23:59:59").getTime(),
  );
  expect(billTime("2026-11-03T17:20", "start")).toBe(
    new Date("2026-11-03T17:20:00").getTime(),
  );
  expect(billTime("2026-11-03T17:20", "end")).toBe(
    new Date("2026-11-03T17:20:59").getTime(),
  );
});
