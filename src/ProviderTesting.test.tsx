import { act, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { beforeEach, expect, it, vi } from "vitest";
import { gatewayDemo } from "./gateway-preview";
import type {
  ProviderTestingState,
  ProviderTestJob,
  ProviderTestRequest,
  ProviderTestModels,
  ProviderTestError,
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
import ProviderTesting from "./ProviderTesting";
let gateway: typeof gatewayDemo;
let states: Record<string, ProviderTestingState>;
let catalogs: Record<string, ProviderTestModels>;
let refreshFailure = false;
let errorDetail: ProviderTestError | null = null;
let reply: string;
let outputAvailable: boolean;
const emptyCatalog = (): ProviderTestModels => ({
  models: [],
  defaultModel: "",
  fetchedAt: null,
  error: null,
});
const catalog = (): ProviderTestModels => ({
  models: [
    { id: "fixture-model", name: "Fixture" },
    { id: "fixture-alt", name: "Alternate" },
  ],
  defaultModel: "fixture-model",
  fetchedAt: 1000,
  error: null,
});
const makeJob = (request: ProviderTestRequest): ProviderTestJob => ({
  id: `fixture-job-${request.providerId}`,
  clientId: request.clientId,
  providerId: request.providerId,
  providerName: "Fixture",
  providerVersion: "fixture-version",
  endpoint: request.endpoint,
  kind: request.kind,
  mode: "text",
  requestedModel: request.model,
  forwardedModel: request.model,
  returnedModel: null,
  status: errorDetail ? "failed" : "running",
  createdAt: Date.now(),
  startedAt: Date.now(),
  completedAt: null,
  elapsedMs: 0,
  firstByteMs: null,
  error: errorDetail ? "上游返回错误" : null,
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
  gateway.providers.forEach((p, i) => {
    p.baseUrl = `https://provider-${i}.example.invalid/v1`;
    p.extraBaseUrls = [`https://auxiliary-${i}.example.invalid/v1`];
  });
  states = Object.fromEntries(
    gateway.providers.map((p) => [
      p.id,
      {
        revision: `fixture-settings-${p.id}`,
        connectionPrompt: "Reply with a short greeting.",
        jobs: [],
        degraded: {},
        history: [],
        detectionStatus: [],
        error: null,
        detection: {
          enabled: false,
          mode: "smart",
          intervalMinutes: 15,
          minIntervalMinutes: 5,
          model: "fixture-model",
          endpoint: p.baseUrl,
        },
        bank: {
          models: [{ id: "fixture-fingerprint", name: "Fixture fingerprint" }],
          updating: false,
          error: null,
        },
      },
    ]),
  );
  catalogs = {};
  refreshFailure = false;
  errorDetail = null;
  reply = "fixture streamed result";
  outputAvailable = true;
  mock.command.mockReset();
  mock.listeners.clear();
  mock.command.mockImplementation(
    async (name: string, args: Record<string, unknown> = {}) => {
      if (name === "provider_test_state")
        return structuredClone(states[String(args.providerId)]);
      if (name === "get_gateway") return structuredClone(gateway);
      if (name === "provider_test_models") {
        const key = `${args.providerId}:${args.endpoint}`;
        if (args.refresh && !refreshFailure) catalogs[key] = catalog();
        const value = structuredClone(catalogs[key] ?? emptyCatalog());
        if (args.refresh && refreshFailure)
          value.error = "fixture upstream unavailable";
        return value;
      }
      if (name === "provider_test_start") {
        const request = args.request as ProviderTestRequest;
        const job = makeJob(request);
        states[job.providerId].jobs.unshift(job);
        if (request.kind === "connection")
          states[job.providerId].connectionPrompt = request.prompt!;
        return job;
      }
      if (name === "provider_test_query")
        return {
          job: Object.values(states)
            .flatMap((s) => s.jobs)
            .find((j) => j.id === args.id),
          output: outputAvailable
            ? {
                text: reply,
                groups: {},
                diagnostics: {},
                error: errorDetail,
                groupErrors: {},
              }
            : null,
        };
      if (name === "provider_test_set_detection") {
        const state = states[String(args.providerId)];
        if (args.expectedRevision !== state.revision)
          throw new Error("该供应商检测设置已变化，请重新读取后重试");
        state.detection = structuredClone(
          args.detection as ProviderTestingState["detection"],
        );
        state.revision += "-saved";
      }
      return null;
    },
  );
});
const open = (index = 0) =>
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[index]}
      close={vi.fn()}
    />,
  );
const seed = (index = 0, endpoint = gateway.providers[index].baseUrl) => {
  catalogs[`${gateway.providers[index].id}:${endpoint}`] = catalog();
};
const emit = async () => {
  await act(async () => {
    mock.listeners.get("provider-tests")?.();
  });
};

it("sends and remembers the provider prompt without overwriting a draft on refresh", async () => {
  const user = userEvent.setup();
  const first = open();
  const input = await screen.findByDisplayValue("Reply with a short greeting.");
  await user.clear(input);
  await user.type(input, "fixture greeting{Enter}第二行");
  states[gateway.providers[0].id].connectionPrompt = "external fixture";
  await emit();
  expect(input).toHaveValue("fixture greeting\n第二行");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  await screen.findByRole("button", { name: "停止测试" });
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: expect.objectContaining({ prompt: "fixture greeting\n第二行" }),
  });
  expect(states[gateway.providers[1].id].connectionPrompt).toBe(
    "Reply with a short greeting.",
  );
  first.unmount();
  open();
  await waitFor(() =>
    expect(screen.getByRole("textbox", { name: "测试文案" })).toHaveValue(
      "fixture greeting\n第二行",
    ),
  );
  await screen.findByText("fixture streamed result");
});

it("blocks whitespace and keeps the draft when starting cannot be saved", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation((name, args) => {
    if (name === "provider_test_start") throw new Error("fixture save failed");
    return original(name, args);
  });
  open();
  const input = await screen.findByDisplayValue("Reply with a short greeting.");
  await user.clear(input);
  await user.type(input, " {Enter} ");
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
  await user.clear(input);
  await user.type(input, "unsaved fixture");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  await screen.findByText("fixture save failed");
  expect(input).toHaveValue("unsaved fixture");
  expect(states[gateway.providers[0].id].connectionPrompt).toBe(
    "Reply with a short greeting.",
  );
  expect(states[gateway.providers[0].id].jobs).toHaveLength(0);
});

it("confirms closing an unsent prompt and reports draft state to navigation", async () => {
  const user = userEvent.setup();
  const close = vi.fn();
  const dirty = vi.fn();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={close}
      onDirtyChange={dirty}
    />,
  );
  const input = await screen.findByDisplayValue("Reply with a short greeting.");
  await user.type(input, " fixture");
  expect(dirty).toHaveBeenLastCalledWith(true);
  await user.click(screen.getAllByRole("button", { name: "关闭" })[0]);
  await user.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "取消" },
    ),
  );
  expect(input).toHaveValue("Reply with a short greeting. fixture");
  expect(close).not.toHaveBeenCalled();
  await user.click(screen.getAllByRole("button", { name: "关闭" })[0]);
  await user.click(
    within(await screen.findByRole("dialog", { name: "确认操作" })).getByRole(
      "button",
      { name: "放弃修改" },
    ),
  );
  expect(close).toHaveBeenCalledOnce();
  expect(states[gateway.providers[0].id].connectionPrompt).toBe(
    "Reply with a short greeting.",
  );
});

it("shows live replies before diagnostics and keeps partial text after stopping", async () => {
  const user = userEvent.setup();
  reply = "";
  open();
  await screen.findByDisplayValue("Reply with a short greeting.");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  await screen.findByText("正在等待回复…");
  reply = "fixture first line\n第二行";
  await emit();
  const response = screen.getByRole("region", { name: "模型回复" });
  expect(within(response).getByText(/fixture first line/).textContent).toBe(
    reply,
  );
  expect(
    response.compareDocumentPosition(
      screen.getByRole("region", { name: "测试详情" }),
    ) & Node.DOCUMENT_POSITION_FOLLOWING,
  ).toBeTruthy();
  states[gateway.providers[0].id].jobs[0].status = "cancelled";
  await emit();
  expect(within(response).getByText("已停止")).toBeInTheDocument();
  await user.click(within(response).getByRole("button", { name: "复制回复" }));
  expect(await navigator.clipboard.readText()).toBe(reply);
  expect(
    within(response).getByRole("button", { name: "已复制" }),
  ).toBeInTheDocument();
});

it("reopens the active connection and explains missing output after restart", async () => {
  const provider = gateway.providers[0];
  const base = makeJob({
    clientId: "codex",
    providerId: provider.id,
    expectedRevision: "fixture",
    endpoint: provider.baseUrl,
    model: "fixture-model",
    kind: "connection",
    concurrency: 1,
  });
  states[provider.id].jobs = [
    { ...base, id: "newer-finished", status: "completed" },
    { ...base, id: "active-connection" },
  ];
  const first = open();
  await waitFor(() =>
    expect(mock.command).toHaveBeenCalledWith("provider_test_query", {
      id: "active-connection",
    }),
  );
  first.unmount();
  states[provider.id].jobs = [
    { ...base, id: "newer-finished", status: "completed" },
  ];
  outputAvailable = false;
  open();
  await screen.findByText("回复正文未保留，请重新测试。");
  expect(screen.getByRole("button", { name: "复制回复" })).toBeDisabled();
});

it("renders compact fingerprint revisions and readable sources", async () => {
  const user = userEvent.setup();
  const bank = states[gateway.providers[0].id].bank;
  bank.source = "builtin";
  bank.version = {
    revision: "123456789abcdef",
    sha256: "fixture-digest",
    analyzer_version: 1,
  };
  open();
  await user.click(screen.getByRole("tab", { name: "指纹库" }));
  await screen.findByText("1234567");
  expect(screen.getByText("内置")).toBeInTheDocument();
  expect(
    screen.queryByText(/fixture-digest|analyzer_version/),
  ).not.toBeInTheDocument();
  bank.source = "cache";
  await emit();
  expect(screen.getByText("已更新")).toBeInTheDocument();
  bank.source = undefined;
  bank.version = undefined;
  await emit();
  expect(screen.queryByText("1234567")).not.toBeInTheDocument();
  expect(screen.getByText("版本").nextElementSibling).toHaveTextContent("—");
});

it("opens only the selected provider and reads cache without fetching upstream", async () => {
  open();
  await screen.findByText("尚未缓存该端点模型，点击重新拉取。");
  expect(mock.command).toHaveBeenCalledWith("provider_test_state", {
    clientId: "codex",
    providerId: gateway.providers[0].id,
  });
  expect(mock.command).toHaveBeenCalledWith(
    "provider_test_models",
    expect.objectContaining({
      providerId: gateway.providers[0].id,
      endpoint: gateway.providers[0].baseUrl,
      refresh: false,
    }),
  );
  expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  expect(screen.queryByText("全选")).not.toBeInTheDocument();
  expect(screen.queryByLabelText("测试类型")).not.toBeInTheDocument();
  expect(screen.queryByLabelText("接口")).not.toBeInTheDocument();
  expect(screen.queryByRole("tab", { name: "预约" })).not.toBeInTheDocument();
});
it("refreshes only by explicit click and retains the cached model across reopen", async () => {
  const user = userEvent.setup();
  const first = open();
  await user.click(await screen.findByRole("button", { name: "重新拉取模型" }));
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  await user.click(screen.getByRole("tab", { name: "模型指纹" }));
  await user.click(screen.getByRole("tab", { name: "连接测试" }));
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
  first.unmount();
  open();
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  expect(
    mock.command.mock.calls.filter(
      ([name, args]) => name === "provider_test_models" && args.refresh,
    ),
  ).toHaveLength(1);
});
it("starts ordinary chat at the chosen auxiliary endpoint without changing the gateway", async () => {
  const user = userEvent.setup();
  const endpoint = gateway.providers[0].extraBaseUrls![0];
  seed(0, endpoint);
  open();
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试端点" }),
    endpoint,
  );
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: {
      clientId: "codex",
      providerId: gateway.providers[0].id,
      expectedRevision: "fixture-current",
      endpoint,
      model: "fixture-alt",
      kind: "connection",
      concurrency: 1,
      prompt: "Reply with a short greeting.",
    },
  });
  expect(
    await screen.findByText("fixture streamed result"),
  ).toBeInTheDocument();
  expect(screen.getByText(endpoint, { selector: "dd" })).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "停止测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_cancel", {
    id: `fixture-job-${gateway.providers[0].id}`,
  });
  expect(
    mock.command.mock.calls.some(
      ([name]) => name === "update_gateway" || name.includes("batch"),
    ),
  ).toBe(false);
});
it("allows default chat without a directory but requires a cached fingerprint model", async () => {
  const user = userEvent.setup();
  open();
  await screen.findByText("尚未缓存该端点模型，点击重新拉取。");
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "开始测试" })).toBeEnabled(),
  );
  await user.click(screen.getByRole("tab", { name: "模型指纹" }));
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
  await user.click(screen.getByRole("button", { name: "重新拉取模型" }));
  await screen.findByRole("option", { name: "Fixture (fixture-model)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "指纹并发" }),
    "3",
  );
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: expect.objectContaining({
      kind: "fingerprint",
      model: "fixture-model",
      concurrency: 3,
      providerId: gateway.providers[0].id,
    }),
  });
});
it("preserves the selected cached model and keeps testing available after refresh failure", async () => {
  seed();
  const user = userEvent.setup();
  open();
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  refreshFailure = true;
  await user.click(screen.getByRole("button", { name: "重新拉取模型" }));
  expect(await screen.findByRole("alert")).toHaveTextContent(
    "fixture upstream unavailable",
  );
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
  expect(screen.getByRole("button", { name: "开始测试" })).toBeEnabled();
});
it("preserves a removed model as unavailable after a successful explicit refresh", async () => {
  seed();
  const user = userEvent.setup();
  open();
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation((name, args) =>
    name === "provider_test_models" && args.refresh
      ? Promise.resolve({ ...catalog(), models: [catalog().models[0]] })
      : original(name, args),
  );
  await user.click(screen.getByRole("button", { name: "重新拉取模型" }));
  await screen.findByRole("option", { name: "fixture-alt（当前不可用）" });
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
});
it("ignores late catalog responses from a previously selected endpoint", async () => {
  const user = userEvent.setup();
  let finish!: (value: ProviderTestModels) => void;
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation((name, args) =>
    name === "provider_test_models" &&
    args.endpoint === gateway.providers[0].baseUrl
      ? new Promise((resolve) => {
          finish = resolve;
        })
      : original(name, args),
  );
  const endpoint = gateway.providers[0].extraBaseUrls![0];
  seed(0, endpoint);
  open();
  await waitFor(() => expect(finish).toBeTypeOf("function"));
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试端点" }),
    endpoint,
  );
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  await act(async () =>
    finish({
      ...catalog(),
      models: [{ id: "old-endpoint", name: "Old endpoint" }],
    }),
  );
  expect(
    screen.queryByRole("option", { name: "Old endpoint (old-endpoint)" }),
  ).not.toBeInTheDocument();
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
});
it("shows and copies the complete upstream error instead of only the code", async () => {
  const user = userEvent.setup();
  errorDetail = {
    httpStatus: 429,
    message: "上游返回 HTTP 429",
    body: `fixture quota details\n${"long detail ".repeat(400)}\nfixture error end`,
    truncated: false,
  };
  open();
  await waitFor(() =>
    expect(screen.getByRole("button", { name: "开始测试" })).toBeEnabled(),
  );
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  const error = await screen.findByRole("alert", { name: "完整错误信息" });
  expect(error.querySelector("pre")?.textContent).toBe(errorDetail.body);
  await user.click(within(error).getByRole("button", { name: "复制错误" }));
  expect(await navigator.clipboard.readText()).toContain("fixture error end");
});
it("never mixes another provider's records or cancellation into the current dialog", async () => {
  const user = userEvent.setup();
  const other = gateway.providers[1];
  const job = makeJob({
    clientId: "codex",
    providerId: other.id,
    expectedRevision: "fixture",
    endpoint: other.baseUrl,
    model: "foreign-model",
    kind: "connection",
    concurrency: 1,
  });
  states[gateway.providers[0].id].jobs.push(job);
  open();
  await user.click(screen.getByRole("tab", { name: "测试结果" }));
  expect(await screen.findByText("暂无测试结果")).toBeInTheDocument();
  expect(
    screen.queryByRole("button", { name: "停止" }),
  ).not.toBeInTheDocument();
});
it("saves independent detection settings and preserves a draft during runtime updates", async () => {
  seed();
  const user = userEvent.setup();
  open();
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  const minimum = await screen.findByRole("spinbutton", { name: "最小间隔" });
  await user.clear(minimum);
  await user.type(minimum, "17");
  await user.click(screen.getByRole("checkbox", { name: "启用定时检测" }));
  await emit();
  expect(minimum).toHaveValue(17);
  await user.click(screen.getByRole("button", { name: "保存" }));
  await screen.findByText("检测设置已保存");
  expect(mock.command).toHaveBeenCalledWith("provider_test_set_detection", {
    providerId: gateway.providers[0].id,
    expectedRevision: `fixture-settings-${gateway.providers[0].id}`,
    detection: expect.objectContaining({
      enabled: true,
      minIntervalMinutes: 17,
      endpoint: gateway.providers[0].baseUrl,
    }),
  });
  expect(states[gateway.providers[1].id].detection.minIntervalMinutes).toBe(5);
  expect(states[gateway.providers[1].id].detection.enabled).toBe(false);
});
it("allows disabling detection when its cached model or endpoint is unavailable", async () => {
  const user = userEvent.setup();
  const state = states[gateway.providers[0].id];
  state.detection.enabled = true;
  state.detection.endpoint = "https://removed.example.invalid/v1";
  open();
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  await user.click(
    await screen.findByRole("checkbox", { name: "启用定时检测" }),
  );
  await user.click(screen.getByRole("button", { name: "保存" }));
  await screen.findByText("检测设置已保存");
  expect(state.detection.enabled).toBe(false);
});
it("keeps conflicting detection drafts until an explicit reread", async () => {
  const user = userEvent.setup();
  open();
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  const input = await screen.findByRole("spinbutton", { name: "最小间隔" });
  await user.clear(input);
  await user.type(input, "12");
  states[gateway.providers[0].id].revision = "changed-by-another-window";
  await emit();
  await user.click(screen.getByRole("button", { name: "保存" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("检测设置已变化");
  expect(input).toHaveValue(12);
  await user.click(screen.getByRole("button", { name: "重新读取" }));
  await waitFor(() => expect(input).toHaveValue(5));
});
it("shows the provider's actual interval, start time and unavailable statistics", async () => {
  const user = userEvent.setup();
  const id = gateway.providers[0].id;
  states[id].detectionStatus = [
    {
      providerId: id,
      status: "waiting",
      model: "fixture-model",
      recentRequests: 65,
      intervalMinutes: 17,
      lastStartedAt: 1000,
      nextAt: 2000,
      cooldownUntil: null,
      error: null,
    },
  ];
  open();
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  expect(await screen.findByText("65 次")).toBeInTheDocument();
  expect(screen.getByText("17 分钟")).toBeInTheDocument();
  states[id].detectionStatus[0].recentRequests = null;
  states[id].detectionStatus[0].status = "statsUnavailable";
  await emit();
  expect(await screen.findByText("暂不可用")).toBeInTheDocument();
});
it("keeps search-only and non-Codex providers on ordinary connections", async () => {
  gateway.providers[0].searchOnly = true;
  const first = open();
  expect(
    screen.queryByRole("tab", { name: "模型指纹" }),
  ).not.toBeInTheDocument();
  expect(
    screen.queryByRole("tab", { name: "定时检测" }),
  ).not.toBeInTheDocument();
  first.unmount();
  gateway.clientId = "claude";
  gateway.providers[0].searchOnly = false;
  open();
  expect(
    screen.queryByRole("tab", { name: "模型指纹" }),
  ).not.toBeInTheDocument();
  expect(screen.getByRole("tab", { name: "连接测试" })).toBeInTheDocument();
});
it("keeps keyboard navigation inside the provider's testing tabs", async () => {
  const user = userEvent.setup();
  open();
  screen.getByRole("tab", { name: "连接测试" }).focus();
  await user.keyboard("{ArrowRight}");
  expect(screen.getByRole("tab", { name: "模型指纹" })).toHaveFocus();
  await user.keyboard("{End}");
  expect(screen.getByRole("tab", { name: "指纹库" })).toHaveFocus();
  await user.keyboard("{Home}");
  expect(screen.getByRole("tab", { name: "连接测试" })).toHaveFocus();
});
