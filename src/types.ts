import { ErrorCodes } from "./error-codes";
export type ClientId = "codex" | "claude" | "grok" | "dsh";
export type Preferences = {
  codexHome: string;
  claudeHome?: string;
  grokHome?: string;
  dshHome?: string;
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
  subscription?: {
    planType: string | null;
    label: string;
    stale: boolean;
  } | null;
};
export type ViewState = {
  accounts: Account[];
  authRevision: string;
  configRevision: string;
  currentState: "saved" | "unsaved" | "missing" | "invalid" | "gateway";
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
  code: import("./error-codes").CompatibleErrorCode;
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
        code: ErrorCodes.Unknown,
        message: typeof e === "string" ? e : "操作失败，请重新尝试",
      };

export type GatewaySettings = {
  port: number;
  maxRetries: number;
  failureThreshold: number;
  transientFailureThreshold?: number;
  successThreshold: number;
  cooldownSeconds: number;
  rateLimitSeconds: number;
  capacityRetrySeconds: number;
  websocketRetrySeconds: number;
  threadIdleReleaseSeconds?: number;
  endpointProbeSeconds?: number;
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
  cause?: {
    eventId: string;
    code: string;
    upstreamReason?: string | null;
    errorBasis?: string | null;
    reason: string;
    status: number | null;
    wsCloseCode: number | null;
  } | null;
};
export type GrokProtocol = "responses" | "chat_completions" | "messages";
export type Provider = {
  protocol?: GrokProtocol | null;
  id: string;
  name: string;
  baseUrl: string;
  extraBaseUrls?: string[];
  preferredBaseUrl?: string;
  endpointMeasurements?: {
    baseUrl: string;
    latencyMs: number | null;
    status: number | null;
    error: string | null;
    checkedAt: number;
  }[];
  queued: boolean;
  health: Health;
  modelHealth?: { model: string; health: Health }[];
  quotaVersion: string;
  quota: ProviderQuota | null;
  quotaChecking?: boolean;
  quotaCheck?: import("./limit-labels").QuotaCheck | null;
  maxConcurrency: number;
  maxRpm: number;
  activeRequests: number;
  reservedThreads?: number;
  rpmUsed: number;
  rpmRetryIn: number;
  rpmLimited: boolean;
  rpmWaitReason?: string | null;
  rpmLedgerError?: boolean;
  allowedModels: string[] | null;
  handoffAfterCompaction?: boolean;
  takeNewThreads?: boolean;
};
export type GatewayListener = {
  port: number;
  revision: string;
  runningClients: ClientId[];
};
export type GrokModel = {
  key: string;
  model: string;
  protocol: GrokProtocol | null;
  address: string | null;
  addressSource: string | null;
  credentialAvailable: boolean;
  missingEnvironment: string[];
  error: string | null;
};
export type GatewayState = {
  listener?: GatewayListener;
  grokModels?: GrokModel[];
  grokNativeAvailable?: boolean;
  dshProfile?: {
    profile: string;
    protocol: GrokProtocol;
    model: string | null;
    makeDefault: boolean;
  } | null;
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
  current?: {
    kind: "gateway" | "provider" | "official" | "unmatched" | "unknown";
    id?: string;
    by?: "urlToken" | "urlOnly";
    host?: string;
    reason?: string;
    model: string | null;
    modelSource: string | null;
    warning: string | null;
  };
  configError: string | null;
  configWarning?: string | null;
  connectionMode?: "bearer" | "apiKey";
  providers: Provider[];
  settings: GatewaySettings;
  activeConnections: number;
  waitingRequests: number;
  capacityRetries: {
    providerId: string;
    retryIn: number;
    existingOwner?: boolean;
  }[];
  handoffWaiting?: {
    providerId: string;
    state:
      | "awaiting_compaction"
      | "awaiting_request"
      | "capacity_full"
      | "claim_in_flight";
  }[];
  websocketRetries?: { providerId: string; retryIn: number }[];
  transientRetries?: { providerId: string; retryIn: number }[];
  error: string | null;
  recoveryPending: boolean;
};
export type StartupState = {
  launchOnBoot: boolean;
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
  subscriptions?: {
    name: string | null;
    status: string | null;
    expiresAt: string | null;
    active: boolean | null;
    plans: QuotaPlan[];
  }[];
  expiresAt: string | null;
  expiresAtUnix: number | null;
  today: QuotaUsage | null;
  totalUsage: QuotaUsage | null;
};

export type ModelCatalog = {
  metadata?: Record<string, ModelMetadata>;
  providerId: string;
  version: string;
  models: string[];
  checkedAt: number | null;
  stale: boolean;
  error: string | null;
  retryAt: number | null;
};
export type ModelMetadata = {
  sources: string[];
  model: string;
  matchedBy: string;
  name: string | null;
  reasoning: boolean | null;
  context: number | null;
  output: number | null;
  reasoningEfforts: Record<string, string | null> | null;
};
