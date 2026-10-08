import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { EventRecord } from "./EventLog";

const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, (value: unknown) => void>(),
  confirm: vi.fn(),
}));

vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (name: string, listener: (value: unknown) => void) => {
    mock.listeners.set(name, listener);
    return () => mock.listeners.delete(name);
  }),
}));
vi.mock("./confirmation", () => ({
  confirmAction: mock.confirm,
}));

import EventLog from "./EventLog";

const record: EventRecord = {
  id: "event-1",
  firstAt: 1_760_000_000,
  lastAt: 1_760_000_030,
  count: 2,
  clientId: "codex",
  providerId: "provider-1",
  model: "fixture-model",
  reason: "model_unavailable",
  action: "trying_next",
  level: "warning",
  status: 404,
  errorCode: "MODEL_UNAVAILABLE",
  attempt: 1,
};

const page = (requestedPage: number) => ({
  items: [record],
  total: 51,
  page: requestedPage,
  error: null,
});

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(true);
  mock.command.mockImplementation(async (name: string, args: Record<string, unknown> = {}) => {
    if (name === "get_gateway") {
      return {
        clientId: args.clientId,
        providers: [{ id: "provider-1", name: "Fixture Provider" }],
      };
    }
    if (name === "get_app_events") {
      const filter = args.filter as { page?: number } | undefined;
      return page(Number(filter?.page ?? 1));
    }
    if (name === "get_app_event") return record;
    return undefined;
  });
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});

function eventCalls() {
  return mock.command.mock.calls.filter(([name]) => name === "get_app_events");
}

describe("event log", () => {
  it("filters by client, provider, level, reason and time range", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    expect(await screen.findByRole("button", { name: /模型不支持/ })).toBeInTheDocument();
    expect(screen.getByText("HTTP 404")).toBeInTheDocument();
    expect(screen.queryByText("MODEL_UNAVAILABLE")).not.toBeInTheDocument();
    expect(
      within(screen.getByRole("table"))
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["时间", "客户端 / 供应商", "状态", "问题", "处理结果"]);
    expect(await screen.findByRole("option", { name: "Fixture Provider" })).toBeInTheDocument();

    await user.selectOptions(screen.getByRole("combobox", { name: "客户端" }), "codex");
    await user.selectOptions(screen.getByRole("combobox", { name: "供应商" }), "provider-1");
    await user.selectOptions(screen.getByRole("combobox", { name: "级别" }), "warning");
    await user.selectOptions(screen.getByRole("combobox", { name: "原因" }), "model_unavailable");
    await user.selectOptions(screen.getByRole("combobox", { name: "状态码" }), "client_error");
    fireEvent.change(screen.getByLabelText("开始时间"), {
      target: { value: "2026-10-05T10:00" },
    });
    fireEvent.change(screen.getByLabelText("结束时间"), {
      target: { value: "2026-10-05T11:00" },
    });

    await waitFor(() => {
      const [, args] = eventCalls().at(-1)!;
      expect(args).toEqual({
        filter: expect.objectContaining({
          clientId: "codex",
          providerId: "provider-1",
          level: "warning",
          reason: "model_unavailable",
          statusGroup: "client_error",
          page: 1,
          from: expect.any(Number),
          to: expect.any(Number),
        }),
      });
    });
  });

  it("paginates independently and pauses event reloads while hidden", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });
    const before = eventCalls().length;
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => {
      const [, args] = eventCalls().at(-1)!;
      expect(args).toEqual({ filter: { page: 2, from: null, to: null } });
    });

    act(() => mock.listeners.get("app-visibility")?.(false));
    act(() => mock.listeners.get("app-event")?.({ record }));
    await Promise.resolve();
    expect(eventCalls().length).toBe(before + 1);

    act(() => mock.listeners.get("app-visibility")?.(true));
    await waitFor(() => expect(eventCalls().length).toBeGreaterThan(before + 1));
  });

  it("opens detail, closes on Escape and returns focus to the reason button", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    const reason = await screen.findByRole("button", { name: /模型不支持/ });
    await user.click(reason);
    const dialog = await screen.findByRole("dialog", { name: "模型不支持" });
    expect(dialog).toHaveTextContent("Fixture Provider");
    expect(within(dialog).getByText("HTTP 404")).toBeInTheDocument();
    expect(within(dialog).getByText("MODEL_UNAVAILABLE")).toBeInTheDocument();
    expect(within(dialog).getByText("fixture-model")).toBeInTheDocument();

    fireEvent(dialog, new Event("cancel", { bubbles: true, cancelable: true }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "模型不支持" })).not.toBeInTheDocument());
    expect(document.activeElement).toBe(reason);
  });

  it("clears the journal only after confirmation", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });
    await user.click(screen.getByRole("button", { name: "清空日志" }));
    expect(mock.confirm).toHaveBeenCalledWith("清空全部异常日志？");
    await waitFor(() => expect(mock.command).toHaveBeenCalledWith("clear_app_events"));
  });
});
