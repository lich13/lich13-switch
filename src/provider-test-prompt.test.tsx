import { act, renderHook } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
const mocks = vi.hoisted(() => ({ command: vi.fn(), confirm: vi.fn() }));
vi.mock("./bridge", () => ({ command: mocks.command }));
vi.mock("./confirmation", () => ({ confirmAction: mocks.confirm }));
import { flushTestPrompt, useTestPrompt } from "./provider-test-prompt";
afterEach(() => {
  vi.useRealTimers();
  mocks.command.mockReset();
  mocks.confirm.mockReset();
});
it("debounces 500 ms and coalesces writes while preserving newer input", async () => {
  vi.useFakeTimers();
  let release!: () => void;
  mocks.command
    .mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    )
    .mockResolvedValue(undefined);
  const { result, rerender } = renderHook(
    ({ initial }) => useTestPrompt(initial),
    { initialProps: { initial: "saved fixture" } },
  );
  act(() => result.current.change("first fixture"));
  await act(() => vi.advanceTimersByTimeAsync(499));
  expect(mocks.command).not.toHaveBeenCalled();
  await act(() => vi.advanceTimersByTimeAsync(1));
  expect(mocks.command).toHaveBeenCalledTimes(1);
  act(() => {
    result.current.change("middle fixture");
    result.current.change("latest fixture");
  });
  rerender({ initial: "stale event fixture" });
  expect(result.current.prompt).toBe("latest fixture");
  await act(async () => {
    release();
    await result.current.flush();
  });
  expect(mocks.command.mock.calls).toEqual([
    ["provider_test_save_prompt", { prompt: "first fixture" }],
    ["provider_test_save_prompt", { prompt: "latest fixture" }],
  ]);
  expect(result.current.prompt).toBe("latest fixture");
  expect(result.current.saving).toBe(false);
});
it("flushes before navigation and preserves a failed draft until retry or explicit discard", async () => {
  mocks.command.mockRejectedValue(new Error("fixture write failure"));
  mocks.confirm.mockResolvedValue(false);
  const { result } = renderHook(() => useTestPrompt("saved fixture"));
  act(() => result.current.change("new fixture"));
  await act(async () => {
    expect(await flushTestPrompt()).toBe(false);
  });
  expect(result.current.prompt).toBe("new fixture");
  expect(result.current.error).toContain("fixture write failure");
  mocks.command.mockResolvedValue(undefined);
  await act(async () => {
    expect(await result.current.flush()).toBe(true);
  });
  expect(result.current.error).toBe("");
  act(() => result.current.change("   "));
  const writes = mocks.command.mock.calls.length;
  await act(async () => {
    expect(await flushTestPrompt()).toBe(false);
  });
  expect(mocks.command).toHaveBeenCalledTimes(writes);
  expect(result.current.prompt).toBe("   ");
  mocks.confirm.mockResolvedValue(true);
  await act(async () => {
    expect(await flushTestPrompt()).toBe(true);
  });
  expect(result.current.prompt).toBe("new fixture");
});
it("waits for an in-flight write and flushes the newest draft before leaving", async () => {
  let release!: () => void;
  mocks.command
    .mockImplementationOnce(
      () =>
        new Promise<void>((resolve) => {
          release = resolve;
        }),
    )
    .mockResolvedValue(undefined);
  const { result } = renderHook(() => useTestPrompt("saved fixture"));
  act(() => result.current.change("first fixture"));
  let leaving!: Promise<boolean>;
  act(() => {
    leaving = flushTestPrompt();
  });
  act(() => result.current.change("latest fixture"));
  await act(async () => {
    release();
    expect(await leaving).toBe(true);
  });
  expect(mocks.command).toHaveBeenLastCalledWith("provider_test_save_prompt", {
    prompt: "latest fixture",
  });
  expect(mocks.confirm).not.toHaveBeenCalled();
});
