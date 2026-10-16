export type Tokens = {
  inclusiveInput?: number | null;
  input: number | null;
  output: number | null;
  cacheRead: number | null;
  cacheWrite: number | null;
  cacheWrite5m?: number | null;
  cacheWrite1h?: number | null;
  imageInput?: number | null;
  imageOutput?: number | null;
  audioInput?: number | null;
  audioOutput?: number | null;
};
export type PriceSnapshot = {
  version: string;
  source: string;
  model: string;
  multiplier: string;
  rates: Record<string, string>;
  cost: string;
  basis?: {
    operation?: string;
    unit?: string;
    quantity?: number;
    cost_per_request?: string;
  };
};
export type Attempt = {
  transfer?: {
    receivedAt?: number | null;
    sentAt: number;
    firstByteAt: number | null;
    terminalAt: number | null;
    terminalEvent?: string | null;
    terminalStatus?: string | null;
    incompleteReason?: string | null;
    upstreamClientRequestHash?: string | null;
    endedAt: number;
    upstreamBytes: number;
    downstreamBytes: number;
    connection?: {
      id: string;
      requestSequence: number;
      readBytes: number;
      socketWrittenBytes: number;
      events: { sequence: number; at: number; kind: string }[];
    } | null;
    terminalHandedOff: boolean;
    upstreamEof: boolean;
    upstreamEnd?: string | null;
    wsCloseCode?: number | null;
    downstreamError: string | null;
    timeoutPhase: string | null;
    timeoutSeconds: number | null;
    lastProgressMs: number | null;
    clientRequestHash: string | null;
    responseHash: string | null;
  } | null;
  end?: {
    cause: string;
    terminalReceived: boolean;
    responseStarted: boolean;
  } | null;
  threadHash?: string | null;
  upstreamRequestHash?: string | null;
  routingDecision?: string | null;
  routingEvidence?: {
    input: string;
    hasToolDeclarations: boolean;
    restriction?: string | null;
  } | null;
  pricingBasis?: string | null;
  mappingRevision?: string | null;
  repeatCount?: number;
  compactedUnpriced?: number | null;
  operation?: "model" | "web_search" | "compaction";
  compactionKind?: string | null;
  usageStatus?: string;
  availability?: Record<string, string>;
  usageSources?: Record<string, string>;
  id: string;
  provider: string | null;
  requestedModel: string | null;
  responseModel: string | null;
  pricingModel: string | null;
  responseId: string | null;
  status: number | null;
  outcome: string;
  startedAt: number;
  durationMs: number;
  firstTokenMs: number | null;
  stream: boolean;
  transport: string;
  tokens: Tokens;
  serviceTier: string | null;
  price: PriceSnapshot | null;
};
export type FailoverEvidence = {
  stage: "initial" | "proactive_handoff" | "failure_fallback" | "cursor_pinned" | "unknown_owner";
  failure: "rate_limit" | "account_rejected" | "model_unavailable" | "upstream_service" | "transport" | "protocol" | null;
  eligibleCandidates: number;
  exclusions: Record<string, number>;
  wait: { kind: string; seconds: number } | null;
  exit: "forwarded" | "no_eligible_provider" | "attempts_exhausted" | "permanent_rejection" | "cancelled" | "stopped" | "admission_rejected" | "replay_unsafe" | string | null;
};
export type UsageRecord = {
  failoverEvidence?: FailoverEvidence | null;
  finishedAt?: number | null;
  id: string;
  client: string;
  source: string;
  startedAt: number;
  sessionId: string | null;
  attempts: Attempt[];
  completed: boolean;
  estimatedSpeed: boolean;
  duplicateOf: string | null;
  deduplication: string;
  mergedSources?: string[];
};
export type Totals = {
  requests: number;
  attempts?: number;
  success: number;
  statusKnown: number;
  sessions: number;
  tokens: Tokens;
  cost: string;
  unpriced: number;
  durationMs: number;
  measuredOutputs: number;
  generationMs: number;
  firstTokenSumMs?: number;
  firstTokenSamples?: number;
  cacheReadEligible?: number;
  cacheInputEligible?: number;
};
export type Group = { id: string; totals: Totals };
export type UsageFilter = {
  source?: "proxy" | "sessions" | "grok_sessions";
  operation?: string;
  start?: number;
  end?: number;
  client?: string;
  provider?: string;
  model?: string;
  status?: string;
  page?: number;
};
export type Dashboard = {
  dataVersion?: number;
  reviewCount?: number;
  sourceHistoryIncomplete?: boolean;
  totals: Totals;
  providers: Group[];
  models: Group[];
  precision: string;
  detailSince: number;
  sources: Record<string, number>;
};
export type UsagePage = {
  dataVersion?: number;
  rows: UsageRecord[];
  total: number;
  page: number;
  detailSince: number;
};
export type UsageSettings = {
  recording: boolean;
  autoSync: boolean;
  refreshSeconds: number;
  multiplier: string;
  pricingModel: "response" | "request";
};
export type UsageState = {
  settings: UsageSettings;
  syncing: boolean;
  reports: Record<
    string,
    {
      files: number;
      totalFiles?: number;
      merged?: number;
      pending?: number;
      historicalBefore?: number;
      phase?: string;
      imported: number;
      skipped: number;
      errors: number;
      completedAt: number | null;
    }
  >;
  error: string | null;
};
export type ProviderPriceMapping = {
  client: "codex" | "claude" | "grok";
  provider: string;
  enabled: boolean;
  matchOn: "request" | "response";
  fromModel: string;
  toModel: string;
};
export type PricingConfig = {
  providerMappings?: ProviderPriceMapping[];
  autoUpdate: boolean;
  selected: string[] | null;
  excluded: string[];
  fixed: Record<string, Record<string, unknown>>;
  aliases: Record<string, string>;
};
export type PricingView = {
  config: PricingConfig;
  models: Record<string, Record<string, unknown>>;
  version: string;
  source: string;
  checkedAt: number | null;
  updatedAt: number | null;
  error: string | null;
  syncing: boolean;
  revision: string;
};
export const numeric = (n: number | null | undefined) =>
  n == null ? "—" : n.toLocaleString("zh-CN");
export const compact = (n: number | null | undefined) => {
  if (n == null || !Number.isFinite(n)) return "—";
  if (Math.abs(n) < 1000) return numeric(n);
  const units = ["", "K", "M", "B"];
  let power = Math.min(3, Math.floor(Math.log10(Math.abs(n)) / 3));
  let value = Number((n / 1000 ** power).toPrecision(3));
  if (Math.abs(value) >= 1000 && power < 3) {
    power++;
    value = Number((value / 1000).toPrecision(3));
  }
  return `${value}${units[power]}`;
};
export const money = (n: string | null | undefined) =>
  n == null ? "未定价" : "$" + Number(n).toFixed(4);
export const tokenTotal = (t: Tokens) =>
  [t.input, t.output, t.cacheRead, t.cacheWrite].every((n) => n == null)
    ? null
    : [t.input, t.output, t.cacheRead, t.cacheWrite].reduce<number>(
        (a, b) => a + (b ?? 0),
        0,
      );
export const clientName = (c: string) =>
  c === "codex" ? "Codex" : c === "claude" ? "Claude Code" : c === "grok" ? "Grok Build" : c;
export function speed(a: Attempt, estimated: boolean) {
  const ms = estimated
    ? a.durationMs
    : a.firstTokenMs == null
      ? 0
      : a.durationMs - a.firstTokenMs;
  return a.tokens.output != null && a.tokens.output > 0 && ms > 0
    ? `${((a.tokens.output / ms) * 1000).toFixed(1)}${estimated ? "（估算）" : ""}`
    : "—";
}

export function averageFirstToken(
  t: Pick<Totals, "firstTokenSumMs" | "firstTokenSamples"> | null | undefined,
): number | null {
  return t?.firstTokenSamples
    ? (t.firstTokenSumMs ?? 0) / t.firstTokenSamples
    : null;
}
export function firstTokenLabel(ms: number | null | undefined): string {
  return ms == null ? "—" : `${(ms / 1000).toFixed(2)}s`;
}
export function firstTokenTitle(t: Totals | null | undefined): string {
  const ms = averageFirstToken(t);
  return ms == null
    ? "无首字样本"
    : `${(ms / 1000).toFixed(3)}s · ${numeric(t?.firstTokenSamples)} 个有效样本`;
}
