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
    successThreshold: 2,
    cooldownSeconds: 60,
    rateLimitSeconds: 5,
    capacityRetrySeconds: 60,
    websocketRetrySeconds: 60,
    handoffAfterCompaction: true,
    errorRate: 0.6,
    minRequests: 10,
    firstByteSeconds: 60,
    idleSeconds: 120,
    totalSeconds: 600,
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
      supportsWebsocket: true,
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
      supportsWebsocket: true,
    },
  ],
  activeConnections: 0,
  waitingRequests: 0,
  capacityRetries: [],
  error: null,
  recoveryPending: false,
};
if (densePreview) {
  gatewayDemo.providers.push(...Array.from({ length: 12 }, (_, index) => ({
    ...structuredClone(gatewayDemo.providers[index % 2]),
    id: `fixture-provider-${index + 1}`,
    name: `团队供应商 ${index + 1} · 跨区域研发与长名称显示`,
    baseUrl: "https://provider.example.invalid/v1",
  })));
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
export function gatewayPreview(name: string, args: Record<string, unknown>) {
  const s = args.clientId === "claude" ? claudeGatewayDemo : gatewayDemo;
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
      source: provider.id === "primary" ? "sub2api" : "newapi",
      checkedAt: Math.floor(Date.now() / 1000),
      successAt: Math.floor(Date.now() / 1000),
      retryAt: null,
      stale: false,
      error: null,
      keyStatus: null,
      plans: [
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
      totalUsage: null,
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
          supportsWebsocket: true,
        });
    }
    if (["modelsProvider", "policyProvider"].includes(e.op as string) && p)
      p.allowedModels = e.allowedModels as string[] | null;
    if (["websocketProvider", "policyProvider"].includes(e.op as string) && p)
      p.supportsWebsocket = Boolean(e.supportsWebsocket);
    if (e.op === "settings") s.settings = e.settings as GatewaySettings;
    if (e.op === "import") throw new Error("预览模式无法读取真实客户端配置");
    s.revision = crypto.randomUUID();
  }
  return structuredClone(s);
}
