import type {
  GatewayModels,
  DshFileView,
  DshFileKey,
} from "./bridge-contracts";
import type { GatewayState, GatewaySettings, ProviderQuota } from "./types";
import { densePreview } from "./preview-fixtures";
const healthy = {
  state: "closed" as const,
  failures: 0,
  requests: 0,
  retryIn: 0,
};
export const gatewayDemo: GatewayState = {
  clientId: "codex",
  revision: "preview-gateway",
  running: false,
  address: "http://127.0.0.1:43721/v1",
  listener: { port: 43721, revision: "preview-listener", runningClients: [] },
  mode: "manual",
  selected: "primary",
  lastSuccessful: null,
  configRevision: "preview-config",
  configProvider: "primary",
  configState: "provider",
  configError: null,
  settings: {
    port: 43721,
    maxRetries: 3,
    failureThreshold: 4,
    transientFailureThreshold: 6,
    successThreshold: 2,
    cooldownSeconds: 60,
    rateLimitSeconds: 5,
    capacityRetrySeconds: 60,
    websocketRetrySeconds: 300,
    errorRate: 0.6,
    minRequests: 10,
    firstByteSeconds: 300,
    idleSeconds: 300,
    retryBudgetSeconds: 300,
    threadIdleReleaseSeconds: 300,
    endpointProbeSeconds: 300,
    connectSeconds: 300,
    queueSeconds: 300,
    maxWaiting: 100,
  },
  providers: [
    {
      id: "primary",
      name: "api.example.com",
      baseUrl: "https://api.example.com/v1",
      queued: true,
      health: { ...healthy },
      quotaVersion: "preview-quota",
      quota: null,
      maxConcurrency: 4,
      maxRpm: 60,
      activeRequests: 0,
      rpmUsed: 0,
      rpmRetryIn: 0,
      rpmLimited: false,
      allowedModels: null,
      handoffAfterCompaction: false,
      takeNewThreads: false,
    },
    {
      id: "backup",
      name: "备用工作空间 · 长名称供应商的列表与键盘操作验证",
      baseUrl: "https://backup.example.com/api/v1",
      queued: true,
      health: { ...healthy },
      quotaVersion: "preview-quota",
      quota: null,
      maxConcurrency: 0,
      maxRpm: 0,
      activeRequests: 0,
      rpmUsed: 0,
      rpmRetryIn: 0,
      rpmLimited: false,
      allowedModels: null,
      handoffAfterCompaction: false,
      takeNewThreads: false,
    },
  ],
  activeConnections: 0,
  waitingRequests: 0,
  capacityRetries: [],
  error: null,
  recoveryPending: false,
};
if (densePreview) {
  gatewayDemo.providers[0].modelHealth = [
    {
      model: "gpt-6-luna",
      health: {
        ...healthy,
        available: false,
        retryIn: 60,
        cooldownReason: "capacity_retry",
        cause: {
          eventId: "fixture-model-capacity",
          code: "rate_limit_exceeded",
          reason: "rate_limit",
          status: 429,
          wsCloseCode: null,
        },
      },
    },
  ];
  gatewayDemo.providers.push(
    ...Array.from({ length: 12 }, (_, index) => ({
      ...structuredClone(gatewayDemo.providers[index % 2]),
      id: `fixture-provider-${index + 1}`,
      name: `团队供应商 ${index + 1} · 跨区域研发与长名称显示`,
      baseUrl: "https://provider.example.invalid/v1",
    })),
  );
}
export const claudeGatewayDemo: GatewayState = {
  ...structuredClone(gatewayDemo),
  clientId: "claude",
  revision: "preview-claude",
  address: "http://127.0.0.1:43721",
  settings: { ...gatewayDemo.settings },
  selected: "claude-primary",
  configProvider: "claude-primary",
  providers: gatewayDemo.providers.map((p, i) => ({
    ...structuredClone(p),
    id: `claude-${p.id}`,
    name: i ? "Claude 备用供应商" : "claude.example.com",
    baseUrl: "https://claude.example.com",
    allowedModels: null,
  })),
};
if (densePreview) {
  gatewayDemo.mode = "auto";
  gatewayDemo.running = true;
  gatewayDemo.configState = "gateway";
  gatewayDemo.configProvider = null;
  const [primary, backup, receiver] = gatewayDemo.providers;
  Object.assign(primary, {
    handoffAfterCompaction: true,
    activeRequests: 1,
    reservedThreads: 2,
  });
  Object.assign(backup, {
    handoffAfterCompaction: true,
    activeRequests: 1,
    reservedThreads: 1,
    maxConcurrency: 2,
  });
  receiver.handoffAfterCompaction = true;
  gatewayDemo.activeConnections = 2;
  gatewayDemo.handoffWaiting = [
    { providerId: primary.id, state: "awaiting_compaction" },
    { providerId: backup.id, state: "capacity_full" },
    { providerId: receiver.id, state: "awaiting_request" },
  ];
}
export const grokGatewayDemo: GatewayState = {
  ...structuredClone(gatewayDemo),
  clientId: "grok",
  revision: "preview-grok",
  address: "http://127.0.0.1:43721/v1",
  settings: { ...gatewayDemo.settings },
  handoffWaiting: [],
  providers: gatewayDemo.providers.map((p) => ({
    ...structuredClone(p),
    protocol: "responses",
    handoffAfterCompaction: false,
    takeNewThreads: false,
    reservedThreads: 0,
  })),
};
export const dshGatewayDemo: GatewayState = {
  ...structuredClone(grokGatewayDemo),
  clientId: "dsh",
  revision: "preview-dsh",
  providers: [],
  running: false,
};
export const agyGatewayDemo: GatewayState = {
  ...structuredClone(grokGatewayDemo),
  clientId: "agy",
  revision: "preview-agy",
  address: "http://127.0.0.1:43721",
  configRevision: null,
  providers: grokGatewayDemo.providers.map((p) => ({
    ...p,
    protocol: "gemini_native",
  })),
};
function syncListener(port = gatewayDemo.settings.port, changed = false) {
  const states = [
    gatewayDemo,
    claudeGatewayDemo,
    grokGatewayDemo,
    dshGatewayDemo,
    agyGatewayDemo,
  ];
  const revision = changed
    ? crypto.randomUUID()
    : gatewayDemo.listener!.revision;
  const runningClients = states
    .filter((state) => state.running)
    .map((state) => state.clientId);
  for (const state of states) {
    state.settings.port = port;
    state.listener = { port, revision, runningClients };
    state.address = `http://127.0.0.1:${port}${state.clientId === "claude" || state.clientId === "agy" ? "" : "/v1"}`;
  }
}
syncListener();
const modelViews = new Map<string, GatewayModels>();
const dshFiles = new Map<string, DshFileView>();
const dshPrevious = new Map<string, string>();
const modelIndex = {
  updatedAt: 1,
  version: "fixture-index",
  providers: 1,
  models: 1,
  error: null,
};
export function gatewayPreview(name: string, args: Record<string, unknown>) {
  const s =
    args.clientId === "agy"
      ? agyGatewayDemo
      : args.clientId === "dsh"
        ? dshGatewayDemo
        : args.clientId === "claude"
          ? claudeGatewayDemo
          : args.clientId === "grok"
            ? grokGatewayDemo
            : gatewayDemo;
  if (["get_models_dev", "update_models_dev"].includes(name)) return modelIndex;
  if (name === "search_models_dev")
    return {
      hits: [
        {
          source: "fixture/fixture-model",
          provider: "fixture",
          providerName: "Fixture",
          model: "fixture-model",
          name: "Fixture model",
          context: 128000,
          output: 8192,
          cost: { input: 1, output: 2 },
        },
      ],
      error: null,
    };
  if (
    [
      "get_gateway_models",
      "refresh_gateway_models",
      "edit_model_list",
    ].includes(name)
  ) {
    let view = modelViews.get(s.clientId);
    if (!view) {
      view = {
        clientId: s.clientId,
        passthrough: s.clientId === "codex",
        hint:
          s.clientId === "codex"
            ? "Codex 目录透传，本列表只读"
            : "DSH 接入或同步时写入模型列表",
        models: [
          {
            id: "fixture-model",
            providers: s.providers
              .slice(0, 1)
              .map((p) => ({ id: p.id, name: p.name })),
            manual: false,
            source: null,
            pinned: null,
            metadata: {
              sources: ["fixture"],
              model: "fixture-model",
              matchedBy: "exact",
              name: "Fixture model",
              reasoning: null,
              reasoningEfforts: null,
              context: 128000,
              output: 8192,
            },
            price: {
              source: "models.dev",
              inputPerM: "1",
              outputPerM: "2",
              cacheReadPerM: "0.1",
              cacheWritePerM: null,
              deviation: false,
            },
          },
        ],
        providers: s.providers
          .filter((p) => p.queued)
          .map((p) => ({
            id: p.id,
            name: p.name,
            count: 1,
            checkedAt: 1,
            stale: false,
            error: null,
          })),
        modelsDev: modelIndex,
        error: null,
      };
      modelViews.set(s.clientId, view);
    }
    if (name === "edit_model_list" && !view.passthrough) {
      const id = String(args.id),
        model = view.models.find((m) => m.id === id);
      if (args.op === "add") {
        if (model) model.manual = true;
        else
          view.models.push({
            id,
            providers: [],
            manual: true,
            source: typeof args.source === "string" ? args.source : null,
            pinned: null,
            metadata: null,
            price: null,
          });
      } else if (args.op === "remove") {
        if (model?.providers.length) model.manual = false;
        else view.models = view.models.filter((m) => m.id !== id);
      } else if (model) {
        model.pinned = args.op === "pin" ? String(args.source) : null;
        model.source = model.pinned;
      }
    }
    return structuredClone(view);
  }
  if (
    [
      "read_dsh_file",
      "save_dsh_file",
      "read_previous_dsh_file",
      "restore_dsh_file",
    ].includes(name)
  ) {
    const id = `${args.profile ?? "desktop"}/${args.key}`;
    const readonly = args.key === "environment" || args.key === "credentials";
    let file = dshFiles.get(id) ?? {
      profiles: ["desktop", "default"],
      profile: String(args.profile ?? "desktop"),
      key: args.key as DshFileKey,
      files: [
        "profilePatch",
        "homePatch",
        "environment",
        "credentials",
      ] as DshFileKey[],
      text: readonly ? "FIXTURE_KEY: ••••" : "# Fixture configuration\n[]\n",
      revision: "fixture-file",
      readOnly: readonly,
      canRestore: false,
    };
    if (name === "read_previous_dsh_file") return dshPrevious.get(id) ?? "[]\n";
    if (name === "save_dsh_file" || name === "restore_dsh_file") {
      if (readonly) throw { code: "CONFIG", message: "此文件只读" };
      if (args.expectedRevision !== file.revision)
        throw { code: "CONFLICT", message: "文件已变化，草稿已保留" };
      const text =
        name === "restore_dsh_file"
          ? (dshPrevious.get(id) ?? "[]\n")
          : String(args.text);
      dshPrevious.set(id, file.text);
      file = {
        ...file,
        text,
        revision: `fixture-${Date.now()}`,
        canRestore: true,
      };
    }
    dshFiles.set(id, file);
    return structuredClone(file);
  }
  if (name === "dsh_official_models")
    return {
      models: [
        { id: "deepseek-flash", name: "DeepSeek-V4.1-Flash" },
        { id: "deepseek-v4-pro", name: "DeepSeek-V4-Pro" },
      ],
      error: null,
    };
  if (name === "preview_dsh_profile")
    return {
      profiles: ["default", "desktop"],
      profile: args.profile ?? "desktop",
      revision: "preview-dsh-profile",
      models: [],
      attached: dshGatewayDemo.running,
      protocol: args.protocol,
      model: null,
      makeDefault: true,
      current: {
        kind: "unmatched",
        providerName: "Fixture",
        model: "fixture-model",
        modelSource: "profile desktop",
        warning: null,
        protocol: "chat_completions",
      },
      states: ["desktop", "default"].map((profile) => ({
        profile,
        current: {
          kind: "unmatched",
          providerName: "Fixture",
          model: "fixture-model",
          modelSource: `profile ${profile}`,
          warning: null,
        },
      })),
      warning: "默认模型仅影响新会话",
    };
  if (name === "attach_dsh_profile" || name === "attach_dsh_profiles") {
    dshGatewayDemo.running = true;
    dshGatewayDemo.dshProfile = args.options as GatewayState["dshProfile"];
    syncListener();
    if (name === "attach_dsh_profiles")
      return {
        state: structuredClone(dshGatewayDemo),
        results: (args.all
          ? ["default", "desktop"]
          : [dshGatewayDemo.dshProfile?.profile]
        ).map((profile) => ({ profile, status: "success", error: null })),
      };
    return structuredClone(dshGatewayDemo);
  }
  if (name === "get_gateway_listener") return gatewayDemo.listener;
  if (name === "cancel_grok_import") return null;
  if (name === "preview_grok_import")
    return {
      token: args.useEnvironment ? "preview-grok-import" : null,
      revision: grokGatewayDemo.revision,
      selected: "fixture-model",
      models: [
        {
          key: "fixture-model",
          model: "fixture-model",
          protocol: "responses",
          address: "https://example.invalid/v1",
          addressSource: "global",
          credentialAvailable: !!args.useEnvironment,
          missingEnvironment: args.useEnvironment ? [] : ["XAI_API_KEY"],
          error: null,
        },
      ],
      environmentAvailable: true,
      environmentLoaded: !!args.useEnvironment,
      clearEnvironment: ["GROK_MODELS_BASE_URL", "XAI_API_KEY"],
      error: args.useEnvironment
        ? null
        : "未读取到 API 凭据；可确认读取 endpoint.env",
    };
  if (name === "commit_grok_import") return structuredClone(grokGatewayDemo);
  if (name === "set_gateway_listener") {
    const port = Number(args.port);
    if (gatewayDemo.listener!.runningClients.length)
      throw new Error("请先停用全部客户端网关，再修改共享端口");
    syncListener(port, true);
    return gatewayDemo.listener;
  }
  if (name === "probe_provider_endpoints") {
    const p = s.providers.find((p) => p.id === args.providerId);
    if (!p) throw new Error("供应商不存在");
    p.endpointMeasurements = [p.baseUrl, ...(p.extraBaseUrls ?? [])].map(
      (baseUrl, i) => ({
        baseUrl,
        latencyMs: i ? 48 : 96,
        status: 401,
        error: null,
        checkedAt: Date.now(),
      }),
    );
    p.preferredBaseUrl = p.extraBaseUrls?.[0] ?? p.baseUrl;
    return structuredClone(s);
  }
  if (name === "start_gateway") {
    s.running = true;
    s.configState = "gateway";
    s.configProvider = null;
    syncListener();
  }
  if (name === "stop_gateway") {
    s.running = false;
    s.configState = "provider";
    s.configProvider = s.selected;
    syncListener();
  }
  if (name === "query_provider_quota") {
    const provider = s.providers.find((p) => p.id === args.providerId)!;
    const result: ProviderQuota = {
      providerId: provider.id,
      version: provider.quotaVersion,
      state: "ok",
      source: provider.id.endsWith("primary") ? "sub2api" : "newapi",
      checkedAt: Math.floor(Date.now() / 1000),
      successAt: Math.floor(Date.now() / 1000),
      retryAt: null,
      stale: false,
      error: null,
      keyStatus: null,
      subscriptions: provider.id.endsWith("primary")
        ? [
            {
              name: "开发者套餐",
              status: "active",
              active: true,
              expiresAt: "2027-12-31T00:00:00Z",
              plans: [
                {
                  name: "每日",
                  remaining: 8,
                  used: 2,
                  total: 10,
                  unit: "USD",
                  unlimited: false,
                  resetAt: null,
                },
                {
                  name: "每周",
                  remaining: 42,
                  used: 8,
                  total: 50,
                  unit: "USD",
                  unlimited: false,
                  resetAt: null,
                },
              ],
            },
          ]
        : [
            {
              name: "团队研发与长周期协作套餐",
              status: "active",
              active: true,
              expiresAt: null,
              plans: [
                {
                  name: "每月",
                  remaining: 800,
                  used: 200,
                  total: 1000,
                  unit: "额度单位",
                  unlimited: false,
                  resetAt: null,
                },
              ],
            },
            {
              name: "开发者加量套餐",
              status: "active",
              active: true,
              expiresAt: null,
              plans: [
                {
                  name: "套餐额度",
                  remaining: 5,
                  used: 0,
                  total: 5,
                  unit: "USD",
                  unlimited: false,
                  resetAt: null,
                },
              ],
            },
          ],
      plans: provider.id.endsWith("primary")
        ? [
            {
              name: "开发者套餐",
              remaining: 8,
              used: 2,
              total: 10,
              unit: "USD",
              unlimited: false,
              resetAt: null,
            },
            {
              name: "每日",
              remaining: 8,
              used: 2,
              total: 10,
              unit: "USD",
              unlimited: false,
              resetAt: null,
            },
            {
              name: "每周",
              remaining: 42,
              used: 8,
              total: 50,
              unit: "USD",
              unlimited: false,
              resetAt: null,
            },
          ]
        : [
            {
              name: "API Key 配额",
              remaining: 42.35,
              used: 7.65,
              total: 50,
              unit: "USD",
              unlimited: false,
              resetAt: null,
            },
          ],
      expiresAt: null,
      expiresAtUnix: null,
      today: { requests: 18, tokens: 56200, cost: 1.25 },
      totalUsage: { requests: 2400, tokens: 9600000, cost: 28.5 },
    };
    provider.quota = result;
    return result;
  }
  if (name === "update_gateway") {
    const e = args.edit as Record<string, unknown>,
      id = String(e.id),
      p = s.providers.find((p) => p.id === id);
    if (e.op === "mode") s.mode = e.mode as "auto" | "manual";
    if (e.op === "select") {
      s.selected = id;
      s.mode = "manual";
      if (!s.running) {
        s.configState = "provider";
        s.configProvider = id;
      }
    }
    if (e.op === "queueProvider" && p) p.queued = Boolean(e.queued);
    if (e.op === "concurrencyProvider" && p)
      p.maxConcurrency = Number(e.maxConcurrency);
    if (e.op === "rpmProvider" && p) p.maxRpm = Number(e.maxRpm);
    if (e.op === "renameProvider" && p) p.name = String(e.name);
    if (e.op === "endpointsProvider" && p) {
      p.extraBaseUrls = [
        ...new Set(
          (e.baseUrls as string[]).map((url) =>
            new URL(url).href.replace(/\/$/, ""),
          ),
        ),
      ].filter((url) => url !== p.baseUrl.replace(/\/$/, ""));
      p.endpointMeasurements = [];
      p.preferredBaseUrl = p.baseUrl;
    }
    if (e.op === "swapEndpoint" && p) {
      const index = p.extraBaseUrls?.indexOf(String(e.baseUrl)) ?? -1;
      if (index < 0) throw new Error("辅助端点已变化，请刷新后重试");
      const previous = p.baseUrl;
      p.baseUrl = p.extraBaseUrls![index];
      p.extraBaseUrls![index] = previous;
      p.endpointMeasurements = [];
      p.preferredBaseUrl = p.baseUrl;
    }
    if (e.op === "reset" && p) p.health = { ...healthy };
    if (e.op === "deleteProvider")
      s.providers = s.providers.filter((p) => p.id !== id);
    if (e.op === "reorder")
      s.providers.sort(
        (a, b) =>
          (e.ids as string[]).indexOf(a.id) - (e.ids as string[]).indexOf(b.id),
      );
    if (e.op === "saveProvider") {
      if (p) {
        p.baseUrl = String(e.baseUrl);
        if (e.searchOnly !== undefined) p.searchOnly = Boolean(e.searchOnly);
      } else
        s.providers.push({
          id: crypto.randomUUID(),
          name: new URL(String(e.baseUrl)).hostname,
          baseUrl: String(e.baseUrl),
          queued: true,
          health: { ...healthy },
          quotaVersion: crypto.randomUUID(),
          quota: null,
          maxConcurrency: 0,
          maxRpm: 0,
          activeRequests: 0,
          rpmUsed: 0,
          rpmRetryIn: 0,
          rpmLimited: false,
          allowedModels: null,
          handoffAfterCompaction: false,
          takeNewThreads: false,
        });
    }
    if (["modelsProvider", "policyProvider"].includes(e.op as string) && p)
      p.allowedModels = e.allowedModels as string[] | null;
    if (["policyProvider", "takeoverProvider"].includes(e.op as string) && p) {
      p.handoffAfterCompaction =
        e.handoffAfterCompaction == null
          ? p.handoffAfterCompaction
          : Boolean(e.handoffAfterCompaction);
      p.takeNewThreads =
        e.takeNewThreads == null ? p.takeNewThreads : Boolean(e.takeNewThreads);
    }
    if (e.op === "connection") {
      if (s.clientId !== "codex" || s.running)
        throw new Error("请先关闭 Codex 网关再修改连接方式");
      if (e.mode !== "auto" && e.mode !== "apiKey" && e.mode !== "bearer")
        throw new Error("连接方式无效");
      if (e.mode !== "auto") s.connectionMode = e.mode;
    }
    if (e.op === "settings") {
      const settings = e.settings as GatewaySettings;
      if (settings.port !== s.settings.port) {
        if (gatewayDemo.listener!.runningClients.length)
          throw new Error("请先停用全部客户端网关，再修改共享端口");
        syncListener(settings.port, true);
      }
      s.settings = settings;
    }
    s.revision = crypto.randomUUID();
  }
  return structuredClone(s);
}
