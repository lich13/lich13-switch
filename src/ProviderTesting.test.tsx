import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import type {
  ProviderTestingState,
  ProviderTestJob,
  ProviderTestRequest,
} from "./bridge-contracts";
const mock = vi.hoisted(() => ({
  command: vi.fn(),
  listeners: new Map<string, () => void>(),
}));
vi.mock("./bridge", () => ({
  command: mock.command,
  subscribe: vi.fn(async (event: string, callback: () => void) => {
    mock.listeners.set(event, callback);
    return () => mock.listeners.delete(event);
  }),
}));
import ProviderTesting, { scheduleTime } from "./ProviderTesting";
let gateway: typeof gatewayDemo;
let state: ProviderTestingState;
let blocked = false;
const makeJob = (request: ProviderTestRequest): ProviderTestJob => ({
  id: "fixture-job",
  clientId: request.clientId,
  providerId: request.providerId,
  providerName: "fixture provider",
  providerVersion: "fixture-version",
  kind: request.kind,
  mode: request.mode,
  requestedModel: request.model,
  forwardedModel: request.model,
  returnedModel: null,
  status: "running",
  createdAt: Date.now(),
  startedAt: Date.now(),
  completedAt: null,
  elapsedMs: 0,
  firstByteMs: null,
  error: null,
  report: null,
  groups: [],
  automatic: false,
  batchId: null,
  canRetry: false,
  completionReason: null,
});
beforeEach(() => {
  gateway = structuredClone(gatewayDemo);
  gateway.revision = "fixture-current";
  gateway.providers = gateway.providers.slice(0, 2);
  state = {
    revision: "fixture-settings",
    jobs: [],
    schedules: [],
    degraded: {},
    history: [],
    detectionStatus: [],
    error: null,
    detection: {
      enabled: false,
      mode: "smart",
      intervalMinutes: 15,
      minIntervalMinutes: 5,
      model: "gpt-6-luna",
      providers: {},
    },
    bank: {
      models: [{ id: "fixture-fingerprint", name: "Fixture" }],
      updating: false,
      error: null,
    },
  };
  blocked = false;
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "provider_test_state") return structuredClone(state);
      if (name === "get_gateway") return structuredClone(gateway);
      if (name === "provider_test_models") {
        if (blocked)
          throw { code: "PROVIDER_TEST", message: "请先完成一次正常请求" };
        return {
          models: [{ id: "fixture-upstream", name: "Fixture" }],
          defaultModel: "fixture-upstream",
          stale: false,
          error: null,
        };
      }
      if (name === "provider_test_start") {
        if (blocked)
          throw { code: "PROVIDER_TEST", message: "请先完成一次正常请求" };
        const job = makeJob(args.request as ProviderTestRequest);
        state.jobs.push(job);
        return job;
      }
      if (name === "provider_test_query")
        return {
          job: state.jobs.find((job) => job.id === args.id),
          output: {
            text: "fixture streamed result",
            groups: {},
            media: [],
            diagnostics: {},
          },
        };
      if (name === "provider_test_batch") {
        const jobs = (args.requests as ProviderTestRequest[]).map(
          (request, n) => ({
            ...makeJob(request),
            id: `fixture-job-${n}`,
            batchId: "fixture-batch",
          }),
        );
        state.jobs.push(...jobs);
        return { id: "fixture-batch", jobs, errors: [] };
      }
      if (name === "provider_test_next_cycle")
        return Date.parse("2026-10-24T10:00:03+08:00");
      return null;
    },
  );
});
it("starts a direct test using the latest gateway revision, streams output and cancels without switching", async () => {
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await waitFor(() =>
    expect(screen.getByRole("combobox", { name: "模型" })).toHaveValue(
      "fixture-upstream",
    ),
  );
  await user.type(screen.getByLabelText("测试输入"), "fixture request");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: expect.objectContaining({
      expectedRevision: "fixture-current",
      model: "fixture-upstream",
      input: "fixture request",
      clientId: "codex",
    }),
  });
  expect(
    await screen.findByText("fixture streamed result"),
  ).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "停止" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_cancel", {
    id: "fixture-job",
  });
  expect(
    mock.command.mock.calls.some(([name]) => name === "update_gateway"),
  ).toBe(false);
});
it("shows missing business UA errors and does not start tests on remount", async () => {
  blocked = true;
  const user = userEvent.setup();
  const first = render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "请先完成一次正常请求",
  );
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  await waitFor(() =>
    expect(
      mock.command.mock.calls.filter(
        ([name]) => name === "provider_test_start",
      ),
    ).toHaveLength(1),
  );
  first.unmount();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await screen.findByRole("alert");
  expect(
    mock.command.mock.calls.filter(([name]) => name === "provider_test_start"),
  ).toHaveLength(1);
});
it("search-only providers expose a search test and cannot enter fingerprint selection", async () => {
  gateway.providers[0].searchOnly = true;
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await screen.findByRole("combobox", { name: "接口" });
  expect(screen.getByRole("combobox", { name: "接口" })).toHaveValue("search");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: expect.objectContaining({ mode: "search" }),
  });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试类型" }),
    "fingerprint",
  );
  expect(
    screen.getByRole("checkbox", {
      name: `${gateway.providers[0].name} · 仅网络搜索`,
    }),
  ).toBeDisabled();
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
});
it("supports batch stopping and schedules the selected providers at cycle plus three seconds", async () => {
  const user = userEvent.setup();
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("checkbox", { name: "全选" }));
  await user.click(screen.getByRole("button", { name: "批量测试" }));
  await user.click(
    (await screen.findAllByRole("button", { name: "停止批次" }))[0],
  );
  expect(mock.command).toHaveBeenCalledWith("provider_test_stop_batch", {
    id: "fixture-batch",
  });
  await user.click(screen.getByRole("tab", { name: "预约" }));
  await user.click(screen.getByRole("button", { name: "最近周期 +3 秒" }));
  await waitFor(() =>
    expect(screen.getByLabelText("预约时间（UTC+08:00）")).toHaveValue(
      "2026-10-24T10:00:03.000",
    ),
  );
  await user.click(screen.getByRole("button", { name: "保存预约" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_schedule", {
    at: Date.parse("2026-10-24T10:00:03+08:00"),
    requests: expect.arrayContaining([
      expect.objectContaining({
        kind: "connection",
        mode: "text",
        input: "",
        media: null,
      }),
    ]),
  });
  expect(scheduleTime(Date.parse("2026-10-24T02:00:03Z"))).toBe(
    "2026-10-24T10:00:03",
  );
});
it("keeps detection drafts on background events and uses the configuration revision", async () => {
  const user = userEvent.setup();
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  const enabled = await screen.findByLabelText("启用定时检测");
  expect(enabled).not.toBeChecked();
  await user.click(enabled);
  state.jobs.push(
    makeJob({
      providerId: gateway.providers[0].id,
      clientId: "codex",
      model: "fixture",
    } as ProviderTestRequest),
  );
  await act(async () => {
    mock.listeners.get("provider-tests")?.();
  });
  expect(enabled).toBeChecked();
  await user.click(screen.getByRole("button", { name: "保存检测设置" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_set_detection", {
    expectedRevision: "fixture-settings",
    detection: expect.objectContaining({
      enabled: true,
      mode: "smart",
      minIntervalMinutes: 5,
    }),
  });
});
it("Claude exposes native connection testing without fingerprint or detection controls", async () => {
  gateway.clientId = "claude";
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await screen.findByRole("tab", { name: "连接与模型测试" });
  expect(
    screen.queryByRole("option", { name: "模型指纹测试" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("tab", { name: "定时检测" }),
  ).not.toBeInTheDocument();
  expect(
    within(screen.getByRole("combobox", { name: "接口" })).getAllByRole(
      "option",
    ),
  ).toHaveLength(1);
});
