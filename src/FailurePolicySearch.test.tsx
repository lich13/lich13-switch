import {
  act,
  fireEvent,
  render,
  screen,
  waitFor,
  within,
} from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, afterEach, expect, it, vi } from "vitest";
import type { FailureFocus, FailureMatch } from "./failure-policy-types";
const mock = vi.hoisted(() => ({ command: vi.fn() }));
vi.mock("./bridge", () => ({ command: mock.command }));
import Advanced from "./GatewayAdvanced";
import Search from "./FailurePolicySearch";
import { failureCatalogPreview, gatewayDemo } from "./gateway-preview";

const capacity: FailureMatch = {
  category: "capacity",
  source: "builtin_message",
  evidence: "at capacity",
  action: "已有会话优先原供应商",
  customRuleId: null,
  conflict: false,
  parameters: ["capacityRetrySeconds"],
  commonParameters: ["retryBudgetSeconds"],
};
const unmatched: FailureMatch = {
  category: null,
  source: null,
  evidence: "",
  action: null,
  customRuleId: null,
  conflict: false,
  parameters: [],
  commonParameters: [],
};
beforeEach(() => {
  mock.command.mockReset();
  mock.command.mockImplementation(async (name: string) => {
    if (name === "get_failure_policy_catalog") return failureCatalogPreview;
    if (name === "get_gateway") return gatewayDemo;
    if (name === "match_failure_policy") return capacity;
  });
});
afterEach(() => vi.useRealTimers());
const searchProps = {
  clientId: "codex" as const,
  rules: [],
  revision: "r1",
  catalog: failureCatalogPreview,
};

it("opens candidate parameters without changing the actual classification", async () => {
  const user = userEvent.setup();
  mock.command.mockImplementation(async (name: string) =>
    name === "match_failure_policy"
      ? {
          ...unmatched,
          category: "unknown_rate_limit",
          parsed: { status: 429 },
          candidates: [
            {
              category: "capacity",
              label: "容量不足",
              action: "已有会话优先原供应商",
              parameters: ["capacityRetrySeconds"],
              commonParameters: [],
            },
          ],
        }
      : failureCatalogPreview,
  );
  const selected = vi.fn();
  render(<Search {...searchProps} onSelection={selected} />);
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "429" },
  });
  await user.click(await screen.findByRole("button", { name: /容量不足 ·/ }));
  expect(screen.getByText(/当前判定：/)).toHaveTextContent("未分类限流");
  expect(screen.getByText("候选策略：容量不足")).toBeInTheDocument();
  expect(selected).toHaveBeenCalledWith("upstream", ["capacityRetrySeconds"]);
});

it("debounces unified search, accepts codes and text, and clears without lookup", async () => {
  vi.useFakeTimers();
  render(<Search {...searchProps} />);
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "model_capacity_exceeded" },
  });
  await act(async () => {
    vi.advanceTimersByTime(299);
  });
  expect(mock.command).not.toHaveBeenCalledWith(
    "match_failure_policy",
    expect.anything(),
  );
  await act(async () => {
    vi.advanceTimersByTime(1);
  });
  expect(mock.command).toHaveBeenLastCalledWith("match_failure_policy", {
    clientId: "codex",
    input: {
      query: "model_capacity_exceeded",
    },
  });
  fireEvent.click(screen.getByRole("button", { name: "清空" }));
  expect(
    screen.queryByRole("region", { name: "匹配结果" }),
  ).not.toBeInTheDocument();
  const count = mock.command.mock.calls.length;
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
  expect(mock.command).toHaveBeenCalledTimes(count);
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "Selected model is at capacity." },
  });
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
  expect(mock.command).toHaveBeenLastCalledWith("match_failure_policy", {
    clientId: "codex",
    input: {
      query: "Selected model is at capacity.",
    },
  });
  expect(screen.getAllByRole("textbox")).toHaveLength(1);
  expect(screen.queryByText("结构化字段")).not.toBeInTheDocument();
});

it("ignores responses after input changes, client changes, clear and unmount", async () => {
  vi.useFakeTimers();
  const pending: ((result: FailureMatch) => void)[] = [];
  mock.command.mockImplementation((name: string) =>
    name === "match_failure_policy"
      ? new Promise<FailureMatch>((resolve) => pending.push(resolve))
      : Promise.resolve(failureCatalogPreview),
  );
  const { rerender, unmount } = render(<Search {...searchProps} />);
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "first" },
  });
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "second" },
  });
  await act(async () => {
    pending[0](capacity);
    vi.advanceTimersByTime(300);
  });
  expect(
    screen.queryByRole("region", { name: "匹配结果" }),
  ).not.toBeInTheDocument();
  rerender(<Search {...searchProps} clientId="claude" />);
  await act(async () => {
    pending[1](capacity);
    vi.advanceTimersByTime(300);
  });
  expect(
    screen.queryByRole("region", { name: "匹配结果" }),
  ).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "清空" }));
  await act(async () => {
    pending[2](capacity);
  });
  expect(
    screen.queryByRole("region", { name: "匹配结果" }),
  ).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "third" },
  });
  await act(async () => {
    vi.advanceTimersByTime(300);
  });
  unmount();
  await act(async () => {
    pending[3](capacity);
  });
});

it("shows relevant controls only and preserves edits across searches and conflicts", async () => {
  const user = userEvent.setup();
  const save = vi
    .fn()
    .mockRejectedValueOnce(new Error("版本冲突"))
    .mockResolvedValueOnce(undefined);
  render(
    <Advanced
      clientId="codex"
      settings={gatewayDemo.settings}
      revision="original"
      busy={false}
      save={save}
    />,
  );
  await user.click(screen.getByText("故障策略"));
  expect(screen.queryByLabelText("容量错误等待 / 秒")).not.toBeInTheDocument();
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "at capacity" },
  });
  const field = await screen.findByRole("spinbutton", {
    name: "容量错误等待 / 秒",
  });
  expect(screen.getAllByLabelText("限流基础等待 / 秒")).toHaveLength(1);
  fireEvent.change(field, { target: { value: "120" } });
  mock.command.mockResolvedValueOnce({
    ...capacity,
    category: "rpm",
    parameters: ["rateLimitSeconds"],
  });
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "requests per minute" },
  });
  const rate = await screen.findByRole("spinbutton", {
    name: "限流基础等待 / 秒",
  });
  fireEvent.change(rate, { target: { value: "45" } });
  await user.click(screen.getByRole("button", { name: "保存参数" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("版本冲突");
  expect(save).toHaveBeenLastCalledWith(
    expect.objectContaining({
      capacityRetrySeconds: 120,
      rateLimitSeconds: 45,
    }),
    "original",
  );
  await user.click(screen.getByRole("button", { name: "重试" }));
  await waitFor(() =>
    expect(save).toHaveBeenLastCalledWith(
      expect.objectContaining({
        capacityRetrySeconds: 120,
        rateLimitSeconds: 45,
      }),
      null,
    ),
  );
});

it("preserves recorded classification, pre-fills log fields, and opens rule creation for misses", async () => {
  const user = userEvent.setup();
  const consumed = vi.fn();
  const focus: FailureFocus = {
    clientId: "claude",
    recordedCategory: "rpm",
    input: {
      code: "fixture_unknown",
      message: "fixture failure",
      status: 429,
      upstreamType: "rate_limit_error",
    },
  };
  mock.command.mockImplementation(async (name: string) => {
    if (name === "match_failure_policy") return unmatched;
    if (name === "get_gateway") return gatewayDemo;
    return failureCatalogPreview;
  });
  render(
    <Search
      {...searchProps}
      clientId="claude"
      focus={focus}
      consumed={consumed}
    />,
  );
  expect(screen.getByLabelText("搜索错误")).toHaveValue(
    "429 fixture_unknown fixture failure",
  );
  expect(await screen.findByText("未匹配到规则")).toBeInTheDocument();
  expect(screen.getByText("日志记录分类：请求频率 RPM")).toBeInTheDocument();
  expect(consumed).toHaveBeenCalledOnce();
  expect(mock.command).toHaveBeenCalledWith("match_failure_policy", {
    clientId: "claude",
    input: focus.input,
  });
  await user.click(screen.getByRole("button", { name: "加入故障规则" }));
  expect(
    within(screen.getByRole("dialog")).getByLabelText("错误码"),
  ).toHaveValue("fixture_unknown");
});

it("displays conflicts and fixed policies without irrelevant editable parameters", async () => {
  mock.command.mockImplementation(async (name: string) =>
    name === "match_failure_policy"
      ? {
          ...capacity,
          category: "safety_rejection",
          conflict: true,
          parameters: [],
          commonParameters: [],
          action: "返回原因，不换商绕过",
        }
      : failureCatalogPreview,
  );
  render(<Search {...searchProps} />);
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "fixture conflict" },
  });
  expect(
    await screen.findByText("返回原因，不换商绕过", { selector: "dd" }),
  ).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("自定义规则冲突");
});

it("locates internal policies, shares one parameter form and retains the draft across tabs", async () => {
  const user = userEvent.setup();
  const save = vi.fn().mockResolvedValue(undefined);
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation(async (name, args) =>
    name === "match_failure_policy"
      ? {
          ...unmatched,
          origin: "gateway",
          policyId: "connection",
          label: "连接失败与连接超时",
          action: "安全边界内原商重试",
          parameters: ["connectSeconds"],
        }
      : original(name, args),
  );
  render(
    <Advanced
      clientId="codex"
      settings={gatewayDemo.settings}
      revision="r1"
      busy={false}
      save={save}
    />,
  );
  await user.click(screen.getByText("故障策略"));
  fireEvent.change(screen.getByLabelText("搜索错误"), {
    target: { value: "CONNECT_TIMEOUT" },
  });
  await screen.findByText("当前判定：连接失败与连接超时");
  await waitFor(() =>
    expect(screen.getByRole("tab", { name: "网关内部策略" })).toHaveAttribute(
      "aria-selected",
      "true",
    ),
  );
  expect(screen.getAllByLabelText("等待与重试预算 / 秒")).toHaveLength(1);
  fireEvent.change(screen.getByLabelText("连接超时 / 秒"), {
    target: { value: "25" },
  });
  fireEvent.change(screen.getByLabelText("等待与重试预算 / 秒"), {
    target: { value: "600" },
  });
  expect(
    screen.queryByRole("button", { name: "添加匹配条件" }),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("tab", { name: "上游报错策略" }));
  expect(screen.getAllByLabelText("等待与重试预算 / 秒")).toHaveLength(1);
  expect(screen.getByLabelText("等待与重试预算 / 秒")).toHaveValue(600);
  expect(screen.getByLabelText("搜索错误")).toHaveValue("CONNECT_TIMEOUT");
  await user.click(screen.getByRole("tab", { name: "网关内部策略" }));
  expect(screen.getByLabelText("连接超时 / 秒")).toHaveValue(25);
  await user.click(screen.getByRole("button", { name: "保存参数" }));
  expect(save).toHaveBeenLastCalledWith(
    expect.objectContaining({ connectSeconds: 25, retryBudgetSeconds: 600 }),
    "r1",
  );
  expect(
    mock.command.mock.calls.filter(
      ([name]) => name === "get_failure_policy_catalog",
    ),
  ).toHaveLength(1);
});

it("shows the parsed attachment preview but keeps its event ID as lookup evidence", async () => {
  const parsed = {
    status: 503,
    code: "model_capacity_exceeded",
    message: "fixture busy",
  };
  mock.command.mockResolvedValue({ ...capacity, parsed });
  render(
    <Search
      {...searchProps}
      focus={{ clientId: "codex", input: { eventId: "fixture-event" } }}
    />,
  );
  await screen.findByText("当前判定：容量不足");
  expect(screen.getByLabelText("搜索错误")).toHaveValue(
    "503 model_capacity_exceeded fixture busy",
  );
  expect(mock.command).toHaveBeenCalledWith("match_failure_policy", {
    clientId: "codex",
    input: { eventId: "fixture-event" },
  });
});
