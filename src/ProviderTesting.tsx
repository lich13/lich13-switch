import { useCallback, useEffect, useId, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { command } from "./bridge";
import { subscriptions } from "./subscriptions";
import Modal from "./Modal";
import { errorOf, type GatewayState, type Provider } from "./types";
import type {
  ProviderTestRequest,
  ProviderTestMode,
  ProviderTestKind,
  ProviderTestingState,
  ProviderTestDetail,
  ProviderTestModels,
  ProviderTestJob,
  ProviderDetection,
} from "./bridge-contracts";
import "./provider-testing.css";

// Old modes remain readable in history; new connections only send ordinary chat.
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
  idle: "无业务调用，已暂停",
  waiting: "等待下次检测",
};
type Tab = ProviderTestKind | "history" | "detection" | "bank";
const active = (job: ProviderTestJob) =>
  ["queued", "running"].includes(job.status);
const date = (at: number | null) =>
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
  initialKind = "connection",
  close,
}: {
  gateway: GatewayState;
  provider?: Provider;
  initialKind?: ProviderTestKind;
  close: () => void;
}) {
  const tabsId = useId();
  const [state, setState] = useState<ProviderTestingState | null>(null);
  const [tab, setTab] = useState<Tab>(initialKind);
  const kind: ProviderTestKind =
    tab === "fingerprint" ? "fingerprint" : "connection";
  const [selected, setSelected] = useState<string[]>(
    provider ? [provider.id] : [],
  );
  const [chosen, setChosen] = useState({ connection: "", fingerprint: "" });
  const model = chosen[kind];
  const [catalog, setCatalog] = useState<{
    providerId: string;
    value: ProviderTestModels;
  } | null>(null);
  const [catalogRefresh, setCatalogRefresh] = useState(0);
  const forceRefresh = useRef(false);
  const [catalogLoading, setCatalogLoading] = useState(false);
  const [catalogError, setCatalogError] = useState("");
  const [concurrency, setConcurrency] = useState(1);
  const [busy, setBusy] = useState(false);
  const pending = useRef(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [detailId, setDetailId] = useState<string | null>(null);
  const [detail, setDetail] = useState<ProviderTestDetail | null>(null);
  const [detection, setDetection] = useState<ProviderDetection | null>(null);
  const [detectionRevision, setDetectionRevision] = useState("");
  const [now, setNow] = useState(Date.now());
  const fail = useCallback((e: unknown) => setError(errorOf(e).message), []);
  const refresh = useCallback(
    async () => setState(await command("provider_test_state")),
    [],
  );
  useEffect(() => {
    void refresh().catch(fail);
    const events = subscriptions(fail);
    events.watch("provider-tests", refresh);
    return () => events.dispose();
  }, [refresh, fail]);
  useEffect(() => {
    if (!state || detection) return;
    setDetection(structuredClone(state.detection));
    setDetectionRevision(state.revision);
  }, [state, detection]);
  useEffect(() => {
    if (!detailId) {
      setDetail(null);
      return;
    }
    let disposed = false;
    void command("provider_test_query", { id: detailId })
      .then((value) => {
        if (!disposed) setDetail(value);
      })
      .catch((e) => {
        if (!disposed) fail(e);
      });
    return () => {
      disposed = true;
    };
  }, [detailId, state, fail]);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);

  const eligible = gateway.providers.filter(
    (p) => kind !== "fingerprint" || !p.searchOnly,
  );
  const selectedIds = selected.filter((id) =>
    eligible.some((p) => p.id === id),
  );
  const first = gateway.providers.find((p) => p.id === selectedIds[0]);
  const firstId = first?.id;
  const models =
    catalog && catalog.providerId === firstId ? catalog.value : null;
  useEffect(() => {
    setChosen({ connection: "", fingerprint: "" });
    setDetailId(null);
  }, [firstId, gateway.clientId]);
  useEffect(() => {
    let disposed = false;
    const reload = forceRefresh.current;
    forceRefresh.current = false;
    setCatalogError("");
    setCatalogLoading(Boolean(firstId));
    if (!firstId) return;
    void command("get_gateway", { clientId: gateway.clientId })
      .then((current) =>
        command("provider_test_models", {
          clientId: gateway.clientId,
          providerId: firstId,
          expectedRevision: current.revision,
          refresh: reload,
        }),
      )
      .then((value) => {
        if (disposed) return;
        setCatalog({ providerId: firstId, value });
        setChosen((current) => ({
          ...current,
          fingerprint: current.fingerprint || value.defaultModel,
        }));
      })
      .catch((e) => {
        if (!disposed) setCatalogError(errorOf(e).message);
      })
      .finally(() => {
        if (!disposed) setCatalogLoading(false);
      });
    return () => {
      disposed = true;
    };
  }, [firstId, gateway.clientId, catalogRefresh]);

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
  const scopedJobs =
    state?.jobs.filter(
      (j) =>
        j.clientId === gateway.clientId &&
        (!provider || j.providerId === provider.id),
    ) ?? [];
  const running = scopedJobs.filter(
    (j) => active(j) && selectedIds.includes(j.providerId),
  );
  const currentBatch = scopedJobs.find((j) => j.id === detailId)?.batchId;
  const visibleJobs =
    tab === "history"
      ? scopedJobs
      : scopedJobs.filter(
          (j) =>
            j.kind === kind &&
            (active(j) ||
              j.id === detailId ||
              (currentBatch && j.batchId === currentBatch)),
        );
  const currentDetail = detail?.job.id === detailId ? detail : null;
  const modelAvailable = !!models?.models.some((m) => m.id === model);
  const canStart =
    selectedIds.length > 0 &&
    !busy &&
    !running.length &&
    ((kind === "connection" && !model) ||
      (modelAvailable && !catalogLoading && !catalogError && !models?.stale));
  const start = () =>
    perform(async () => {
      const current = await command("get_gateway", {
        clientId: gateway.clientId,
      });
      const entries: ProviderTestRequest[] = selectedIds.map((providerId) => ({
        clientId: gateway.clientId,
        providerId,
        expectedRevision: current.revision,
        model,
        kind,
        concurrency: kind === "fingerprint" ? concurrency : 1,
      }));
      if (entries.length === 1) {
        const job = await command("provider_test_start", {
          request: entries[0],
        });
        setDetailId(job.id);
      } else {
        const batch = await command("provider_test_batch", {
          requests: entries,
        });
        setNotice(`已启动 ${batch.jobs.length} 项测试`);
        if (batch.jobs[0]) setDetailId(batch.jobs[0].id);
        if (batch.errors.length)
          setError(
            batch.errors
              .map(
                (e) =>
                  `${gateway.providers.find((p) => p.id === e.providerId)?.name ?? "供应商"}：${e.error}`,
              )
              .join("；"),
          );
      }
    });
  const stop = () =>
    perform(async () => {
      const batches = [
        ...new Set(running.flatMap((j) => (j.batchId ? [j.batchId] : []))),
      ];
      for (const id of batches)
        await command("provider_test_stop_batch", { id });
      for (const job of running.filter((j) => !j.batchId))
        await command("provider_test_cancel", { id: job.id });
    });
  const retry = () =>
    perform(async () => {
      if (currentDetail) {
        const job = await command("provider_test_retry", {
          id: currentDetail.job.id,
        });
        setDetailId(job.id);
      }
    });
  const setOverride = (
    id: string,
    values: Partial<ProviderDetection["providers"][string]>,
  ) =>
    setDetection(
      (d) =>
        d && {
          ...d,
          providers: {
            ...d.providers,
            [id]: {
              ...(d.providers[id] ?? { enabled: null, model: null }),
              ...values,
            },
          },
        },
    );
  const readDetection = async () => {
    const current = await command("provider_test_state");
    setState(current);
    setDetection(structuredClone(current.detection));
    setDetectionRevision(current.revision);
  };
  const testing = tab === "connection" || tab === "fingerprint";
  const tabs: [Tab, string][] = [
    ["connection", "连接测试"],
    ...(gateway.clientId === "codex" && !provider?.searchOnly
      ? [["fingerprint", "模型测试"] as [Tab, string]]
      : []),
    ["history", "测试结果"],
    ...(gateway.clientId === "codex"
      ? ([
          ["detection", "定时检测"],
          ["bank", "指纹库"],
        ] as [Tab, string][])
      : []),
  ];

  return (
    <Modal
      title={provider ? `${provider.name} 测试` : "供应商测试"}
      close={close}
      className="provider-testing"
    >
      <div className="provider-test-tabs" role="tablist" aria-label="测试管理">
        {tabs.map(([id, name], index) => (
          <button
            key={id}
            id={`${tabsId}-${id}`}
            type="button"
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
            {!provider && (
              <fieldset className="provider-test-selection">
                <legend>供应商</legend>
                <label className="provider-test-select-all">
                  <input
                    type="checkbox"
                    aria-label="全选"
                    checked={
                      selectedIds.length > 0 &&
                      selectedIds.length === eligible.length
                    }
                    onChange={(e) =>
                      setSelected(
                        e.target.checked ? eligible.map((p) => p.id) : [],
                      )
                    }
                  />
                  全选
                  <span>
                    {selectedIds.length} / {eligible.length}
                  </span>
                </label>
                <div className="provider-test-options">
                  {gateway.providers.map((p) => (
                    <label key={p.id}>
                      <input
                        type="checkbox"
                        checked={selectedIds.includes(p.id)}
                        disabled={kind === "fingerprint" && p.searchOnly}
                        onChange={(e) =>
                          setSelected((ids) =>
                            e.target.checked
                              ? [...ids, p.id]
                              : ids.filter((id) => id !== p.id),
                          )
                        }
                      />
                      <span>
                        {p.name}
                        {p.searchOnly ? " · 仅网络搜索" : ""}
                      </span>
                    </label>
                  ))}
                </div>
              </fieldset>
            )}
            <div
              className={`provider-test-config${kind === "fingerprint" ? " with-concurrency" : ""}`}
            >
              <label className="provider-test-field">
                <span>模型</span>
                <div className="provider-test-model-control">
                  <select
                    aria-label="测试模型"
                    value={model}
                    disabled={
                      busy || !!running.length || catalogLoading || !first
                    }
                    onChange={(e) =>
                      setChosen((current) => ({
                        ...current,
                        [kind]: e.target.value,
                      }))
                    }
                  >
                    {kind === "connection" ? (
                      <option value="">默认模型</option>
                    ) : (
                      !model && <option value="">选择模型</option>
                    )}
                    {model && !modelAvailable && (
                      <option value={model} disabled>
                        {model}
                        {!catalogLoading && models && !catalogError
                          ? "（当前不可用）"
                          : ""}
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
                    aria-label="刷新模型目录"
                    title="刷新模型目录"
                    disabled={
                      !first || catalogLoading || busy || !!running.length
                    }
                    onClick={() => {
                      forceRefresh.current = true;
                      setCatalogRefresh((n) => n + 1);
                    }}
                  >
                    <RefreshCw
                      size={16}
                      className={catalogLoading ? "spin" : undefined}
                    />
                  </button>
                </div>
              </label>
              {kind === "fingerprint" && (
                <label className="provider-test-field">
                  <span>组并发</span>
                  <select
                    aria-label="组并发"
                    value={concurrency}
                    disabled={busy || !!running.length}
                    onChange={(e) => setConcurrency(Number(e.target.value))}
                  >
                    {[1, 2, 3].map((n) => (
                      <option key={n} value={n}>
                        {n}
                      </option>
                    ))}
                  </select>
                </label>
              )}
            </div>
            {catalogLoading && (
              <p className="provider-test-hint" role="status">
                正在读取模型目录…
              </p>
            )}
            {(catalogError || models?.stale) && (
              <p className="provider-test-error" role="alert">
                {catalogError || models?.error}。
                {kind === "connection"
                  ? "可使用默认模型测试，或刷新目录重试。"
                  : "请刷新目录后重试。"}
              </p>
            )}
            {model &&
              models &&
              !modelAvailable &&
              !catalogError &&
              !catalogLoading && (
                <p className="provider-test-error" role="alert">
                  所选模型不在当前供应商目录中，请重新选择。
                </p>
              )}
          </>
        )}
        {(testing || tab === "history") && (
          <>
            {!!visibleJobs.length && (
              <div className="provider-test-results" aria-label="测试记录">
                <div className="provider-test-result-head">
                  <span>供应商</span>
                  <span>测试</span>
                  <span>状态</span>
                  <span>耗时</span>
                  <span />
                </div>
                {visibleJobs.map((job) => (
                  <div
                    className={`provider-test-row${detailId === job.id ? " selected" : ""}`}
                    key={job.id}
                  >
                    <button
                      className="provider-test-job"
                      aria-label={`${job.providerName} ${job.kind === "fingerprint" ? "模型测试" : modes[job.mode]} ${statuses[job.status] ?? job.status}`}
                      onClick={() => setDetailId(job.id)}
                    >
                      <span>{job.providerName}</span>
                      <span>
                        {job.kind === "fingerprint"
                          ? "模型测试"
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
                <TestResult detail={currentDetail} now={now} />
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
                onChange={(e) =>
                  setDetection({ ...detection, enabled: e.target.checked })
                }
              />
            </label>
            <div className="provider-test-detection-config">
              <label className="provider-test-field">
                <span>检测模式</span>
                <select
                  value={detection.mode}
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
                  type="number"
                  min={5}
                  max={detection.mode === "smart" ? 30 : 1440}
                  value={
                    detection.mode === "smart"
                      ? detection.minIntervalMinutes
                      : detection.intervalMinutes
                  }
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
              <label className="provider-test-field provider-test-full">
                <span>全局检测模型</span>
                <input
                  value={detection.model}
                  onChange={(e) =>
                    setDetection({ ...detection, model: e.target.value })
                  }
                />
              </label>
            </div>
            <div className="provider-test-detection-list">
              <div className="provider-test-detection-head">
                <span>供应商</span>
                <span>参与</span>
                <span>覆盖模型</span>
                <span>检测状态</span>
              </div>
              {gateway.providers
                .filter((p) => !p.searchOnly)
                .map((p) => {
                  const override = detection.providers[p.id];
                  const status = state?.detectionStatus.find(
                    (s) => s.providerId === p.id,
                  );
                  const mark = state?.degraded[p.id];
                  return (
                    <div className="provider-test-provider" key={p.id}>
                      <strong>{p.name}</strong>
                      <label className="provider-test-provider-enabled">
                        <span>参与检测</span>
                        <input
                          type="checkbox"
                          aria-label={`${p.name} 参与检测`}
                          checked={override?.enabled ?? true}
                          onChange={(e) =>
                            setOverride(p.id, { enabled: e.target.checked })
                          }
                        />
                      </label>
                      <input
                        aria-label={`${p.name} 覆盖模型`}
                        placeholder={detection.model}
                        value={override?.model ?? ""}
                        onChange={(e) =>
                          setOverride(p.id, { model: e.target.value || null })
                        }
                      />
                      <div className="provider-test-provider-status">
                        {status ? (
                          <>
                            <span>
                              {statuses[status.status] ?? status.status}
                            </span>
                            <span>
                              最近一小时 {status.recentRequests} 次
                              {status.intervalMinutes
                                ? ` · ${status.intervalMinutes} 分钟`
                                : ""}
                            </span>
                            {status.nextAt != null && (
                              <span>下次 {date(status.nextAt)}</span>
                            )}
                            {status.cooldownUntil && (
                              <span>冷却至 {date(status.cooldownUntil)}</span>
                            )}
                            {status.error && (
                              <span
                                className="provider-test-error"
                                role="alert"
                              >
                                {status.error}
                              </span>
                            )}
                          </>
                        ) : (
                          <span>—</span>
                        )}
                      </div>
                      {mark && (
                        <div className="provider-test-mark" role="status">
                          <span>
                            降智：{mark.requestedModel} → {mark.predictedModel}
                            （{(mark.probability * 100).toFixed(1)}%）
                          </span>
                          <button
                            className="secondary compact"
                            disabled={busy}
                            onClick={() =>
                              void perform(async () => {
                                const current = await command(
                                  "provider_test_state",
                                );
                                await command("provider_test_clear_mark", {
                                  providerId: p.id,
                                  expectedRevision: current.revision,
                                });
                              })
                            }
                          >
                            清除标记
                          </button>
                          {mark.error && <span role="alert">{mark.error}</span>}
                          <span>重新入队请使用供应商队列开关。</span>
                        </div>
                      )}
                    </div>
                  );
                })}
            </div>
            <details className="provider-test-diagnostics">
              <summary>检测历史</summary>
              {state?.history.length ? (
                state.history.map((j) => (
                  <button
                    className="provider-test-history"
                    key={j.id}
                    onClick={() => {
                      setDetailId(j.id);
                      setTab("history");
                    }}
                  >
                    <time>{date(j.completedAt)}</time>
                    <span>{j.providerName}</span>
                    <span>{j.report?.prediction ?? statuses[j.status]}</span>
                  </button>
                ))
              ) : (
                <p className="provider-test-empty">暂无检测记录</p>
              )}
            </details>
          </>
        )}
        {tab === "bank" && state && (
          <>
            <dl className="provider-test-summary">
              <div>
                <dt>来源</dt>
                <dd>
                  {state.bank.source === "cache"
                    ? "已更新指纹库"
                    : "内置指纹库"}
                </dd>
              </div>
              <div>
                <dt>版本</dt>
                <dd>
                  {String(state.bank.version?.revision ?? "—").slice(0, 12)}
                </dd>
              </div>
              <div>
                <dt>最近检查</dt>
                <dd>{date(state.bank.checkedAt ?? null)}</dd>
              </div>
            </dl>
            {state.bank.error && (
              <p className="provider-test-error" role="alert">
                {state.bank.error}
              </p>
            )}
            <div className="provider-test-bank">
              <div className="provider-test-bank-head">
                <span>模型名称</span>
                <span>模型 ID</span>
              </div>
              {state.bank.models.map((m) => (
                <div key={m.id}>
                  <span>{m.name}</span>
                  <span>{m.id}</span>
                </div>
              ))}
            </div>
          </>
        )}
      </div>
      <footer className="provider-test-footer">
        {(error || state?.error) && (
          <div className="provider-test-error" role="alert">
            {error || state?.error}
          </div>
        )}
        {notice && <p role="status">{notice}</p>}
        <div className="provider-test-actions">
          {currentDetail?.job.canRetry &&
            (tab === "history" || tab === "fingerprint") && (
              <button
                className="secondary"
                disabled={busy}
                onClick={() => void retry()}
              >
                重试失败组
              </button>
            )}
          {testing &&
            (running.length ? (
              <button
                className="secondary"
                disabled={busy}
                onClick={() => void stop()}
              >
                {running.some((j) => j.batchId) ? "停止批次" : "停止测试"}
              </button>
            ) : (
              <button
                className="primary"
                disabled={!canStart}
                onClick={() => void start()}
              >
                {selectedIds.length > 1 ? "批量测试" : "开始测试"}
              </button>
            ))}
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
                disabled={busy}
                onClick={() =>
                  void perform(async () => {
                    await command("provider_test_set_detection", {
                      detection,
                      expectedRevision: detectionRevision,
                    });
                    await readDetection();
                    setNotice("检测设置已保存");
                  })
                }
              >
                保存检测设置
              </button>
            </>
          )}
          {tab === "bank" && (
            <button
              className="primary"
              disabled={busy || state?.bank.updating}
              onClick={() =>
                void perform(async () => {
                  await command("provider_test_update_bank");
                })
              }
            >
              {state?.bank.updating ? "更新中…" : "更新指纹库"}
            </button>
          )}
          {tab === "history" && !currentDetail?.job.canRetry && (
            <button className="secondary" onClick={close}>
              关闭
            </button>
          )}
        </div>
      </footer>
    </Modal>
  );
}

function TestResult({
  detail: { job, output },
  now,
}: {
  detail: ProviderTestDetail;
  now: number;
}) {
  return (
    <section className="provider-test-detail" aria-label="测试详情">
      <dl className="provider-test-summary">
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
            <div>
              <dt>预测模型</dt>
              <dd>{job.report.prediction}</dd>
            </div>
            <div>
              <dt>匹配度</dt>
              <dd>
                {(job.report.probability * 100).toFixed(2)}%
                <span className="provider-test-inline-meta">
                  有效组 {job.report.used_outputs}
                </span>
              </dd>
            </div>
          </>
        )}
      </dl>
      {job.completionReason && (
        <p className="provider-test-hint">
          {job.completionReason === "confidence99"
            ? "匹配度达到 99%，提前完成"
            : "首组命中降智规则，提前完成"}
        </p>
      )}
      {job.error && (
        <p className="provider-test-error" role="alert">
          {job.error}
        </p>
      )}
      {output?.text && (
        <pre className="provider-test-output">{output.text}</pre>
      )}
      {!!job.groups.length && (
        <div className="provider-test-groups">
          <div className="provider-test-group-head">
            <span>分组</span>
            <span>状态</span>
            <span>预测模型</span>
            <span>匹配度</span>
            <span>耗时</span>
          </div>
          {job.groups.map((group) => (
            <details key={group.index}>
              <summary>
                <span>第 {group.index + 1} 组</span>
                <span>{statuses[group.status] ?? group.status}</span>
                <span>{group.report?.prediction ?? "—"}</span>
                <span>
                  {group.report
                    ? `${(group.report.probability * 100).toFixed(2)}%`
                    : "—"}
                </span>
                <span>{(group.elapsedMs / 1000).toFixed(2)} s</span>
              </summary>
              <div className="provider-test-group-detail">
                <dl className="provider-test-summary">
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
                {group.error && (
                  <p className="provider-test-error" role="alert">
                    {group.error}
                  </p>
                )}
                {group.uncertain && (
                  <p className="provider-test-hint">
                    上游结果未知，未自动重放。
                  </p>
                )}
                <pre>{JSON.stringify(group.diagnostics, null, 2)}</pre>
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
