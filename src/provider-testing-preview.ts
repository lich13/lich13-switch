import type {
  ProviderDetection,
  ProviderTestDetail,
  ProviderTestModels,
  ProviderTestRequest,
  ProviderTestingState,
} from "./bridge-contracts";
import { gatewayPreview } from "./gateway-preview";
import type { GatewayState } from "./types";
const states = new Map<string, ProviderTestingState>();
const catalogs = new Map<string, ProviderTestModels>();
const details = new Map<string, ProviderTestDetail>();
const bank: ProviderTestingState["bank"] = {
  source: "builtin",
  version: { revision: "123456789abcdef" },
  models: [],
  checkedAt: null,
  updating: false,
  error: null,
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
          model: "gpt-6-luna",
          endpoint: null,
        },
        bank,
      };
      states.set(key, state);
    }
    return state;
  };
  if (name === "provider_test_state") return structuredClone(stateFor(key));
  if (name === "provider_test_models") {
    const cacheKey = `${key}:${args.endpoint}`;
    if (args.refresh)
      catalogs.set(cacheKey, {
        models: [
          { id: "fixture-model", name: "示例模型" },
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
      state.connectionPrompt = prompt;
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
    };
    state.jobs.unshift(job);
    details.set(job.id, {
      job,
      output: {
        text: "预览测试结果",
        groups: {},
        diagnostics: {},
        error: null,
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
    bank.checkedAt = Date.now();
    return structuredClone(bank);
  }
  return null;
}
