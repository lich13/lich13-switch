export type ClientId = "codex" | "claude";
export type Preferences = {
  codexHome: string;
  claudeHome?: string;
  cliPath: string;
  theme: "system" | "dark" | "light";
  quotaRefreshSeconds?: number;
  systemNotifications?: boolean;
};
export type Account = {
  id: string;
  name: string;
  kind: "chatgpt" | "apiKey";
  email: string | null;
  current: boolean;
  updatedAt: number;
  credentialRevision?: string;
};
export type ViewState = {
  accounts: Account[];
  authRevision: string;
  configRevision: string;
  currentState: "saved" | "unsaved" | "missing" | "invalid";
  preferences: Preferences;
  authSync?: {
    state: string;
    accountId: string | null;
    message: string;
    at: number;
  } | null;
  authSource: {
    provider: string;
    credentialStore: string;
    inlineToken: boolean;
    envKey: boolean;
    commandAuth: boolean;
    requiresOpenaiAuth: boolean;
    warning: string | null;
  };
  error: string | null;
  officialMode?: OfficialMode;
};
export type OfficialMode = {
  enabled: boolean;
  state: "disabled" | "enabled" | "conflict" | "unavailable" | string;
  accountId: string | null;
  error: string | null;
};
export type ConfigDocument = {
  clientId: ClientId;
  text: string;
  revision: string;
  path: string;
  guarded: boolean;
  canRestore: boolean;
};
export type LoginState = {
  phase: string;
  mode: string;
  url: string | null;
  code: string | null;
  message: string;
  callbackReady: boolean;
  targetAccountId?: string | null;
};
export type AppError = {
  code: string;
  message: string;
  line?: number;
  column?: number;
};
export type UpdateInfo = {
  hasUpdate: boolean;
  currentVersion: string;
  latestVersion: string | null;
  releaseUrl: string;
  asset: { name: string; url: string } | null;
};
export const errorOf = (e: unknown): AppError =>
  typeof e === "object" && e !== null && "message" in e
    ? (e as AppError)
    : {
        code: "UNKNOWN",
        message: typeof e === "string" ? e : "操作失败，请重新尝试",
      };

export type GatewaySettings = {
  port: number;
  maxRetries: number;
  failureThreshold: number;
  successThreshold: number;
  cooldownSeconds: number;
  rateLimitSeconds: number;
  capacityRetrySeconds: number;
  websocketRetrySeconds: number;
  handoffAfterCompaction?: boolean;
  errorRate: number;
  minRequests: number;
  firstByteSeconds: number;
  idleSeconds: number;
  totalSeconds: number;
  connectSeconds: number;
  queueSeconds: number;
  maxWaiting: number;
};
export type Health = {
  state: "closed" | "open" | "half_open";
  failures: number;
  requests: number;
  retryIn: number;
  cooldownReason?: string | null;
  protectedSingleProvider?: boolean;
  probeInFlight?: boolean;
  available?: boolean;
};
export type Provider = {
  id: string;
  name: string;
  baseUrl: string;
  queued: boolean;
  health: Health;
  quotaVersion: string;
  quota: ProviderQuota | null;
  maxConcurrency: number;
  maxRpm: number;
  activeRequests: number;
  rpmUsed: number;
  rpmRetryIn: number;
  rpmLimited: boolean;
  rpmLedgerError?: boolean;
  allowedModels: string[] | null;
  supportsWebsocket: boolean;
};
export type GatewayState = {
  clientId: ClientId;
  revision: string;
  running: boolean;
  address: string;
  mode: "manual" | "auto";
  selected: string | null;
  lastSuccessful: string | null;
  configRevision: string | null;
  configProvider: string | null;
  configState: string;
  configError: string | null;
  configWarning?: string | null;
  providers: Provider[];
  settings: GatewaySettings;
  activeConnections: number;
  waitingRequests: number;
  capacityRetries: { providerId: string; retryIn: number }[];
  compactionPending?: string[];
  websocketRetries?: { providerId: string; retryIn: number }[];
  error: string | null;
  recoveryPending: boolean;
};
export type StartupState = {
  launchOnBoot: boolean;
  restoreGateway: boolean;
  revision: string;
};

export type QuotaPlan = {
  name: string;
  remaining: number | null;
  used: number | null;
  total: number | null;
  unit: string;
  unlimited: boolean;
  resetAt: string | null;
};
export type QuotaUsage = {
  requests: number | null;
  tokens: number | null;
  cost: number | null;
};
export type ProviderQuota = {
  providerId: string;
  version: string;
  state: "idle" | "loading" | "ok" | "unsupported" | "error";
  source: "sub2api" | "newapi" | null;
  checkedAt: number | null;
  successAt: number | null;
  retryAt: number | null;
  nextRefreshAt?: number | null;
  stale: boolean;
  error: string | null;
  keyStatus: string | null;
  plans: QuotaPlan[];
  expiresAt: string | null;
  expiresAtUnix: number | null;
  today: QuotaUsage | null;
  totalUsage: QuotaUsage | null;
};

export type ModelCatalog = {
  providerId: string;
  version: string;
  models: string[];
  checkedAt: number | null;
  stale: boolean;
  error: string | null;
  retryAt: number | null;
};
