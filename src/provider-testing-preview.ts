import type {
  DetectionPlatform,
  FingerprintBankState,
  ProviderDetection,
  ProviderTestDetail,
  ProviderTestModels,
  ProviderTestRequest,
  ProviderTestingState,
} from "./bridge-contracts";
import { gatewayPreview } from "./gateway-preview";
import type { GatewayState } from "./types";
let globalPrompt = "Reply with a short greeting.";
const platforms = new Map<string, DetectionPlatform>();
const states = new Map<string, ProviderTestingState>();
const catalogs = new Map<string, ProviderTestModels>();
const details = new Map<string, ProviderTestDetail>();
const bank: FingerprintBankState = {
  source: "builtin",
  version: { revision: "123456789abcdef" },
  models: [],
  checkedAt: null,
  updating: false,
  error: null,
};
const lmBank: FingerprintBankState = {
  ...bank,
  version: { revision: "567e11d65a48d4fbba2713cc4dfbe685cdd05a73" },
  updatedAt: null,
  result: null,
};
export function providerTestingPreview(
  name: string,
  args: Record<string, unknown>,
) {
  const clientId = String(args.clientId ?? "codex"),
    providerId = String(args.providerId);
  const key = `${clientId}:${providerId}`;
  const stateFor = (key: string) => {
    let state = states.get(key);
    if (!state) {
      state = {
        revision: crypto.randomUUID(),
        connectionPrompt: globalPrompt,
        platform: platforms.get(key.split(":")[0]) ?? {
          engine: key.startsWith("codex:") ? "modelTrace" : "lmDetector",
          modelTraceSamples: 3,
          lmDetectorSamples: 1,
        },
        banks: {
          modelTrace: bank,
          lmDetector: lmBank,
        },
        jobs: [],
        degraded: {},
        detectionStatus: [],
        error: null,
        detection: {
          enabled: false,
          mode: "smart",
          intervalMinutes: 15,
          minIntervalMinutes: 5,
          model: key.startsWith("codex:") ? "gpt-6-luna" : "",
          endpoint: null,
        },
      };
      states.set(key, state);
    }
    return state;
  };
  if (name === "provider_test_save_prompt") {
    if (!String(args.prompt).trim()) throw new Error("请输入测试文案");
    globalPrompt = String(args.prompt);
    for (const state of states.values()) state.connectionPrompt = globalPrompt;
  }
  if (name === "provider_test_set_platform") {
    const platform = structuredClone(args.platform as DetectionPlatform);
    platforms.set(clientId, platform);
    for (const [scope, state] of states)
      if (scope.startsWith(`${clientId}:`)) state.platform = platform;
  }
  if (name === "provider_test_state") return structuredClone(stateFor(key));
  if (name === "provider_test_models") {
    const cacheKey = `${key}:${args.endpoint}`;
    if (args.refresh)
      catalogs.set(cacheKey, {
        models: [
          { id: "fixture-model", name: "示例模型" },
          {
            id: "fixture-long-model-name-with-regional-deployment-and-version-20261025",
            name: "长型号示例",
          },
          { id: "gpt-6-luna", name: "gpt-6-luna" },
        ],
        defaultModel: "fixture-model",
        fetchedAt: Date.now(),
        error: null,
      });
    return structuredClone(
      catalogs.get(cacheKey) ?? {
        models: [],
        defaultModel: "",
        fetchedAt: null,
        error: null,
      },
    );
  }
  if (name === "provider_test_start") {
    const request = args.request as ProviderTestRequest;
    const state = stateFor(`${request.clientId}:${request.providerId}`);
    if (request.kind === "connection") {
      const prompt = request.prompt ?? state.connectionPrompt;
      if (!prompt.trim()) throw new Error("请输入测试文案");
    }
    const gateway = gatewayPreview("get_gateway", {
      clientId: request.clientId,
    }) as GatewayState;
    const provider = gateway.providers.find((p) => p.id === request.providerId);
    const job: ProviderTestDetail["job"] = {
      id: crypto.randomUUID(),
      clientId: request.clientId,
      providerId: request.providerId,
      providerName: provider?.name ?? "供应商",
      providerVersion: "preview",
      endpoint: request.endpoint ?? provider?.baseUrl ?? null,
      kind: request.kind,
      mode: "text",
      requestedModel: request.model || "fixture-model",
      forwardedModel: request.model || "fixture-model",
      returnedModel: request.model || "fixture-model",
      status: "completed",
      createdAt: Date.now(),
      startedAt: Date.now(),
      completedAt: Date.now(),
      elapsedMs: 120,
      firstByteMs: 80,
      error: null,
      report: null,
      groups: [],
      automatic: false,
      batchId: null,
      canRetry: false,
      completionReason: null,
      engine: state.platform.engine,
      sampleCount:
        state.platform.engine === "lmDetector"
          ? state.platform.lmDetectorSamples
          : state.platform.modelTraceSamples,
      resourceVersion: state.banks[state.platform.engine].version ?? {},
      tokenizer: request.tokenizer ?? false,
      tokenizerResult: null,
    };
    const fixtureError =
      new URLSearchParams(location.search).get("test-preview") === "error";
    if (request.kind === "fingerprint") {
      const count = job.sampleCount;
      const report = {
        prediction: "gpt-6-astra",
        prediction_name: "gpt-6-astra/6.1-sol",
        probability: count === 1 ? null : 0.92,
        used_outputs: count - (fixtureError ? 1 : 0),
        results: [
          {
            model: "gpt-6-astra",
            display_name: "gpt-6-astra/6.1-sol",
            probability: count === 1 ? null : 0.92,
            score: 4.2,
          },
          {
            model: "claude-opus-4.8",
            display_name: "claude-opus-4.8",
            probability: count === 1 ? null : 0.06,
            score: 1.7,
          },
          {
            model: "grok-4.7",
            display_name: "grok-4.7",
            probability: count === 1 ? null : 0.02,
            score: 0.4,
          },
        ],
        bank_version: job.resourceVersion,
        disposition: "unknown" as const,
        ambiguous: true,
        initial: count === 1 && job.engine === "lmDetector",
      };
      job.report = report.used_outputs ? report : null;
      job.groups = Array.from({ length: count }, (_, index) => ({
        index,
        expected: 300,
        status: fixtureError && index === 0 ? "failed" : "completed",
        attempts: 1,
        elapsedMs: 1200,
        returnedModel: job.returnedModel,
        report: fixtureError && index === 0 ? null : report,
        diagnostics: {},
        error:
          fixtureError && index === 0 ? "上游请求失败，请查看错误正文。" : null,
        retryable: false,
        uncertain: false,
      }));
      if (request.tokenizer)
        job.tokenizerResult = {
          status: "completed",
          steps: [
            { probe: null, state: "done", tokens: 18 },
            { probe: "fixture-probe", state: "done", tokens: 36 },
          ],
          verdict: {
            kind: "unknown",
            top: { id: "o200k" },
            confidence: 0.81,
            claim: { status: "uncertain", probability: 0.81 },
          },
        };
    }
    if (fixtureError) {
      job.status = "failed";
      job.error = "上游返回 HTTP 403";
    }
    state.jobs.unshift(job);
    details.set(job.id, {
      job,
      output: {
        text:
          request.kind === "connection"
            ? "预览测试结果。\n" +
              "这里是一段较长的回复，用于检查多行换行、滚动和底部操作区域。".repeat(
                24,
              )
            : "",
        groups: Object.fromEntries(
          job.groups.map((group) => [
            group.index,
            "12, 84, 51, 29, 72, 36, 95, 10, 62, 43",
          ]),
        ),
        diagnostics: {},
        error: fixtureError
          ? {
              httpStatus: 403,
              message: "上游拒绝请求",
              body: '{"error":{"message":"fixture access denied with a long error description for wrapping and scrolling","type":"permission_error"}}',
              truncated: false,
            }
          : null,
        groupErrors: {},
      },
    });
    return structuredClone(job);
  }
  if (name === "provider_test_query")
    return structuredClone(details.get(String(args.id)));
  if (name === "provider_test_set_detection") {
    const state = stateFor(key);
    state.detection = structuredClone(args.detection as ProviderDetection);
    state.revision = crypto.randomUUID();
  }
  if (name === "provider_test_clear_mark") {
    const state = stateFor(key);
    delete state.degraded[providerId];
    state.revision = crypto.randomUUID();
  }
  if (name === "provider_test_update_bank") {
    const selected = args.engine === "lmDetector" ? lmBank : bank;
    selected.checkedAt = Date.now();
    if (args.engine === "lmDetector") selected.result = "up-to-date";
    return structuredClone(selected);
  }
  return null;
}
