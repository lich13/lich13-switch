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
  address: "http://127.0.0.1:15722/v1",
  mode: "manual",
  selected: "primary",
  lastSuccessful: null,
  configRevision: "preview-config",
  configProvider: "primary",
  configState: "provider",
  configError: null,
  settings: {
    port: 15722,
    maxRetries: 3,
    failureThreshold: 4,
    transientFailureThreshold: 6,
    successThreshold: 2,
    cooldownSeconds: 60,
    rateLimitSeconds: 5,
    capacityRetrySeconds: 60,
    websocketRetrySeconds: 60,
    errorRate: 0.6,
    minRequests: 10,
    firstByteSeconds: 600,
    idleSeconds: 600,
    totalSeconds: 600,
    threadIdleReleaseSeconds: 300,
    endpointProbeSeconds: 300,
    connectSeconds: 15,
    queueSeconds: 30,
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
  address: "http://127.0.0.1:15723",
  settings: { ...gatewayDemo.settings, port: 15723 },
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
export function gatewayPreview(name: string, args: Record<string, unknown>) {
  const s = args.clientId === "claude" ? claudeGatewayDemo : gatewayDemo;
  if (name === "preview_sub2api_import")
    return {
      token: "preview-sub2api",
      revision: s.revision,
      unsupportedProxies: 1,
      template: { ready: false, token: "preview-import-template", rows: [
        { id: 0, name: "标准并发模板" }, { id: 1, name: "自定义模型映射模板" },
      ] },
      rows: [
        {
          id: 0,
          name: "导入供应商",
          baseUrl: "https://import.example.invalid/v1",
          concurrency: 3,
          error: null,
        },
        {
          id: 1,
          name: "OAuth 账号",
          baseUrl: null,
          concurrency: null,
          error: "仅支持 API Key，OAuth 账号不支持",
        },
      ],
    };
  if (name === "prepare_sub2api_export")
    return { ready: !args.replaceTemplate, token: args.replaceTemplate ? "preview-template" : null, rows: [
      { id: 0, name: "标准并发模板" }, { id: 1, name: "自定义模型映射模板" },
    ] };
  if (name === "export_sub2api_providers" || name === "select_sub2api_template")
    return true;
  if (name === "cancel_sub2api_import") return null;
  if (name === "commit_sub2api_import") {
    if (args.expectedRevision !== s.revision)
      throw { code: "CONFLICT", message: "网关设置已变化，请使用最新状态重试" };
    s.providers.push({
      ...s.providers[0],
      id: crypto.randomUUID(),
      name: "导入供应商",
      baseUrl: "https://import.example.invalid/v1",
      queued: false,
      handoffAfterCompaction: false,
      takeNewThreads: false,
    });
    s.revision = crypto.randomUUID();
    return structuredClone(s);
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
  }
  if (name === "stop_gateway") {
    s.running = false;
    s.configState = "provider";
    s.configProvider = s.selected;
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
      if (p) p.baseUrl = String(e.baseUrl);
      else
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
    if (e.op === "settings") s.settings = e.settings as GatewaySettings;
    s.revision = crypto.randomUUID();
  }
  return structuredClone(s);
}
