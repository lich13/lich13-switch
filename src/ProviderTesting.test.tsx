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
import ProviderTesting from "./ProviderTesting";
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
  mode: "text",
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
          models: [
            { id: "fixture-upstream", name: "Fixture" },
            { id: "fixture-alt", name: "Alternate" },
          ],
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
      return null;
    },
  );
});
it("selects real models and starts ordinary chat with the current revision", async () => {
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  const select = screen.getByRole("combobox", { name: "测试模型" });
  expect(select.tagName).toBe("SELECT");
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  expect(select).toHaveValue("");
  expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  expect(screen.queryByLabelText("接口")).not.toBeInTheDocument();
  expect(screen.queryByLabelText("测试类型")).not.toBeInTheDocument();
  expect(screen.queryByLabelText("测试输入")).not.toBeInTheDocument();
  expect(document.querySelector('input[type="file"]')).toBeNull();
  expect(screen.queryByRole("tab", { name: "预约" })).not.toBeInTheDocument();
  await user.selectOptions(select, "fixture-alt");
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: {
      clientId: "codex",
      providerId: gateway.providers[0].id,
      expectedRevision: "fixture-current",
      kind: "connection",
      model: "fixture-alt",
      concurrency: 1,
    },
  });
  expect(
    await screen.findByText("fixture streamed result"),
  ).toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "停止测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_cancel", {
    id: "fixture-job",
  });
  expect(
    mock.command.mock.calls.some(([name]) => name === "update_gateway"),
  ).toBe(false);
});

it("reports missing business UA without automatically retrying on remount", async () => {
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

it("runs ordinary chat for search-only providers and excludes fingerprint testing", async () => {
  gateway.providers[0].searchOnly = true;
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await screen.findByRole("option", { name: "Fixture (fixture-upstream)" });
  expect(
    screen.queryByRole("tab", { name: "模型测试" }),
  ).not.toBeInTheDocument();
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: {
      clientId: "codex",
      providerId: gateway.providers[0].id,
      expectedRevision: "fixture-current",
      kind: "connection",
      model: "",
      concurrency: 1,
    },
  });
});

it("starts and stops a batch with a separate default for each selected provider", async () => {
  const user = userEvent.setup();
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("checkbox", { name: "全选" }));
  await user.click(screen.getByRole("button", { name: "批量测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_batch", {
    requests: gateway.providers.map((p) => ({
      clientId: "codex",
      providerId: p.id,
      expectedRevision: "fixture-current",
      kind: "connection",
      model: "",
      concurrency: 1,
    })),
  });
  await user.click(await screen.findByRole("button", { name: "停止批次" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_stop_batch", {
    id: "fixture-batch",
  });
});

it("keeps detection drafts across background events and saves against their original revision", async () => {
  const user = userEvent.setup();
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  const enabled = await screen.findByLabelText("启用定时检测");
  await user.click(enabled);
  state.revision = "new-background-revision";
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

it.each(["claude", "grok", "dsh"] as const)(
  "%s exposes only connection and results tabs",
  async (clientId) => {
    gateway.clientId = clientId;
    render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
    expect(screen.getAllByRole("tab").map((t) => t.textContent)).toEqual([
      "连接测试",
      "测试结果",
    ]);
    expect(
      screen.queryByRole("combobox", { name: "接口" }),
    ).not.toBeInTheDocument();
  },
);

it("forces catalog refresh without replacing a user-selected model", async () => {
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  const original = mock.command.getMockImplementation()!;
  let finish!: (value: unknown) => void;
  mock.command.mockImplementation((name, args) =>
    name === "provider_test_models"
      ? new Promise((resolve) => {
          finish = resolve;
        })
      : original(name, args),
  );
  await user.click(screen.getByRole("button", { name: "刷新模型目录" }));
  await waitFor(() => expect(finish).toBeTypeOf("function"));
  expect(mock.command).toHaveBeenCalledWith(
    "provider_test_models",
    expect.objectContaining({ refresh: true }),
  );
  await act(async () =>
    finish({
      models: [
        { id: "fixture-alt", name: "Alternate" },
        { id: "fixture-new", name: "New" },
      ],
      defaultModel: "fixture-new",
      stale: false,
      error: null,
    }),
  );
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
});

it("ignores a late catalog response after switching the selected provider", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  const pendingModels = new Map<string, (value: unknown) => void>();
  mock.command.mockImplementation((name, args) =>
    name === "provider_test_models"
      ? new Promise((resolve) =>
          pendingModels.set(String(args.providerId), resolve),
        )
      : original(name, args),
  );
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(
    screen.getByRole("checkbox", { name: gateway.providers[0].name }),
  );
  await waitFor(() =>
    expect(pendingModels.has(gateway.providers[0].id)).toBe(true),
  );
  await user.click(
    screen.getByRole("checkbox", { name: gateway.providers[1].name }),
  );
  await user.click(
    screen.getByRole("checkbox", { name: gateway.providers[0].name }),
  );
  await waitFor(() =>
    expect(pendingModels.has(gateway.providers[1].id)).toBe(true),
  );
  const catalog = (id: string) => ({
    models: [{ id, name: id }],
    defaultModel: id,
    stale: false,
    error: null,
  });
  await act(async () =>
    pendingModels.get(gateway.providers[1].id)!(catalog("fixture-second")),
  );
  await act(async () =>
    pendingModels.get(gateway.providers[0].id)!(catalog("fixture-first")),
  );
  expect(
    screen.getByRole("option", { name: "fixture-second" }),
  ).toBeInTheDocument();
  expect(
    screen.queryByRole("option", { name: "fixture-first" }),
  ).not.toBeInTheDocument();
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue("");
});

it("preserves a selected model on refresh failure and permits only default connection testing", async () => {
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      close={vi.fn()}
    />,
  );
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation((name, args) =>
    name === "provider_test_models"
      ? Promise.reject(new Error("目录读取失败"))
      : original(name, args),
  );
  await user.click(screen.getByRole("button", { name: "刷新模型目录" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("目录读取失败");
  expect(screen.getByRole("combobox", { name: "测试模型" })).toHaveValue(
    "fixture-alt",
  );
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "",
  );
  expect(screen.getByRole("button", { name: "开始测试" })).toBeEnabled();
  await user.click(screen.getByRole("tab", { name: "模型测试" }));
  expect(screen.getByRole("button", { name: "开始测试" })).toBeDisabled();
});

it("opens fingerprint testing directly and submits the selected model and concurrency", async () => {
  const user = userEvent.setup();
  render(
    <ProviderTesting
      gateway={gateway}
      provider={gateway.providers[0]}
      initialKind="fingerprint"
      close={vi.fn()}
    />,
  );
  await screen.findByRole("option", { name: "Alternate (fixture-alt)" });
  expect(screen.getByRole("tab", { name: "模型测试" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
  await user.selectOptions(
    screen.getByRole("combobox", { name: "测试模型" }),
    "fixture-alt",
  );
  await user.selectOptions(
    screen.getByRole("combobox", { name: "组并发" }),
    "3",
  );
  await user.click(screen.getByRole("button", { name: "开始测试" }));
  expect(mock.command).toHaveBeenCalledWith("provider_test_start", {
    request: expect.objectContaining({
      kind: "fingerprint",
      model: "fixture-alt",
      concurrency: 3,
    }),
  });
  const tabs = screen.getByRole("tablist");
  within(tabs).getByRole("tab", { name: "模型测试" }).focus();
  await user.keyboard("{ArrowRight}");
  expect(screen.getByRole("tab", { name: "测试结果" })).toHaveAttribute(
    "aria-selected",
    "true",
  );
});

it("keeps all completed jobs in the current batch visible", async () => {
  const user = userEvent.setup();
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("checkbox", { name: "全选" }));
  await user.click(screen.getByRole("button", { name: "批量测试" }));
  await screen.findByRole("button", { name: "停止批次" });
  state.jobs.forEach((job) => {
    job.status = "completed";
    job.completedAt = Date.now() - 10_000;
  });
  await act(async () => {
    mock.listeners.get("provider-tests")?.();
  });
  expect(
    screen.getAllByRole("button", { name: "fixture provider 普通对话 已完成" }),
  ).toHaveLength(2);
});

it("reloads authoritative detection settings and uses the saved revision for the next edit", async () => {
  const user = userEvent.setup();
  const original = mock.command.getMockImplementation()!;
  mock.command.mockImplementation(async (name, args) => {
    if (name === "provider_test_set_detection") {
      state.detection = structuredClone(args.detection);
      state.revision = "fixture-after-save";
      return null;
    }
    return original(name, args);
  });
  render(<ProviderTesting gateway={gateway} close={vi.fn()} />);
  await user.click(screen.getByRole("tab", { name: "定时检测" }));
  const input = await screen.findByLabelText("全局检测模型");
  state.detection.model = "fixture-external-model";
  state.revision = "fixture-external-revision";
  await user.click(screen.getByRole("button", { name: "重新读取" }));
  await waitFor(() => expect(input).toHaveValue("fixture-external-model"));
  await user.clear(input);
  await user.type(input, "fixture-saved-model");
  await user.click(screen.getByRole("button", { name: "保存检测设置" }));
  await screen.findByText("检测设置已保存");
  expect(input).toHaveValue("fixture-saved-model");
  await user.click(screen.getByRole("button", { name: "保存检测设置" }));
  const writes = mock.command.mock.calls.filter(
    ([name]) => name === "provider_test_set_detection",
  );
  expect(writes[0][1].expectedRevision).toBe("fixture-external-revision");
  expect(writes[1][1].expectedRevision).toBe("fixture-after-save");
});
