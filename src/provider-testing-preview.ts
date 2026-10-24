import type {
  ProviderDetection,
  ProviderTestDetail,
  ProviderTestRequest,
  ProviderTestingState,
} from "./bridge-contracts";
import { gatewayPreview } from "./gateway-preview";
import type { GatewayState } from "./types";
const state: ProviderTestingState = {
  revision: "preview-tests-1",
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
    source: "bundled",
    version: { revision: "preview" },
    models: [],
    checkedAt: null,
    updating: false,
    error: null,
  },
};
const details = new Map<string, ProviderTestDetail>();
export function providerTestingPreview(
  name: string,
  args: Record<string, unknown>,
) {
  const gateway = (clientId: string) =>
    gatewayPreview("get_gateway", { clientId }) as GatewayState;
  const start = (
    request: ProviderTestRequest,
    batchId: string | null = null,
  ) => {
    const provider = gateway(request.clientId).providers.find(
      (p) => p.id === request.providerId,
    );
    const job: ProviderTestDetail["job"] = {
      id: crypto.randomUUID(),
      clientId: request.clientId,
      providerId: request.providerId,
      providerName: provider?.name ?? "供应商",
      providerVersion: "preview",
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
      batchId,
      canRetry: false,
      completionReason: null,
    };
    state.jobs.unshift(job);
    details.set(job.id, {
      job,
      output: { text: "预览测试结果", groups: {}, diagnostics: {} },
    });
    return job;
  };
  if (name === "provider_test_state") return structuredClone(state);
  if (name === "provider_test_models")
    return {
      models: [{ id: "fixture-model", name: "示例模型" }],
      defaultModel: "fixture-model",
      stale: false,
      error: null,
    };
  if (name === "provider_test_start")
    return start(args.request as ProviderTestRequest);
  if (name === "provider_test_query")
    return structuredClone(details.get(String(args.id)));
  if (name === "provider_test_batch") {
    const id = crypto.randomUUID();
    return {
      id,
      jobs: (args.requests as ProviderTestRequest[]).map((r) => start(r, id)),
      errors: [],
    };
  }
  if (name === "provider_test_set_detection") {
    state.detection = structuredClone(args.detection as ProviderDetection);
    state.revision = crypto.randomUUID();
  }
  if (name === "provider_test_clear_mark") {
    delete state.degraded[String(args.providerId)];
    state.revision = crypto.randomUUID();
  }
  if (name === "provider_test_update_bank") {
    state.bank.checkedAt = Date.now();
    return structuredClone(state.bank);
  }
  return null;
}
