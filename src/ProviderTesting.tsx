import { flushTestPrompt, useTestPrompt } from "./provider-test-prompt";
import { clientName } from "./ClientSelection";
import { useCallback, useEffect, useId, useRef, useState } from "react";
import { Copy, RefreshCw } from "lucide-react";
import { command } from "./bridge";
import { confirmAction } from "./confirmation";
import { subscriptions } from "./subscriptions";
import Modal from "./Modal";
import { errorOf, type GatewayState, type Provider } from "./types";
import type {
  DetectionEngine,
  DetectionPlatform,
  ProviderTestMode,
  ProviderTestKind,
  ProviderTestingState,
  ProviderTestDetail,
  ProviderTestModels,
  ProviderTestJob,
  ProviderDetection,
  ProviderTestError,
  FingerprintReport,
} from "./bridge-contracts";
import "./provider-testing.css";

const engineNames = { modelTrace: "ModelTrace", lmDetector: "LM Detector" };
const bankResults: Record<string, string> = {
  updated: "已更新",
  "up-to-date": "已是最新",
  failed: "检查失败",
};
const percent = (value: number | null | undefined) =>
  value == null ? "—" : `${(value * 100).toFixed(2)}%`;
const basisLabel = (
  basis: FingerprintReport["disposition_basis"],
  groups: number[] = [],
) =>
  basis === "sample_family"
    ? `第 ${groups.join("、")} 组出现较低家族`
    : basis === "aggregate"
      ? "汇总依据"
      : "";
const modes: Record<ProviderTestMode, string> = {
  text: "普通对话",
  compact: "Compact",
  image: "图像",
  video: "视频",
  search: "搜索",
  tts: "TTS",
  stt: "STT",
  realtime: "Realtime",
};
const statuses: Record<string, string> = {
  pending: "待执行",
  queued: "排队中",
  running: "运行中",
  completed: "已完成",
  failed: "失败",
  cancelled: "已停止",
  skipped: "提前结束",
  interrupted: "结果未知",
  disabled: "已关闭",
  degraded: "已标记降智",
  outOfQueue: "已退出队列",
  idle: "最近一小时无业务调用，已暂停",
  waiting: "等待下次检测",
  statsUnavailable: "等待调用统计",
  modelsUnavailable: "等待模型目录",
};
type Tab = ProviderTestKind | "history" | "detection" | "bank";
const active = (job: ProviderTestJob) =>
  ["queued", "running"].includes(job.status);
const date = (at: number | null | undefined) =>
  at == null
    ? "—"
    : new Date(at).toLocaleString("zh-CN", {
        timeZone: "Asia/Taipei",
        hour12: false,
      });
const duration = (job: ProviderTestJob, now: number) =>
  `${((active(job) && job.startedAt ? Math.max(0, now - job.startedAt) : job.elapsedMs) / 1000).toFixed(2)} s`;

export default function ProviderTesting({
  gateway,
  provider,
  close,
  onDirtyChange,
}: {
  gateway: GatewayState;
  provider: Provider;
  close: () => void;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const tabsId = useId();
  const scope = { clientId: gateway.clientId, providerId: provider.id };
  const [state, setState] = useState<ProviderTestingState | null>(null);
  const [tab, setTab] = useState<Tab>("connection");
  const [endpoint, setEndpoint] = useState(provider.baseUrl);
  const [chosen, setChosen] = useState({ connection: "", fingerprint: "" });
  const [concurrency, setConcurrency] = useState(1);
  const promptState = useTestPrompt(state?.connectionPrompt);
  const { prompt } = promptState;
  const [tokenizer, setTokenizer] = useState(false);
  const [bankEngine, setBankEngine] = useState<DetectionEngine>(
    gateway.clientId === "codex" ? "modelTrace" : "lmDetector",
  );
  const [catalog, setCatalog] = useState<{
    endpoint: string;
    value: ProviderTestModels;
  } | null>(null);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogError, setCatalogError] = useState("");
  const catalogRequest = useRef(0);
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [detailId, setDetailId] = useState<string | null>(null);
  const [detail, setDetail] = useState<ProviderTestDetail | null>(null);
  const [detection, setDetection] = useState<ProviderDetection | null>(null);
  const [detectionSaved, setDetectionSaved] = useState("");
  const [detectionRevision, setDetectionRevision] = useState("");
  const [now, setNow] = useState(Date.now());
  const kind: ProviderTestKind =
    tab === "fingerprint" ? "fingerprint" : "connection";
  const testing = tab === "connection" || tab === "fingerprint";
  const fingerprint = !provider.searchOnly;
  const platform = state?.platform;
  const sampleCount =
    platform?.engine === "lmDetector"
      ? platform.lmDetectorSamples
      : (platform?.modelTraceSamples ?? 3);
  const selectedBank = state?.banks[bankEngine];
  const model = chosen[kind];
  const dirty = !!detection && JSON.stringify(detection) !== detectionSaved;
  useEffect(() => {
    onDirtyChange?.(dirty);
  }, [dirty, onDirtyChange]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const catalogEndpoint =
    tab === "detection" ? (detection?.endpoint ?? provider.baseUrl) : endpoint;
  const models = catalog?.endpoint === catalogEndpoint ? catalog.value : null;
  const endpoints = [
    ...new Set([provider.baseUrl, ...(provider.extraBaseUrls ?? [])]),
  ];
  const endpointAvailable = endpoints.includes(endpoint);
  const modelAvailable = !!models?.models.some((m) => m.id === model);
  const detectionModelAvailable = !!models?.models.some(
    (m) => m.id === detection?.model,
  );
  const fail = useCallback((e: unknown) => setError(errorOf(e).message), []);
  const stateSequence = useRef(0);
  const mounted = useRef(false);
  const refresh = useCallback(
    async (resetDetection = false) => {
      const sequence = ++stateSequence.current;
      try {
        const current = await command("provider_test_state", {
          clientId: gateway.clientId,
          providerId: provider.id,
        });
        if (!mounted.current || sequence !== stateSequence.current) return;
        setState(current);
        if (resetDetection) {
          setDetection(structuredClone(current.detection));
          setDetectionSaved(JSON.stringify(current.detection));
          setDetectionRevision(current.revision);
        }
        return current;
      } catch (error) {
        if (mounted.current && sequence === stateSequence.current) throw error;
      }
    },
    [gateway.clientId, provider.id],
  );
  useEffect(() => {
    mounted.current = true;
    void refresh().catch(fail);
    const events = subscriptions(fail);
    events.watch("provider-tests", async () => {
      await refresh();
    });
    return () => {
      mounted.current = false;
      stateSequence.current++;
      events.dispose();
    };
  }, [refresh, fail]);
  useEffect(() => {
    if (!state || detection) return;
    setDetection(structuredClone(state.detection));
    setDetectionSaved(JSON.stringify(state.detection));
    setDetectionRevision(state.revision);
  }, [state, detection]);
  useEffect(() => {
    if (!state || !testing) return;
    const jobs = state.jobs.filter(
      (job) =>
        job.clientId === gateway.clientId &&
        job.providerId === provider.id &&
        job.kind === kind,
    );
    setDetailId((current) =>
      jobs.some((job) => job.id === current)
        ? current
        : (jobs.find(active)?.id ?? jobs[0]?.id ?? null),
    );
  }, [state, testing, kind, gateway.clientId, provider.id]);
  useEffect(() => {
    if (!detailId) {
      setDetail(null);
      return;
    }
    let disposed = false;
    void command("provider_test_query", { id: detailId })
      .then((value) => {
        if (
          !disposed &&
          value.job.clientId === gateway.clientId &&
          value.job.providerId === provider.id
        )
          setDetail(value);
      })
      .catch((e) => {
        if (!disposed) fail(e);
      });
    return () => {
      disposed = true;
    };
  }, [detailId, state, gateway.clientId, provider.id, fail]);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const loadCatalog = useCallback(
    async (reload: boolean) => {
      const request = ++catalogRequest.current;
      setCatalogLoading(true);
      setCatalogError("");
      try {
        const current = reload
          ? await command("get_gateway", { clientId: gateway.clientId })
          : gateway;
        const value = await command("provider_test_models", {
          clientId: gateway.clientId,
          providerId: provider.id,
          expectedRevision: current.revision,
          endpoint: catalogEndpoint,
          refresh: reload,
        });
        if (request !== catalogRequest.current) return;
        setCatalog({ endpoint: catalogEndpoint, value });
        if (value.defaultModel)
          setChosen((current) => ({
            ...current,
            fingerprint: current.fingerprint || value.defaultModel,
          }));
      } catch (e) {
        if (request === catalogRequest.current)
          setCatalogError(errorOf(e).message);
      } finally {
        if (request === catalogRequest.current) setCatalogLoading(false);
      }
    },
    [gateway.clientId, provider.id, catalogEndpoint, gateway.revision],
  );
  useEffect(() => {
    void loadCatalog(false);
    return () => {
      ++catalogRequest.current;
    };
  }, [loadCatalog]);
  const perform = async (work: () => Promise<void>) => {
    if (pending.current) return;
    pending.current = true;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await work();
      await refresh();
    } catch (e) {
      fail(e);
    } finally {
      pending.current = false;
      setBusy(false);
    }
  };
  const leave = async () => {
    if (busy || !(await flushTestPrompt())) return;
    if (!dirty || (await confirmAction("放弃未保存的修改？"))) close();
  };
  const scopedJobs =
    state?.jobs.filter(
      (j) => j.clientId === gateway.clientId && j.providerId === provider.id,
    ) ?? [];
  const running = scopedJobs.filter(active);
  const visibleJobs =
    tab === "history"
      ? scopedJobs
      : scopedJobs.filter(
          (j) => j.kind === kind && (active(j) || j.id === detailId),
        );
  const currentDetail = detail?.job.id === detailId ? detail : null;
  const canStart =
    !busy &&
    !running.length &&
    !!state &&
    endpointAvailable &&
    (kind !== "connection" || !!prompt?.trim()) &&
    ((kind === "connection" && !model) || (modelAvailable && !catalogLoading));
  const start = () =>
    perform(async () => {
      if (!(await promptState.flush())) return;
      const promptSnapshot = promptState.snapshot();
      const current = await command("get_gateway", {
        clientId: gateway.clientId,
      });
      const job = await command("provider_test_start", {
        request: {
          ...scope,
          expectedRevision: current.revision,
          endpoint,
          model,
          kind,
          concurrency:
            kind === "fingerprint" ? Math.min(concurrency, sampleCount) : 1,
          tokenizer:
            kind === "fingerprint" &&
            platform?.engine === "lmDetector" &&
            tokenizer,
          ...(kind === "connection" ? { prompt: promptSnapshot! } : {}),
        },
      });
      setDetailId(job.id);
    });
  const stop = () =>
    perform(async () => {
      for (const job of running)
        await command("provider_test_cancel", { id: job.id });
    });
  const retry = () =>
    perform(async () => {
      if (currentDetail)
        setDetailId(
          (await command("provider_test_retry", { id: currentDetail.job.id }))
            .id,
        );
    });
  const readDetection = async () => {
    await refresh(true);
  };
  const tabs: [Tab, string][] = [
    ["connection", "连接测试"],
    ...(fingerprint ? [["fingerprint", "模型指纹"] as [Tab, string]] : []),
    ["history", "测试结果"],
    ...(fingerprint
      ? ([
          ["detection", "定时检测"],
          ["bank", "指纹库"],
        ] as [Tab, string][])
      : []),
  ];
  const status = state?.detectionStatus.find(
    (s) => s.providerId === provider.id,
  );
  const mark = state?.degraded[provider.id];
  const interval =
    detection?.mode === "smart"
      ? detection.minIntervalMinutes
      : detection?.intervalMinutes;
  const intervalValid =
    Number.isInteger(interval) &&
    interval! >= (detection?.mode === "smart" ? 5 : 1) &&
    interval! <= (detection?.mode === "smart" ? 30 : 525600);
  const endpointSelect = (value: string, change: (value: string) => void) => (
    <label className="provider-test-field provider-test-full">
      <span>端点</span>
      <select
        aria-label="测试端点"
        value={value}
        disabled={busy || (testing && !!running.length)}
        onChange={(e) => change(e.target.value)}
      >
        {!endpoints.includes(value) && (
          <option value={value} disabled>
            {value}（已移除）
          </option>
        )}
        {endpoints.map((url, index) => (
          <option key={url} value={url}>
            {index === 0 ? "主端点" : `辅助端点 ${index}`} · {url}
          </option>
        ))}
      </select>
    </label>
  );
  const modelSelect = (
    value: string,
    change: (value: string) => void,
    label: string,
    allowDefault: boolean,
  ) => (
    <label className="provider-test-field provider-test-full">
      <span>模型</span>
      <div className="provider-test-model-control">
        <select
          aria-label={label}
          value={value}
          disabled={busy || catalogLoading || (testing && !!running.length)}
          onChange={(e) => change(e.target.value)}
        >
          {allowDefault ? (
            <option value="">默认模型</option>
          ) : (
            !value && <option value="">选择模型</option>
          )}
          {value && !models?.models.some((m) => m.id === value) && (
            <option value={value} disabled>
              {value}
              {models?.fetchedAt ? "（当前不可用）" : "（尚未拉取）"}
            </option>
          )}
          {models?.models.map((m) => (
            <option key={m.id} value={m.id}>
              {m.name === m.id ? m.id : `${m.name} (${m.id})`}
            </option>
          ))}
        </select>
        <button
          type="button"
          className="icon-button"
          aria-label="重新拉取模型"
          title="重新拉取模型"
          disabled={busy || catalogLoading || (testing && !!running.length)}
          onClick={() => void loadCatalog(true)}
        >
          <RefreshCw size={16} className={catalogLoading ? "spin" : ""} />
        </button>
      </div>
    </label>
  );
  const platformControls = (manual: boolean) =>
    platform && (
      <div className="provider-test-platform provider-test-full">
        <label className="provider-test-field">
          <span>检测源</span>
          <select
            aria-label="检测源"
            value={platform.engine}
            disabled={busy}
            onChange={(e) =>
              void savePlatform({
                ...platform,
                engine: e.target.value as DetectionEngine,
              })
            }
          >
            <option value="modelTrace">ModelTrace</option>
            <option value="lmDetector">LM Detector</option>
          </select>
        </label>
        <label className="provider-test-field">
          <span>采样组数</span>
          <select
            aria-label="采样组数"
            value={sampleCount}
            disabled={busy}
            onChange={(e) =>
              void savePlatform({
                ...platform,
                [platform.engine === "lmDetector"
                  ? "lmDetectorSamples"
                  : "modelTraceSamples"]: Number(e.target.value),
              })
            }
          >
            <option value={1}>1 组 · 快速判断</option>
            <option value={3}>3 组 · 汇总判断</option>
          </select>
        </label>
        {manual && (
          <label className="provider-test-field">
            <span>并发</span>
            <select
              aria-label="指纹并发"
              value={Math.min(concurrency, sampleCount)}
              disabled={busy || !!running.length}
              onChange={(e) => setConcurrency(Number(e.target.value))}
            >
              {Array.from({ length: sampleCount }, (_, i) => i + 1).map((n) => (
                <option key={n} value={n}>
                  {n}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>
    );
  const savePlatform = (next: DetectionPlatform) =>
    perform(async () => {
      await command("provider_test_set_platform", {
        clientId: gateway.clientId,
        platform: next,
      });
    });
  const catalogStatus = (
    <>
      {catalogLoading ? (
        <p className="provider-test-hint" role="status">
          正在读取模型目录…
        </p>
      ) : (
        !models?.fetchedAt && (
          <p className="provider-test-hint">
            尚未缓存该端点模型，点击重新拉取。
          </p>
        )
      )}
      {models?.fetchedAt && (
        <p className="provider-test-hint">上次拉取 {date(models.fetchedAt)}</p>
      )}
      {models?.errorDetail ? (
        <ErrorDetail detail={models.errorDetail} />
      ) : (
        (catalogError || models?.error) && (
          <p className="provider-test-error" role="alert">
            {catalogError || models?.error}
          </p>
        )
      )}
    </>
  );
  return (
    <Modal
      title={`${provider.name} · 测试`}
      close={() => void leave()}
      busy={busy}
      className="provider-testing"
    >
      <div className="provider-test-tabs" role="tablist" aria-label="测试管理">
        {tabs.map(([id, name], index) => (
          <button
            key={id}
            type="button"
            id={`${tabsId}-${id}`}
            role="tab"
            aria-selected={tab === id}
            aria-controls={`${tabsId}-panel`}
            tabIndex={tab === id ? 0 : -1}
            onClick={() => setTab(id)}
            onKeyDown={(e) => {
              const next =
                e.key === "ArrowRight"
                  ? (index + 1) % tabs.length
                  : e.key === "ArrowLeft"
                    ? (index + tabs.length - 1) % tabs.length
                    : e.key === "Home"
                      ? 0
                      : e.key === "End"
                        ? tabs.length - 1
                        : null;
              if (next == null) return;
              e.preventDefault();
              setTab(tabs[next][0]);
              document.getElementById(`${tabsId}-${tabs[next][0]}`)?.focus();
            }}
          >
            {name}
          </button>
        ))}
      </div>
      <div
        className="provider-test-content"
        id={`${tabsId}-panel`}
        role="tabpanel"
        aria-labelledby={`${tabsId}-${tab}`}
      >
        {testing && (
          <>
            <div className="provider-test-config">
              {endpointSelect(endpoint, (value) => {
                setEndpoint(value);
                setChosen({ connection: "", fingerprint: "" });
              })}
              {modelSelect(
                model,
                (value) =>
                  setChosen((previous) => ({ ...previous, [kind]: value })),
                "测试模型",
                kind === "connection",
              )}
              {kind === "fingerprint" && platformControls(true)}
              {kind === "fingerprint" && platform?.engine === "lmDetector" && (
                <label className="provider-test-tokenizer provider-test-full">
                  <input
                    type="checkbox"
                    checked={tokenizer}
                    disabled={busy || !!running.length}
                    onChange={(e) => setTokenizer(e.target.checked)}
                  />
                  <span>
                    附加分词器探测 <small>最多 22 次短请求，仅供参考</small>
                  </span>
                </label>
              )}
            </div>
            {catalogStatus}
            {model && models?.fetchedAt && !modelAvailable && (
              <p className="provider-test-error" role="alert">
                所选模型不在该端点缓存目录中，请重新选择。
              </p>
            )}
            {kind === "connection" && (
              <label className="provider-test-field provider-test-prompt">
                <span>测试文案 · 全局</span>
                <textarea
                  aria-label="测试文案"
                  value={prompt ?? ""}
                  disabled={prompt === null}
                  rows={3}
                  onChange={(e) => promptState.change(e.target.value)}
                />
                {promptState.saving && <span role="status">正在保存…</span>}
                {promptState.error && (
                  <span className="provider-test-error" role="alert">
                    {promptState.error}{" "}
                    <button
                      type="button"
                      className="text-button"
                      onClick={() => void promptState.flush()}
                    >
                      重试保存
                    </button>
                  </span>
                )}
              </label>
            )}
          </>
        )}
        {tab === "connection" && (
          <ConnectionReply
            key={detailId ?? "empty"}
            detail={
              currentDetail?.job.kind === "connection" ? currentDetail : null
            }
            loading={!!detailId && !currentDetail}
          />
        )}
        {(testing || tab === "history") && (
          <>
            {!!visibleJobs.length && (
              <div className="provider-test-results" aria-label="测试记录">
                <div className="provider-test-result-head">
                  <span>时间</span>
                  <span>测试</span>
                  <span>状态</span>
                  <span>耗时</span>
                  <span />
                </div>
                {visibleJobs.map((job) => (
                  <div
                    key={job.id}
                    className={`provider-test-row${job.id === detailId ? " selected" : ""}`}
                  >
                    <button
                      className="provider-test-job"
                      aria-label={`${job.kind === "fingerprint" ? "模型指纹" : modes[job.mode]} ${statuses[job.status] ?? job.status}`}
                      onClick={() => setDetailId(job.id)}
                    >
                      <span>{date(job.createdAt)}</span>
                      <span>
                        {job.automatic
                          ? "定时检测"
                          : job.kind === "fingerprint"
                            ? "模型指纹"
                            : modes[job.mode]}
                      </span>
                      <span
                        className={
                          job.status === "failed" ? "provider-test-error" : ""
                        }
                      >
                        {statuses[job.status] ?? job.status}
                      </span>
                      <span className="provider-test-number">
                        {duration(job, now)}
                      </span>
                    </button>
                    {active(job) ? (
                      <button
                        className="text-button"
                        disabled={busy}
                        onClick={() =>
                          void perform(() =>
                            command("provider_test_cancel", { id: job.id }),
                          )
                        }
                      >
                        停止
                      </button>
                    ) : (
                      <span />
                    )}
                  </div>
                ))}
              </div>
            )}
            {tab === "history" && !visibleJobs.length && (
              <p className="provider-test-empty">暂无测试结果</p>
            )}
            {currentDetail &&
              (tab === "history" || currentDetail.job.kind === kind) && (
                <TestResult
                  detail={currentDetail}
                  now={now}
                  showReply={tab === "history"}
                />
              )}
          </>
        )}
        {tab === "detection" && detection && (
          <>
            <label className="provider-test-toggle">
              <span>启用定时检测</span>
              <input
                type="checkbox"
                checked={detection.enabled}
                disabled={
                  busy ||
                  (!detection.enabled &&
                    (!detection.model || !detectionModelAvailable))
                }
                onChange={(e) =>
                  setDetection({ ...detection, enabled: e.target.checked })
                }
              />
            </label>
            <p className="provider-test-hint">
              {clientName(gateway.clientId)}{" "}
              的所有供应商共用检测源和组数，改选后自动保存，新任务生效。模型、端点和周期仅用于本供应商。
            </p>
            {platformControls(false)}
            <div className="provider-test-detection-config">
              {endpointSelect(detection.endpoint ?? provider.baseUrl, (value) =>
                setDetection({ ...detection, endpoint: value }),
              )}
              <div className="provider-test-full">
                {modelSelect(
                  detection.model,
                  (value) => setDetection({ ...detection, model: value }),
                  "检测模型",
                  false,
                )}
              </div>
              <label className="provider-test-field">
                <span>检测模式</span>
                <select
                  aria-label="检测模式"
                  value={detection.mode}
                  disabled={busy}
                  onChange={(e) =>
                    setDetection({
                      ...detection,
                      mode: e.target.value as "smart" | "fixed",
                    })
                  }
                >
                  <option value="smart">智能</option>
                  <option value="fixed">固定周期</option>
                </select>
              </label>
              <label className="provider-test-field">
                <span>
                  {detection.mode === "smart"
                    ? "最小间隔（分钟）"
                    : "检测周期（分钟）"}
                </span>
                <input
                  aria-label={
                    detection.mode === "smart" ? "最小间隔" : "检测周期"
                  }
                  type="number"
                  step="1"
                  min={detection.mode === "smart" ? 5 : 1}
                  max={detection.mode === "smart" ? 30 : 525600}
                  value={interval}
                  disabled={busy}
                  onChange={(e) =>
                    setDetection({
                      ...detection,
                      [detection.mode === "smart"
                        ? "minIntervalMinutes"
                        : "intervalMinutes"]: Number(e.target.value),
                    })
                  }
                />
              </label>
            </div>
            {catalogStatus}
            <dl className="provider-test-summary provider-test-detection-summary">
              <div>
                <dt>状态</dt>
                <dd>
                  {status ? (statuses[status.status] ?? status.status) : "—"}
                </dd>
              </div>
              <div>
                <dt>上次检测开始</dt>
                <dd>{date(status?.lastStartedAt)}</dd>
              </div>
              <div>
                <dt>最近一小时</dt>
                <dd>
                  {status?.recentRequests == null
                    ? "暂不可用"
                    : `${status.recentRequests} 次`}
                </dd>
              </div>
              <div>
                <dt>当前周期</dt>
                <dd>
                  {status?.intervalMinutes == null
                    ? "—"
                    : `${status.intervalMinutes} 分钟`}
                </dd>
              </div>
              <div>
                <dt>下次检测</dt>
                <dd>{date(status?.nextAt)}</dd>
              </div>
              {status?.cooldownUntil && (
                <div>
                  <dt>冷却至</dt>
                  <dd>{date(status.cooldownUntil)}</dd>
                </div>
              )}
            </dl>
            {status?.error && (
              <p className="provider-test-error" role="alert">
                {status.error}
              </p>
            )}
            {mark && (
              <div className="provider-test-mark">
                <span>
                  {mark.basis === "sample_family" ? (
                    <>
                      降智：{basisLabel(mark.basis, mark.groupIndexes)} · 请求{" "}
                      {mark.requestedModel}
                    </>
                  ) : (
                    <>
                      降智：{mark.requestedModel} → {mark.predictedModel}（
                      {percent(mark.probability)}）
                      {mark.basis && ` · ${basisLabel(mark.basis)}`}
                    </>
                  )}
                </span>
                <button
                  className="secondary compact"
                  disabled={busy || dirty}
                  onClick={() =>
                    void perform(async () => {
                      const current = await refresh();
                      if (!current) return;
                      await command("provider_test_clear_mark", {
                        ...scope,
                        expectedRevision: current.revision,
                      });
                      await readDetection();
                    })
                  }
                >
                  清除标记
                </button>
                <span>
                  {mark.queueRemoved ? "已退出自动队列" : "退队未完成"}
                </span>
                {mark.error && <span role="alert">{mark.error}</span>}
                <span>重新入队请使用供应商队列开关。</span>
              </div>
            )}
            {running.length > 0 && (
              <button
                className="text-button provider-test-current"
                onClick={() => {
                  setDetailId(running[0].id);
                  setTab("history");
                }}
              >
                查看当前任务
              </button>
            )}
          </>
        )}
        {tab === "bank" && selectedBank && (
          <>
            <label className="provider-test-field provider-test-bank-source">
              <span>检测源</span>
              <select
                aria-label="指纹库检测源"
                value={bankEngine}
                disabled={busy || selectedBank.updating}
                onChange={(e) =>
                  setBankEngine(e.target.value as DetectionEngine)
                }
              >
                <option value="modelTrace">ModelTrace</option>
                <option value="lmDetector">LM Detector</option>
              </select>
            </label>
            <dl className="provider-test-summary">
              <div>
                <dt>来源</dt>
                <dd>
                  {selectedBank.source === "builtin" ||
                  selectedBank.source === "bundled"
                    ? "内置"
                    : selectedBank.source === "cache"
                      ? "已更新"
                      : "—"}
                </dd>
              </div>
              <div>
                <dt>版本</dt>
                <dd>
                  {typeof selectedBank.version?.revision === "string" &&
                  selectedBank.version.revision
                    ? selectedBank.version.revision.slice(0, 7)
                    : "—"}
                </dd>
              </div>
              <div>
                <dt>检查时间</dt>
                <dd>{date(selectedBank.checkedAt)}</dd>
              </div>
              {bankEngine === "lmDetector" && (
                <>
                  <div>
                    <dt>更新时间</dt>
                    <dd>{date(selectedBank.updatedAt)}</dd>
                  </div>
                  <div>
                    <dt>更新结果</dt>
                    <dd role="status">
                      {busy || selectedBank.updating
                        ? "检查中"
                        : selectedBank.error
                          ? "检查失败"
                          : (bankResults[selectedBank.result ?? ""] ?? "—")}
                    </dd>
                  </div>
                </>
              )}
            </dl>
            <div className="provider-test-bank">
              <div className="provider-test-bank-head">
                <span>模型</span>
                <span>标识</span>
              </div>
              {selectedBank.models.map((model) => (
                <div key={model.id}>
                  <span>{model.name}</span>
                  <span>{model.id}</span>
                </div>
              ))}
            </div>
            {selectedBank.error && (
              <p className="provider-test-error" role="alert">
                {selectedBank.error}
              </p>
            )}
          </>
        )}
      </div>
      <footer className="provider-test-footer">
        {(error || state?.error) && (
          <p className="provider-test-error" role="alert">
            {error || state?.error}
          </p>
        )}
        {notice && <p role="status">{notice}</p>}
        <div className="provider-test-actions">
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={() => void leave()}
          >
            关闭
          </button>
          {testing && (
            <>
              {currentDetail?.job.canRetry && (
                <button
                  className="secondary"
                  disabled={busy || !!running.length}
                  onClick={() => void retry()}
                >
                  重试失败组
                </button>
              )}
              {running.length ? (
                <button
                  className="primary"
                  disabled={busy}
                  onClick={() => void stop()}
                >
                  停止测试
                </button>
              ) : (
                <button
                  className="primary"
                  disabled={!canStart}
                  onClick={() => void start()}
                >
                  开始测试
                </button>
              )}
            </>
          )}
          {tab === "detection" && detection && (
            <>
              <button
                className="secondary"
                disabled={busy}
                onClick={() => void perform(readDetection)}
              >
                重新读取
              </button>
              <button
                className="primary"
                disabled={
                  busy ||
                  !intervalValid ||
                  (detection.enabled &&
                    (!detectionModelAvailable ||
                      catalogLoading ||
                      !endpoints.includes(
                        detection.endpoint ?? provider.baseUrl,
                      )))
                }
                onClick={() =>
                  void perform(async () => {
                    await command("provider_test_set_detection", {
                      ...scope,
                      detection,
                      expectedRevision: detectionRevision,
                    });
                    await readDetection();
                    setNotice("检测设置已保存");
                  })
                }
              >
                保存
              </button>
            </>
          )}
          {tab === "bank" && (
            <button
              className="primary"
              disabled={busy || selectedBank?.updating}
              onClick={() =>
                void perform(async () => {
                  const updated = await command("provider_test_update_bank", {
                    engine: bankEngine,
                  });
                  if (bankEngine === "lmDetector") {
                    return;
                  }
                  if (updated.error) throw new Error(updated.error);
                  setNotice(`${engineNames[bankEngine]} 指纹库已更新`);
                })
              }
            >
              {bankEngine === "lmDetector" ? "检查更新" : "更新指纹库"}
            </button>
          )}
        </div>
      </footer>
    </Modal>
  );
}

function ErrorDetail({ detail }: { detail: ProviderTestError }) {
  const [copied, setCopied] = useState(false),
    [copyError, setCopyError] = useState("");
  const text = [
    detail.httpStatus != null ? `HTTP ${detail.httpStatus}` : "",
    detail.message,
    detail.body,
    detail.truncated ? "错误正文超出响应上限或读取中断，以上内容不完整。" : "",
  ]
    .filter(Boolean)
    .join("\n");
  return (
    <section
      className="provider-test-error-detail"
      aria-label="完整错误信息"
      role="alert"
    >
      <div>
        <strong>
          {detail.httpStatus != null ? `HTTP ${detail.httpStatus}` : "连接错误"}
        </strong>
        <button
          type="button"
          className="text-button"
          onClick={() => {
            setCopyError("");
            void navigator.clipboard
              .writeText(text)
              .then(() => setCopied(true))
              .catch(() => setCopyError("复制失败，请选择错误正文后复制。"));
          }}
        >
          <Copy size={14} />
          {copied ? "已复制" : "复制错误"}
        </button>
      </div>
      <p>{detail.message}</p>
      {detail.body && <pre>{detail.body}</pre>}
      {detail.truncated && (
        <p>错误正文超出响应上限或读取中断，以上内容不完整。</p>
      )}
      {copyError && <p>{copyError}</p>}
    </section>
  );
}

function ConnectionReply({
  detail,
  loading = false,
}: {
  detail: ProviderTestDetail | null;
  loading?: boolean;
}) {
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState("");
  const job = detail?.job;
  const output = detail?.output;
  const text = output?.text ?? "";
  useEffect(() => {
    setCopied(false);
    setCopyError("");
  }, [job?.id, text]);
  return (
    <section className="provider-test-reply" aria-label="模型回复">
      <div className="provider-test-reply-heading">
        <h3>模型回复</h3>
        {job && <span>{statuses[job.status] ?? job.status}</span>}
        <button
          type="button"
          className="text-button"
          disabled={!text}
          onClick={() => {
            setCopyError("");
            void navigator.clipboard
              .writeText(text)
              .then(() => setCopied(true))
              .catch(() => setCopyError("复制失败，请选择回复正文后复制。"));
          }}
        >
          <Copy size={14} />
          {copied ? "已复制" : "复制回复"}
        </button>
      </div>
      {text ? (
        <pre className="provider-test-output">{text}</pre>
      ) : (
        <p className="provider-test-empty" role="status">
          {loading
            ? "正在读取回复…"
            : !job
              ? "开始测试后显示回复"
              : !output
                ? "回复正文未保留，请重新测试。"
                : active(job)
                  ? "正在等待回复…"
                  : "未收到文本回复"}
        </p>
      )}
      {output?.error ? (
        <ErrorDetail detail={output.error} />
      ) : (
        job?.error && (
          <p className="provider-test-error" role="alert">
            {job.error}
          </p>
        )
      )}
      {copyError && (
        <p className="provider-test-error" role="alert">
          {copyError}
        </p>
      )}
    </section>
  );
}

function TestResult({
  detail: { job, output },
  now,
  showReply,
}: {
  detail: ProviderTestDetail;
  now: number;
  showReply: boolean;
}) {
  return (
    <section className="provider-test-detail" aria-label="测试详情">
      {showReply && job.kind === "connection" && (
        <ConnectionReply detail={{ job, output }} />
      )}
      <dl className="provider-test-summary">
        {job.kind === "fingerprint" && (
          <>
            <div>
              <dt>结论</dt>
              <dd>
                {active(job)
                  ? "检测中"
                  : job.report?.disposition === "downgrade"
                    ? "判定降智"
                    : job.report?.disposition === "notDowngrade"
                      ? "未命中降智规则"
                      : job.report?.ambiguous
                        ? "无法确定"
                        : "无法判定"}
                {job.report?.initial && (
                  <span className="provider-test-inline-meta">初步判断</span>
                )}
              </dd>
            </div>
            <div>
              <dt>检测源</dt>
              <dd>
                {engineNames[job.engine]} · {job.report?.used_outputs ?? 0} /{" "}
                {job.sampleCount} 组有效
                {!active(job) &&
                  job.report &&
                  job.report.used_outputs < job.sampleCount && (
                    <span className="provider-test-inline-meta">
                      样本不足，仅供参考
                    </span>
                  )}
              </dd>
            </div>
          </>
        )}
        <div>
          <dt>测试端点</dt>
          <dd>{job.endpoint ?? "历史记录未保存端点"}</dd>
        </div>
        <div>
          <dt>请求模型</dt>
          <dd>{job.requestedModel}</dd>
        </div>
        <div>
          <dt>转发模型</dt>
          <dd>{job.forwardedModel}</dd>
        </div>
        <div>
          <dt>返回模型</dt>
          <dd>{job.returnedModel ?? "—"}</dd>
        </div>
        <div>
          <dt>耗时</dt>
          <dd>
            {duration(job, now)}
            <span className="provider-test-inline-meta">
              首字节{" "}
              {job.firstByteMs == null
                ? "—"
                : `${(job.firstByteMs / 1000).toFixed(2)} s`}
            </span>
          </dd>
        </div>
        {job.report && (
          <>
            {job.report.disposition_basis && (
              <div>
                <dt>判定依据</dt>
                <dd>
                  {basisLabel(
                    job.report.disposition_basis,
                    job.report.disposition_group_indexes,
                  )}
                </dd>
              </div>
            )}
            <div>
              <dt>预测模型</dt>
              <dd>{job.report.prediction_name || job.report.prediction}</dd>
            </div>
            <div>
              <dt>置信度</dt>
              <dd>
                {percent(job.report.probability)}
                <span className="provider-test-inline-meta">
                  有效组 {job.report.used_outputs}
                </span>
              </dd>
            </div>
          </>
        )}
      </dl>
      {!!job.report?.results.length && (
        <div className="provider-test-ranking" aria-label="候选排名">
          <div>
            <span>候选模型</span>
            <span>评分</span>
            <span>置信度</span>
          </div>
          {job.report.results.slice(0, 5).map((r, i) => (
            <div key={r.model}>
              <span>
                {i + 1}. {r.display_name || r.model}
              </span>
              <span>{r.score.toFixed(3)}</span>
              <span>{percent(r.probability)}</span>
            </div>
          ))}
        </div>
      )}
      {job.tokenizerResult && (
        <details className="provider-test-diagnostics">
          <summary>
            分词器探测 ·{" "}
            {statuses[job.tokenizerResult.status] ?? job.tokenizerResult.status}
            {job.tokenizerResult.verdict
              ? job.tokenizerResult.verdict.kind === "unknown"
                ? " · 无法确定"
                : ` · ${job.tokenizerResult.verdict.kind === "exact" ? "匹配" : "相近"} ${job.tokenizerResult.verdict.top.id}`
              : ""}
          </summary>
          <p className="provider-test-hint">
            分词器结果独立展示，不参与降智判定或退队。
          </p>
          {job.tokenizerResult.baselineDrift && (
            <p className="provider-test-hint">
              两次对照用量不一致，上游可能添加了变化的隐藏输入。
            </p>
          )}
          {job.tokenizerResult.error && (
            <p className="provider-test-error">{job.tokenizerResult.error}</p>
          )}
          {job.tokenizerResult.verdict && (
            <p>
              置信度 {percent(job.tokenizerResult.verdict.confidence)}
              {job.tokenizerResult.verdict.claim &&
                ` · 与请求型号${{ consistent: "一致", inconsistent: "不一致", uncertain: "关系无法确定" }[job.tokenizerResult.verdict.claim.status] ?? "关系无法确定"}`}
            </p>
          )}
          {job.tokenizerResult.steps?.map((step, i) => (
            <div key={i} className="provider-test-tokenizer-step">
              <span>{step.probe ?? "对照"}</span>
              <span>
                {step.tokens == null
                  ? (step.error ?? "等待用量")
                  : `${step.tokens} tokens`}
              </span>
            </div>
          ))}
        </details>
      )}
      {job.completionReason && (
        <p className="provider-test-hint">
          {job.completionReason === "confidence99"
            ? "匹配度达到 99%，提前完成"
            : "首组命中降智规则，提前完成"}
        </p>
      )}
      {job.kind !== "connection" && output?.error && (
        <ErrorDetail detail={output.error} />
      )}
      {job.kind !== "connection" && job.error && !output?.error && (
        <p className="provider-test-error" role="alert">
          {job.error}
        </p>
      )}
      {job.kind !== "connection" && output?.text && (
        <pre className="provider-test-output">{output.text}</pre>
      )}
      {!!job.groups.length && (
        <div className="provider-test-groups">
          <div className="provider-test-group-head">
            <span>分组</span>
            <span>状态</span>
            <span>预测模型</span>
            <span>置信度</span>
            <span>耗时</span>
          </div>
          {job.groups.map((group) => (
            <details key={group.index}>
              <summary>
                <span>第 {group.index + 1} 组</span>
                <span>{statuses[group.status] ?? group.status}</span>
                <span>
                  {group.report?.prediction_name ||
                    group.report?.prediction ||
                    "—"}
                </span>
                <span>
                  {group.report ? percent(group.report.probability) : "—"}
                </span>
                <span>{(group.elapsedMs / 1000).toFixed(2)} s</span>
              </summary>
              <div className="provider-test-group-detail">
                <dl className="provider-test-summary">
                  {group.report?.decision && (
                    <div>
                      <dt>评分状态</dt>
                      <dd>
                        {
                          {
                            scored: "已评分",
                            partial: "初步排名",
                            unscorable: "样本不足",
                            not_model: "异常序列",
                          }[group.report.decision]
                        }
                      </dd>
                    </div>
                  )}
                  {group.report?.diagnostics?.[0] && (
                    <div>
                      <dt>有效整数</dt>
                      <dd>
                        {group.report.diagnostics[0].parsed_numbers} / 至少{" "}
                        {group.report.diagnostics[0].minimum_numbers}
                      </dd>
                    </div>
                  )}
                  <div>
                    <dt>目标整数</dt>
                    <dd>{group.expected}</dd>
                  </div>
                  <div>
                    <dt>已派发</dt>
                    <dd>{group.attempts} 次</dd>
                  </div>
                  <div>
                    <dt>返回模型</dt>
                    <dd>{group.returnedModel ?? "—"}</dd>
                  </div>
                </dl>
                {output?.groupErrors[String(group.index)] && (
                  <ErrorDetail
                    detail={output.groupErrors[String(group.index)]}
                  />
                )}
                {group.error && !output?.groupErrors[String(group.index)] && (
                  <p className="provider-test-error" role="alert">
                    {group.error}
                  </p>
                )}
                {group.uncertain && (
                  <p className="provider-test-hint">
                    上游结果未知，未自动重放。
                  </p>
                )}
                {!!Object.keys(group.diagnostics ?? {}).length && (
                  <pre>{JSON.stringify(group.diagnostics, null, 2)}</pre>
                )}
                {output?.groups[String(group.index)] && (
                  <pre>{output.groups[String(group.index)]}</pre>
                )}
              </div>
            </details>
          ))}
        </div>
      )}
      {!output && (
        <p className="provider-test-hint">原始输出已释放，仅保留摘要。</p>
      )}
    </section>
  );
}
