import type {
  Attempt,
  Dashboard,
  PricingView,
  Tokens,
  Totals,
  UsageFilter,
  UsageRecord,
  UsageSettings,
  UsageState,
} from "./usage-types";
import { densePreview } from "./preview-fixtures";
const tokens: Tokens = {
  input: 1400,
  output: 860,
  cacheRead: 3200,
  cacheWrite: 0,
};
const rows: UsageRecord[] = Array.from({ length: 47 }, (_, i) => {
  const at = Date.now() - i * 1800000;
  const attempt: Attempt = {
    id: `fixture-attempt-${i}`,
    provider: ["primary", "claude-primary", "backup", "claude-backup"][i % 4],
    requestedModel: i % 2 ? "claude-example" : "gpt-example",
    responseModel: i % 2 ? "claude-example" : "gpt-example-2026",
    pricingModel: i % 2 ? "claude-example" : "gpt-example-2026",
    responseId: null,
    status: i % 5 ? 200 : 429,
    outcome: i % 5 ? "success" : "failure",
    startedAt: at,
    durationMs: 5200,
    firstTokenMs: 340,
    stream: true,
    transport: "http",
    operation: i % 8 === 2 ? "compaction" : "model",
    compactionKind: i % 8 === 2 ? "request" : null,
    usageStatus: "reported",
    tokens:
      i % 5 ? { ...tokens } : { ...tokens, input: 0, output: 0, cacheRead: 0 },
    serviceTier: null,
    price: {
      version: "fixture-price-version",
      source: "sub2api",
      model: "gpt-example",
      multiplier: "1",
      rates: {
        input_cost_per_token: "0.000003",
        output_cost_per_token: "0.000012",
      },
      cost: i % 5 ? "0.01512" : "0",
    },
  };
  return {
    id: `fixture-request-${i}`,
    client: i % 2 ? "claude" : "codex",
    source: i % 3 ? "proxy" : i % 2 ? "claude" : "codex",
    startedAt: at,
    sessionId: null,
    finishedAt: at + 5200,
    attempts: [{ ...attempt, status: i % 3 ? attempt.status : null }],
    completed: i % 5 !== 0,
    estimatedSpeed: i % 3 === 0,
    duplicateOf: null,
    deduplication: "独立记录",
  };
});
if (densePreview) {
  const large = rows[2].attempts[0];
  large.provider = "primary";
  large.tokens = {
    input: 1234,
    output: 1234567,
    cacheRead: 1234567890,
    cacheWrite: 0,
  };
  large.price = {
    ...large.price!,
    cost: "385.188873",
    rates: {
      input_cost_per_token: "0.000003",
      output_cost_per_token: "0.000012",
      cache_read_input_token_cost: "0.0000003",
    },
  };
}
const search = rows[1];
search.client = "codex";
search.source = "proxy";
search.attempts[0] = {
  ...search.attempts[0],
  provider: "primary",
  operation: "web_search",
  requestedModel: null,
  responseModel: null,
  pricingModel: "web_search",
  tokens: { input: null, output: null, cacheRead: null, cacheWrite: null },
  price: {
    version: "web-search-v1",
    source: "request",
    model: "web_search",
    multiplier: "1",
    rates: { cost_per_request: "0.01" },
    cost: "0.01",
    basis: {
      operation: "web_search",
      unit: "request",
      quantity: 1,
      cost_per_request: "0.01",
    },
  },
};
if (densePreview) {
  const completed = rows[2].attempts[0];
  completed.transport = "websocket";
  completed.end = {
    cause: "client_disconnected",
    terminalReceived: true,
    responseStarted: true,
  };
  completed.routingDecision = "compaction_handoff";
  completed.transfer = {
    receivedAt: completed.startedAt,
    sentAt: completed.startedAt + 100,
    firstByteAt: completed.startedAt + 300,
    terminalAt: completed.startedAt + 5100,
    endedAt: completed.startedAt + 5200,
    upstreamBytes: 1048576,
    downstreamBytes: 1048576,
    terminalHandedOff: true,
    upstreamEof: true,
    upstreamEnd: "EOF",
    downstreamError: null,
    timeoutPhase: null,
    timeoutSeconds: null,
    lastProgressMs: 0,
    clientRequestHash: null,
    responseHash: null,
  };
  const interrupted = rows[4];
  interrupted.completed = false;
  interrupted.attempts[0].outcome = "unknown";
  interrupted.attempts[0].end = {
    cause: "client_disconnected",
    terminalReceived: false,
    responseStarted: true,
  };
  interrupted.attempts[0].routingDecision = "existing_thread";
  interrupted.attempts[0].transfer = {
    receivedAt: interrupted.startedAt,
    sentAt: interrupted.startedAt + 100,
    firstByteAt: interrupted.startedAt + 300,
    terminalAt: null,
    endedAt: interrupted.startedAt + 5200,
    upstreamBytes: 16384,
    downstreamBytes: 8192,
    terminalHandedOff: false,
    upstreamEof: false,
    upstreamEnd: null,
    downstreamError: "DOWNSTREAM_CONNECTION_ERROR",
    connection: {
      id: "fixture-connection-id",
      requestSequence: 2,
      readBytes: 4096,
      socketWrittenBytes: 8448,
      events: [
        { sequence: 1, at: interrupted.startedAt + 5100, kind: "peer_reset" },
        { sequence: 2, at: interrupted.startedAt + 5200, kind: "response_body_cancelled" },
      ],
    },
    timeoutPhase: null,
    timeoutSeconds: null,
    lastProgressMs: 10,
    clientRequestHash: null,
    responseHash: null,
  };
  const stopped = rows[5];
  stopped.completed = false;
  stopped.attempts[0].status = 200;
  stopped.attempts[0].outcome = "cancelled";
  stopped.attempts[0].end = {
    cause: "gateway_stopped",
    terminalReceived: false,
    responseStarted: true,
  };
  const retried = rows[8];
  retried.attempts.unshift({
    ...structuredClone(retried.attempts[0]),
    id: "fixture-prior-attempt",
    provider: "primary",
    status: 503,
    outcome: "failure",
    startedAt: retried.startedAt - 1000,
    durationMs: 500,
    firstTokenMs: null,
    tokens: { input: null, output: null, cacheRead: null, cacheWrite: null },
    price: null,
    end: {
      cause: "upstream_rejected",
      terminalReceived: false,
      responseStarted: false,
    },
  });
}
for (const row of rows) {
  row.finishedAt =
    row.attempts.at(-1)?.transfer?.terminalAt ??
    row.attempts.at(-1)?.transfer?.endedAt ??
    row.finishedAt;
}
let settings: UsageSettings = {
  recording: true,
  autoSync: true,
  refreshSeconds: 30,
  multiplier: "1",
  pricingModel: "response",
};
let price: PricingView = {
  config: {
    autoUpdate: true,
    selected: null,
    excluded: [],
    fixed: {},
    aliases: {},
  },
  models: {
    "gpt-example": {
      input_cost_per_token: 0.000003,
      output_cost_per_token: 0.000012,
      cache_read_input_token_cost: 0.0000003,
    },
    "claude-example": {
      input_cost_per_token: 0.000003,
      output_cost_per_token: 0.000015,
      cache_read_input_token_cost: 0.0000003,
      cache_creation_input_token_cost: 0.00000375,
    },
  },
  version: "fixture-price-version",
  source: "Sub2API · Wei-Shaw/model-price-repo",
  checkedAt: Date.now(),
  updatedAt: Date.now() - 86400000,
  error: null,
  syncing: false,
  revision: "fixture-pricing",
};
function total(items: UsageRecord[]): Totals {
  const first = items
    .filter((r) => r.source === "proxy")
    .map((r) => r.attempts.at(-1)!)
    .filter(
      (a) =>
        a.stream &&
        (a.operation ?? "model") === "model" &&
        a.firstTokenMs != null,
    )
    .map((a) => a.firstTokenMs!);

  const sum = (key: keyof Tokens) => {
    const values = items
      .flatMap((row) => row.attempts.map((attempt) => attempt.tokens[key]))
      .filter((value): value is number => value != null);
    return values.length ? values.reduce((n, value) => n + value, 0) : null;
  };
  return {
    firstTokenSumMs: first.reduce((sum, v) => sum + v, 0),
    firstTokenSamples: first.length,
    cacheReadEligible: items
      .flatMap((r) => r.attempts)
      .filter(
        (a) =>
          a.tokens.input != null &&
          a.tokens.cacheRead != null &&
          a.tokens.cacheWrite != null,
      )
      .reduce((n, a) => n + a.tokens.cacheRead!, 0),
    cacheInputEligible: items
      .flatMap((r) => r.attempts)
      .filter(
        (a) =>
          a.tokens.input != null &&
          a.tokens.cacheRead != null &&
          a.tokens.cacheWrite != null,
      )
      .reduce(
        (n, a) =>
          n + a.tokens.input! + a.tokens.cacheRead! + a.tokens.cacheWrite!,
        0,
      ),
    requests: items.length,
    success: items.filter((r) => r.attempts.at(-1)!.status === 200).length,
    statusKnown: items.filter((r) => r.attempts.at(-1)!.status != null).length,
    sessions: items.filter((r) => r.source !== "proxy").length,
    tokens: {
      input: sum("input"),
      output: sum("output"),
      cacheRead: sum("cacheRead"),
      cacheWrite: sum("cacheWrite"),
    },
    cost: String(
      items.reduce(
        (n, r) => n + Number(r.attempts.at(-1)?.price?.cost ?? 0),
        0,
      ),
    ),
    unpriced: 0,
    durationMs: items.length * 5200,
    measuredOutputs: sum("output") ?? 0,
    generationMs: items.length * 4860,
  };
}
export const usageCommands = [
  "get_usage_state",
  "get_usage_dashboard",
  "get_usage_logs",
  "get_usage_detail",
  "set_usage_settings",
  "sync_usage",
  "get_pricing",
  "configure_pricing",
  "update_pricing",
  "reload_pricing",
  "open_pricing_directory",
];
export function usagePreview(
  name: string,
  args: Record<string, unknown>,
  emit: (event: string, value: unknown) => void,
): unknown {
  const state = (): UsageState => ({
    settings: { ...settings },
    syncing: false,
    reports: {
      codex: {
        files: 8,
        imported: 16,
        skipped: 0,
        errors: 0,
        completedAt: Date.now(),
      },
      claude: {
        files: 4,
        imported: 9,
        skipped: 0,
        errors: 0,
        completedAt: Date.now(),
      },
    },
    error: null,
  });
  if (name === "set_usage_settings") {
    settings = { ...(args.settings as UsageSettings) };
    emit("usage-state", {});
    return state();
  }
  if (name === "get_usage_state" || name === "sync_usage") return state();
  if (name === "get_usage_detail") return rows.find((r) => r.id === args.id);
  if (name === "configure_pricing") {
    price = {
      ...price,
      config: args.config as PricingView["config"],
      revision: crypto.randomUUID(),
    };
    price.models = { ...price.models, ...price.config.fixed };
    emit("pricing-state", {});
    return structuredClone(price);
  }
  if (name.includes("pricing"))
    return name === "open_pricing_directory"
      ? undefined
      : structuredClone(price);
  const f = (args.filter ?? {}) as UsageFilter;
  const selected = rows
    .filter(
      (r) =>
        (!f.source ||
          (f.source === "proxy"
            ? r.source === "proxy"
            : r.source !== "proxy")) &&
        (r.finishedAt ?? r.startedAt) >= (f.start ?? 0) &&
        (r.finishedAt ?? r.startedAt) <= (f.end ?? Infinity) &&
        (!f.client || r.client === f.client) &&
        (!f.provider || r.attempts.at(-1)!.provider === f.provider) &&
        (!f.model || r.attempts.at(-1)!.pricingModel === f.model) &&
        (!f.operation ||
          (r.attempts.at(-1)!.operation ?? "model") === f.operation),
    )
    .sort(
      (a, b) => (b.finishedAt ?? b.startedAt) - (a.finishedAt ?? a.startedAt),
    );
  if (name === "get_usage_logs") {
    const filtered = selected.filter(
      (r) =>
        !f.status ||
        (f.status === "none"
          ? r.attempts.at(-1)!.status == null
          : String(r.attempts.at(-1)!.status).startsWith(f.status[0])),
    );
    const page = f.page ?? 1;
    return {
      rows: structuredClone(filtered.slice((page - 1) * 20, page * 20)),
      total: filtered.length,
      page,
      detailSince: 0,
    };
  }
  if (name === "get_usage_dashboard") {
    const group = (key: (r: UsageRecord) => string) =>
      [...new Set(selected.map(key))].map((id) => ({
        id,
        totals: total(selected.filter((r) => key(r) === id)),
      }));
    const d: Dashboard = {
      totals: total(selected),
      providers: group((r) => r.attempts.at(-1)!.provider ?? "session"),
      models: group((r) => r.attempts.at(-1)!.pricingModel ?? "unknown"),
      precision: "millisecond",
      detailSince: 0,
      sources: Object.fromEntries(
        group((r) => r.source).map((g) => [g.id, g.totals.requests]),
      ),
    };
    return d;
  }
}
