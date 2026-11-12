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

let eventRecords: EventRecord[];

const page = (requestedPage: number) => ({
  items: structuredClone(eventRecords),
  total: 51,
  page: requestedPage,
  error: null,
});

beforeEach(() => {
  mock.command.mockReset();
  mock.listeners.clear();
  mock.confirm.mockReset();
  mock.confirm.mockResolvedValue(true);
  eventRecords = [record];
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
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
      if (name === "get_app_event") {
        return eventRecords.find((item) => item.id === args.id) ?? record;
      }
      return undefined;
    },
  );
  Object.defineProperty(document, "visibilityState", {
    configurable: true,
    value: "visible",
  });
});

function eventCalls() {
  return mock.command.mock.calls.filter(([name]) => name === "get_app_events");
}

it("starts a fresh visible request and ignores an older hidden-window response", async () => {
  const original = mock.command.getMockImplementation()!;
  let finish!: (value: ReturnType<typeof page>) => void;
  let reads = 0;
  mock.command.mockImplementation((name, args) =>
    name === "get_app_events" && ++reads === 1
      ? new Promise((resolve) => {
          finish = resolve;
        })
      : original(name, args),
  );
  render(<EventLog />);
  await waitFor(() => expect(eventCalls()).toHaveLength(1));
  act(() => mock.listeners.get("app-visibility")?.(false));
  act(() => mock.listeners.get("app-event")?.(record));
  expect(eventCalls()).toHaveLength(1);
  act(() => mock.listeners.get("app-visibility")?.(true));
  await waitFor(() => expect(eventCalls()).toHaveLength(2));
  expect(
    await screen.findByRole("button", { name: /模型不支持/ }),
  ).toBeInTheDocument();
  await act(async () => {
    finish({ ...page(1), items: [], total: 0 });
  });
  expect(
    screen.getByRole("button", { name: /模型不支持/ }),
  ).toBeInTheDocument();
});

describe("event log", () => {
  it("filters by client, provider, level, reason and time range", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    expect(
      await screen.findByRole("button", { name: /模型不支持/ }),
    ).toBeInTheDocument();
    expect(screen.getByText("404")).toBeInTheDocument();
    expect(screen.getByText("MODEL_UNAVAILABLE")).toBeInTheDocument();
    expect(
      within(screen.getByRole("table"))
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["时间", "客户端 / 供应商", "状态", "错误摘要", "处理结果"]);
    expect(
      await screen.findByRole("option", { name: "Fixture Provider" }),
    ).toBeInTheDocument();

    await user.selectOptions(
      screen.getByRole("combobox", { name: "客户端" }),
      "codex",
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "供应商" }),
      "provider-1",
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "级别" }),
      "warning",
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "原因" }),
      "model_unavailable",
    );
    await user.selectOptions(
      screen.getByRole("combobox", { name: "状态码" }),
      "client_error",
    );
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

  it("sends each selected category to the event query", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });

    const filter = screen.getByRole("combobox", { name: "类别" });
    for (const category of ["ordinary_error", "circuit_error", "status"]) {
      await user.selectOptions(filter, category);
      await waitFor(() => {
        const [, args] = eventCalls().at(-1)!;
        expect(args).toEqual({
          filter: expect.objectContaining({
            category,
            page: 1,
            from: null,
            to: null,
          }),
        });
      });
    }
  });

  it("renders category labels, classifies legacy rows, and keeps five columns", async () => {
    const categorized = (
      id: string,
      category: EventRecord["category"],
      overrides: Partial<EventRecord> = {},
    ): EventRecord => ({ ...record, ...overrides, id, category });
    eventRecords = [
      categorized("ordinary-category", "ordinary_error", {
        reason: "upstream_service",
        action: "returned",
        level: "error",
        status: 502,
      }),
      categorized("circuit-category", "circuit_error", {
        reason: "circuit_open",
        action: "returned",
        level: "error",
        status: 502,
        details: { healthScope: "provider" },
      }),
      categorized("status-category", "status", {
        reason: "thread_handoff",
        action: "routed",
        level: "info",
        status: null,
      }),
      { ...record, id: "legacy-category" },
    ];

    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });
    const table = screen.getByRole("table");
    await waitFor(() =>
      expect(within(table).getAllByRole("row")).toHaveLength(5),
    );

    expect(
      within(table)
        .getAllByRole("columnheader")
        .map((header) => header.textContent),
    ).toEqual(["时间", "客户端 / 供应商", "状态", "错误摘要", "处理结果"]);
    expect(within(table).queryByText("普通错误")).not.toBeInTheDocument();
    expect(within(table).queryByText("熔断错误")).not.toBeInTheDocument();
    expect(within(table).queryByText("状态记录")).not.toBeInTheDocument();
  });

  it.each([
    ["model", "模型熔断"],
    ["provider", "供应商熔断"],
  ] as const)(
    "summarizes a %s circuit event with its own action and trigger",
    async (healthScope, expectedLabel) => {
      eventRecords = [
        {
          ...record,
          id: `circuit-${healthScope}`,
          reason: "circuit_open",
          action: "returned",
          level: "error",
          status: 502,
          errorCode: "CIRCUIT_OPEN",
          category: "circuit_error",
          details: {
            healthScope,
            circuit: {
              causeReason: "upstream_service",
              retryAt: 1_760_000_090,
              failures: 3,
              failureThreshold: 3,
              failedRequests: 3,
              requests: 3,
              errorRate: 1,
              minRequests: 1,
              trigger: "consecutive_failures",
            },
          },
        },
      ];

      render(<EventLog />);
      const table = screen.getByRole("table");
      await waitFor(() =>
        expect(within(table).getAllByRole("row")).toHaveLength(2),
      );
      const summary = within(table).getAllByRole("button")[0];

      expect(summary).toHaveTextContent(expectedLabel);
      expect(summary).toHaveTextContent("CIRCUIT_OPEN");
      expect(summary).not.toHaveTextContent("502");
      expect(summary).toHaveTextContent("连续失败 3/3");
    },
  );

  it("paginates independently and pauses event reloads while hidden", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });
    const before = eventCalls().length;
    await user.click(screen.getByRole("button", { name: "下一页" }));
    await waitFor(() => {
      const [, args] = eventCalls().at(-1)!;
      expect(args).toEqual({
        filter: { origin: "upstream", page: 2, from: null, to: null },
      });
    });

    act(() => mock.listeners.get("app-visibility")?.(false));
    act(() => mock.listeners.get("app-event")?.({ record }));
    await Promise.resolve();
    expect(eventCalls().length).toBe(before + 1);

    act(() => mock.listeners.get("app-visibility")?.(true));
    await waitFor(() =>
      expect(eventCalls().length).toBeGreaterThan(before + 1),
    );
  });

  it("opens detail, closes on Escape and returns focus to the reason button", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    const reason = await screen.findByRole("button", { name: /模型不支持/ });
    await user.click(reason);
    const dialog = await screen.findByRole("dialog", { name: "模型不支持" });
    expect(dialog).toHaveTextContent("Fixture Provider");
    expect(within(dialog).getByText("404")).toBeInTheDocument();
    expect(within(dialog).getByText("MODEL_UNAVAILABLE")).toBeInTheDocument();
    expect(within(dialog).getByText("fixture-model")).toBeInTheDocument();

    fireEvent(dialog, new Event("cancel", { bubbles: true, cancelable: true }));
    await waitFor(() =>
      expect(
        screen.queryByRole("dialog", { name: "模型不支持" }),
      ).not.toBeInTheDocument(),
    );
    expect(document.activeElement).toBe(reason);
  });

  it("shows upstream details and distinguishes WebSocket close codes from HTTP status", async () => {
    const detailed = {
      ...record,
      id: "event-upstream-details",
      reason: "upstream_service",
      action: "returned",
      level: "error",
      status: 502,
      errorCode: "UPSTREAM_SERVICE_ERROR",
      details: {
        upstreamCode: "overloaded_error",
        upstreamType: "server_error",
        parameter: "model",
        message: "上游暂时不可用；请求内容已隐去",
        phase: "response",
        wsCloseCode: 1006,
        countedFailure: true,
        waitSeconds: 45,
        causeId: "11111111-2222-4333-8444-555555555555",
        circuit: {
          failures: 3,
          failureThreshold: 3,
          failedRequests: 3,
          requests: 3,
          errorRate: 1,
          minRequests: 1,
          trigger: "consecutive_failures",
        },
      },
    } as EventRecord;
    eventRecords = [detailed];

    const user = userEvent.setup();
    render(<EventLog />);
    const reason = await screen.findByRole("button", { name: /上游服务异常/ });
    await user.click(reason);
    const dialog = await screen.findByRole("dialog", { name: "上游服务异常" });

    expect(within(dialog).getByText(/502/)).toBeInTheDocument();
    expect(within(dialog).getByText("overloaded_error")).toBeInTheDocument();
    expect(within(dialog).getByText("server_error")).toBeInTheDocument();
    expect(
      within(dialog).getByText("上游暂时不可用；请求内容已隐去"),
    ).toBeInTheDocument();
    expect(
      within(dialog).getByText("HTTP 状态").nextElementSibling,
    ).toHaveTextContent("502 · WS 1006");
    expect(
      within(dialog).getByText(/计入熔断/).nextElementSibling,
    ).toHaveTextContent("是");
  });

  it("clears the journal only after confirmation", async () => {
    const user = userEvent.setup();
    render(<EventLog />);
    await screen.findByRole("button", { name: /模型不支持/ });
    await user.click(screen.getByRole("button", { name: "清空全部日志" }));
    expect(mock.confirm).toHaveBeenCalledWith(
      "清空全部日志（上游报错与网关内部）？",
    );
    await waitFor(() =>
      expect(mock.command).toHaveBeenCalledWith("clear_app_events"),
    );
  });
});

it("queries providers only on mount and keeps event names through filtering and paging", async () => {
  const user = userEvent.setup();
  render(<EventLog />);
  await screen.findByRole("button", { name: /模型不支持/ });
  const gateways = () =>
    mock.command.mock.calls.filter(([name]) => name === "get_gateway");
  expect(gateways()).toHaveLength(4);
  const initial = eventCalls().length;
  await user.selectOptions(
    screen.getByRole("combobox", { name: "客户端" }),
    "codex",
  );
  await waitFor(() => expect(eventCalls()).toHaveLength(initial + 1));
  await user.click(screen.getByRole("button", { name: "下一页" }));
  await waitFor(() => expect(eventCalls()).toHaveLength(initial + 2));
  expect(gateways()).toHaveLength(4);
  act(() =>
    mock.listeners.get("gateway-state")?.({
      clientId: "codex",
      providers: [{ id: "provider-1", name: "Renamed fixture" }],
    }),
  );
  expect(
    await screen.findByRole("option", { name: "Renamed fixture" }),
  ).toBeInTheDocument();
  expect(eventCalls()).toHaveLength(initial + 2);
});

it("separates origins, resets pagination, and hides policy actions for internal causes", async () => {
  const user = userEvent.setup();
  const internal: EventRecord = {
    ...record,
    id: "internal",
    reason: "circuit_open",
    origin: "gateway",
    details: { upstreamCode: "overloaded_error", errorBasis: "service_error" },
  };
  const agyError: EventRecord = {
    ...record,
    id: "agy-err",
    clientId: "antigravity",
    providerId: null,
    reason: "request_interrupted",
    origin: "antigravity",
    details: {
      upstreamCode: "STREAM_TIMEOUT",
      message: "stream was interrupted after 5m0s",
    },
  };
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation(async (name, args) => {
    if (name === "get_app_events")
      return {
        items:
          args.filter.origin === "gateway"
            ? [internal]
            : args.filter.origin === "antigravity"
              ? [agyError]
              : [record],
        total: 51,
        page: args.filter.page,
        error: null,
      };
    if (name === "get_app_event")
      return args.id === "internal"
        ? internal
        : args.id === "agy-err"
          ? agyError
          : record;
    return original(name, args);
  });
  render(<EventLog />);
  await screen.findByRole("button", { name: "下一页" });
  await user.click(screen.getByRole("button", { name: "下一页" }));
  await user.click(screen.getByRole("tab", { name: "网关内部" }));
  await waitFor(() =>
    expect(eventCalls().at(-1)?.[1]).toEqual({
      filter: { origin: "gateway", page: 1, from: null, to: null },
    }),
  );
  const detail = await screen.findByRole("button", {
    name: "熔断",
  });
  await user.click(detail);
  const dialog = await screen.findByRole("dialog");
  expect(
    within(dialog).queryByRole("button", { name: "加入故障规则" }),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: "Antigravity app" }));
  await waitFor(() =>
    expect(eventCalls().at(-1)?.[1]).toEqual({
      filter: { origin: "antigravity", page: 1, from: null, to: null },
    }),
  );
  expect(
    await screen.findByText(/stream was interrupted after 5m0s/),
  ).toBeInTheDocument();
});

it("opens the internal tab and clears incompatible filters when a gateway event is focused", async () => {
  const user = userEvent.setup();
  const { rerender } = render(<EventLog />);
  await screen.findByRole("button", { name: /模型不支持/ });
  await user.selectOptions(screen.getByLabelText("类别"), "ordinary_error");
  eventRecords = [{ ...record, reason: "network", origin: "gateway" }];
  rerender(
    <EventLog
      focus={{
        id: record.id,
        clientId: "codex",
        providerId: null,
        sequence: 1,
      }}
    />,
  );
  await screen.findByRole("dialog", { name: "连接失败" });
  expect(screen.getByRole("tab", { name: "网关内部" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await waitFor(() => {
    const args = eventCalls().at(-1)?.[1];
    expect(args).toEqual({
      filter: expect.objectContaining({ origin: "gateway" }),
    });
    expect(args.filter.category).toBeUndefined();
  });
});

it("refreshes within 250ms even while events keep arriving", async () => {
  vi.useFakeTimers();
  try {
    const { unmount } = render(<EventLog />);
    await act(async () => {});
    const initial = eventCalls().length;
    for (let tick = 0; tick < 9; tick++) {
      await act(async () => {
        mock.listeners.get("app-event")?.({ record });
        vi.advanceTimersByTime(100);
      });
    }
    expect(eventCalls().length).toBe(initial + 3);
    unmount();
  } finally {
    vi.useRealTimers();
  }
});

it("keeps each client's latest gateway event when initial snapshots arrive late", async () => {
  let resolveInitial!: (value: unknown) => void;
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation((name, args) =>
    name === "get_gateway" && args.clientId === "codex"
      ? new Promise((resolve) => {
          resolveInitial = resolve;
        })
      : original(name, args),
  );
  render(<EventLog />);
  await screen.findByRole("button", { name: /模型不支持/ });
  act(() =>
    mock.listeners.get("gateway-state")?.({
      clientId: "codex",
      providers: [{ id: "provider-1", name: "Fresh Provider" }],
    }),
  );
  await act(async () =>
    resolveInitial({
      clientId: "codex",
      providers: [{ id: "provider-1", name: "Old Provider" }],
    }),
  );
  expect(
    screen.getByRole("cell", { name: /Fresh Provider/ }),
  ).toBeInTheDocument();
  expect(screen.queryByText("Old Provider")).not.toBeInTheDocument();
  expect(
    screen.getAllByRole("option", { name: "Fixture Provider" }).length,
  ).toBeGreaterThan(0);
});

it("polls only the visible Antigravity tab and preserves the page and open detail", async () => {
  vi.useFakeTimers();
  try {
    const view = render(<EventLog />);
    await act(async () => {
      await Promise.resolve();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(eventCalls()).toHaveLength(1);
    fireEvent.click(screen.getByRole("tab", { name: "Antigravity app" }));
    await act(async () => {
      await Promise.resolve();
    });
    fireEvent.click(screen.getByRole("button", { name: "下一页" }));
    await act(async () => {
      await Promise.resolve();
    });
    fireEvent.click(screen.getByRole("button", { name: /模型不支持/ }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    const before = eventCalls().length;
    await act(async () => {
      await vi.advanceTimersByTimeAsync(5_000);
    });
    expect(eventCalls()).toHaveLength(before + 1);
    expect(eventCalls().at(-1)?.[1].filter).toMatchObject({
      origin: "antigravity",
      page: 2,
    });
    expect(screen.getByRole("dialog")).toBeInTheDocument();
    act(() => mock.listeners.get("app-visibility")!(false));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(eventCalls()).toHaveLength(before + 1);
    act(() => mock.listeners.get("app-visibility")!(true));
    await act(async () => {
      await Promise.resolve();
    });
    expect(eventCalls()).toHaveLength(before + 2);
    fireEvent.click(screen.getByRole("tab", { name: "网关内部" }));
    await act(async () => {
      await Promise.resolve();
    });
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(eventCalls()).toHaveLength(before + 3);
    view.unmount();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(10_000);
    });
    expect(eventCalls()).toHaveLength(before + 3);
  } finally {
    vi.useRealTimers();
  }
});

it("coalesces slow log refreshes and ignores a response from the previous tab", async () => {
  vi.useFakeTimers();
  try {
    let resolve!: (value: ReturnType<typeof page>) => void;
    const slow = new Promise<ReturnType<typeof page>>((done) => {
      resolve = done;
    });
    const original = mock.command.getMockImplementation()!;
    mock.command.mockImplementation((name, args) =>
      name === "get_app_events" && args.filter.origin === "antigravity"
        ? slow
        : original(name, args),
    );
    render(<EventLog />);
    await act(async () => {
      await Promise.resolve();
    });
    fireEvent.click(screen.getByRole("tab", { name: "Antigravity app" }));
    await act(async () => {
      await vi.advanceTimersByTimeAsync(15_000);
    });
    expect(eventCalls()).toHaveLength(2);
    fireEvent.click(screen.getByRole("tab", { name: "网关内部" }));
    await act(async () => {
      await Promise.resolve();
    });
    expect(eventCalls()).toHaveLength(3);
    await act(async () => {
      resolve({
        ...page(1),
        items: [{ ...record, id: "stale", errorCode: "STALE_RESPONSE" }],
      });
    });
    expect(screen.queryByText("STALE_RESPONSE")).not.toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: /模型不支持/ }),
    ).toBeInTheDocument();
  } finally {
    vi.useRealTimers();
  }
});

it("clearing logs starts a fresh query even when the previous query is still pending", async () => {
  let finish!: (value: ReturnType<typeof page>) => void;
  const slow = new Promise<ReturnType<typeof page>>((resolve) => {
    finish = resolve;
  });
  const original = mock.command.getMockImplementation()!;
  let calls = 0;
  mock.command.mockImplementation((name, args) => {
    if (name === "get_app_events")
      return ++calls === 1
        ? slow
        : Promise.resolve({ ...page(1), items: [], total: 0 });
    return original(name, args);
  });
  render(<EventLog />);
  await waitFor(() => expect(eventCalls()).toHaveLength(1));
  fireEvent.click(screen.getByRole("button", { name: "清空全部日志" }));
  await waitFor(() => expect(eventCalls()).toHaveLength(2));
  await act(async () => {
    finish(page(1));
  });
  expect(
    screen.queryByRole("button", { name: /模型不支持/ }),
  ).not.toBeInTheDocument();
});

it.each(["上游报错", "网关内部"])(
  "refreshes %s events received during a slow query",
  async (tab) => {
    vi.useFakeTimers();
    try {
      const original = mock.command.getMockImplementation()!;
      const finish: ((value: ReturnType<typeof page>) => void)[] = [];
      mock.command.mockImplementation((name, args) =>
        name === "get_app_events"
          ? new Promise((resolve) => finish.push(resolve))
          : original(name, args),
      );
      render(<EventLog />);
      await act(async () => {
        await Promise.resolve();
      });
      if (tab === "网关内部")
        fireEvent.click(screen.getByRole("tab", { name: tab }));
      await act(async () => {
        await Promise.resolve();
      });
      const before = eventCalls().length;
      act(() => {
        mock.listeners.get("app-event")?.({});
        mock.listeners.get("app-event")?.({});
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(250);
      });
      expect(eventCalls()).toHaveLength(before + 1);
      fireEvent.click(screen.getByRole("button", { name: "刷新日志" }));
      expect(eventCalls()).toHaveLength(before + 2);
      await act(async () => {
        finish.pop()!({
          ...page(1),
          items: [{ ...record, errorCode: "NEW_EVENT" }],
        });
      });
      await act(async () => {
        for (const resolve of finish) resolve({ ...page(1), items: [] });
      });
      expect(screen.getByText("NEW_EVENT")).toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  },
);

it("clearing the second page queries only the first page", async () => {
  render(<EventLog />);
  await screen.findByRole("button", { name: /模型不支持/ });
  fireEvent.click(screen.getByRole("button", { name: "下一页" }));
  await waitFor(() => expect(eventCalls().at(-1)?.[1].filter.page).toBe(2));
  const before = eventCalls().length;
  fireEvent.click(screen.getByRole("button", { name: "清空全部日志" }));
  await waitFor(() => expect(eventCalls()).toHaveLength(before + 1));
  expect(eventCalls().at(-1)?.[1].filter.page).toBe(1);
});
