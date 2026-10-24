import { useCallback, useEffect, useRef, useState } from "react";
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

const modes: Record<ProviderTestMode, string> = {
  text: "普通",
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
  dispatching: "派发中",
  dispatched: "已派发",
  disabled: "已关闭",
  degraded: "已标记降智",
  outOfQueue: "已退出队列",
  idle: "无业务调用，已暂停",
  waiting: "等待下次检测",
};
const active = (job: ProviderTestJob) =>
  ["queued", "running"].includes(job.status);
const date = (at: number | null) =>
  at == null
    ? "—"
    : new Date(at).toLocaleString("zh-CN", {
        timeZone: "Asia/Taipei",
        hour12: false,
      });
export const scheduleTime = (at: number) =>
  new Date(at + 8 * 3600000).toISOString().slice(0, 19);
const defaults: Record<string, string> = {
  codex: "gpt-5.6-luna",
  claude: "claude-sonnet-4-5",
  grok: "grok-4.1-fast",
  dsh: "deepseek-v4-pro",
};
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
  const [state, setState] = useState<ProviderTestingState | null>(null);
  const [tab, setTab] = useState("test");
  const [kind, setKind] = useState(initialKind);
  const [mode, setMode] = useState<ProviderTestMode>(
    provider?.searchOnly ? "search" : "text",
  );
  const [selected, setSelected] = useState<string[]>(
    provider ? [provider.id] : [],
  );
  const [model, setModel] = useState(defaults[gateway.clientId]);
  const [models, setModels] = useState<ProviderTestModels | null>(null);
  const [catalogRefresh, setCatalogRefresh] = useState(0);
  const modeRef = useRef(mode);
  modeRef.current = mode;
  const [input, setInput] = useState("");
  const [media, setMedia] = useState<ProviderTestRequest["media"]>(null);
  const [concurrency, setConcurrency] = useState(1);
  const [at, setAt] = useState(
    scheduleTime(Math.ceil((Date.now() + 1000) / 60000) * 60000),
  );
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [detailId, setDetailId] = useState<string | null>(null);
  const [detail, setDetail] = useState<ProviderTestDetail | null>(null);
  const [detection, setDetection] = useState<ProviderDetection | null>(null);
  const [detectionRevision, setDetectionRevision] = useState("");
  const [now, setNow] = useState(Date.now());
  const fail = useCallback((e: unknown) => setError(errorOf(e).message), []);
  const refresh = useCallback(async () => {
    const value = await command("provider_test_state");
    setState(value);
  }, []);
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
      .catch(fail);
    return () => {
      disposed = true;
    };
  }, [detailId, state, fail]);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, []);
  const first = gateway.providers.find((p) => p.id === selected[0]);
  useEffect(() => {
    setModels(null);
    if (!first) return;
    let disposed = false;
    void command("get_gateway", { clientId: gateway.clientId })
      .then((current) =>
        command("provider_test_models", {
          clientId: gateway.clientId,
          providerId: first.id,
          expectedRevision: current.revision,
          fingerprint: kind === "fingerprint",
        }),
      )
      .then((result) => {
        if (!disposed) {
          setModels(result);
          setModel((current) =>
            ["text", "compact", "search"].includes(modeRef.current) &&
            current === defaults[gateway.clientId]
              ? result.defaultModel || current
              : current,
          );
        }
      })
      .catch((e) => {
        if (!disposed) fail(e);
      });
    return () => {
      disposed = true;
    };
  }, [first?.id, kind, gateway.clientId, catalogRefresh, fail]);
  const perform = async (work: () => Promise<void>) => {
    if (busy) return;
    setBusy(true);
    setError("");
    setNotice("");
    try {
      await work();
      await refresh();
    } catch (e) {
      fail(e);
    } finally {
      setBusy(false);
    }
  };
  const requests = async (
    scheduled = false,
  ): Promise<ProviderTestRequest[]> => {
    if (!selected.length) throw new Error("请选择供应商");
    const current = await command("get_gateway", {
      clientId: gateway.clientId,
    });
    return selected.map((providerId) => ({
      clientId: gateway.clientId,
      providerId,
      expectedRevision: current.revision,
      model,
      kind: scheduled ? "connection" : kind,
      mode: scheduled ? "text" : effectiveMode,
      input: scheduled ? "" : input,
      media: scheduled ? null : media,
      concurrency,
    }));
  };
  const start = () =>
    perform(async () => {
      const entries = await requests();
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
  const chooseKind = (value: ProviderTestKind) => {
    setKind(value);
    if (value === "fingerprint") {
      setMode("text");
      setSelected((ids) =>
        ids.filter(
          (id) => !gateway.providers.find((p) => p.id === id)?.searchOnly,
        ),
      );
    }
  };
  const searchOnly =
    selected.length > 0 &&
    selected.every(
      (id) => gateway.providers.find((p) => p.id === id)?.searchOnly,
    );
  const effectiveMode = searchOnly ? "search" : mode;
  const availableModes = searchOnly
    ? ["search"]
    : gateway.clientId === "grok"
      ? Object.keys(modes).filter((m) => m !== "compact")
      : gateway.clientId === "codex"
        ? ["text", "compact", "image", "search"]
        : ["text"];
  const jobs =
    state?.jobs.filter(
      (j) =>
        j.clientId === gateway.clientId &&
        (tab === "history" ||
          active(j) ||
          (j.completedAt != null && now - j.completedAt < 3000)),
    ) ?? [];
  const visibleDetail =
    detail &&
    (tab === "history" ||
      active(detail.job) ||
      detail.job.completedAt == null ||
      now - detail.job.completedAt < 3000);
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
  return (
    <Modal
      title={provider ? `${provider.name} 测试` : "供应商测试"}
      close={close}
    >
      <div className="provider-test-tabs" role="tablist" aria-label="测试管理">
        {[
          ["test", "连接与模型测试"],
          ["history", "测试结果"],
          ["schedule", "预约"],
          ...(gateway.clientId === "codex"
            ? [
                ["detection", "定时检测"],
                ["bank", "指纹库"],
              ]
            : []),
        ].map(([id, name]) => (
          <button
            key={id}
            role="tab"
            aria-selected={tab === id}
            onClick={() => setTab(id)}
          >
            {name}
          </button>
        ))}
      </div>
      {tab === "test" && (
        <>
          <fieldset className="provider-test-selection">
            <legend>供应商</legend>
            <label>
              <input
                type="checkbox"
                checked={
                  selected.length > 0 &&
                  selected.length ===
                    gateway.providers.filter(
                      (p) => kind !== "fingerprint" || !p.searchOnly,
                    ).length
                }
                onChange={(e) =>
                  setSelected(
                    e.target.checked
                      ? gateway.providers
                          .filter(
                            (p) => kind !== "fingerprint" || !p.searchOnly,
                          )
                          .map((p) => p.id)
                      : [],
                  )
                }
              />
              全选
            </label>
            {gateway.providers.map((p) => (
              <label key={p.id}>
                <input
                  type="checkbox"
                  disabled={kind === "fingerprint" && p.searchOnly}
                  checked={selected.includes(p.id)}
                  onChange={(e) =>
                    setSelected((ids) =>
                      e.target.checked
                        ? [...ids, p.id]
                        : ids.filter((id) => id !== p.id),
                    )
                  }
                />
                {p.name}
                {p.searchOnly ? " · 仅网络搜索" : ""}
              </label>
            ))}
          </fieldset>
          <div className="provider-test-fields">
            <label className="field">
              测试类型
              <select
                value={kind}
                onChange={(e) => chooseKind(e.target.value as ProviderTestKind)}
              >
                <option value="connection">连接测试</option>
                {gateway.clientId === "codex" && (
                  <option value="fingerprint">模型指纹测试</option>
                )}
              </select>
            </label>
            {kind === "connection" ? (
              <label className="field">
                接口
                <select
                  value={effectiveMode}
                  onChange={(e) => {
                    const next = e.target.value as ProviderTestMode;
                    setMode(next);
                    setMedia(null);
                    if (next === "image")
                      setModel(
                        gateway.clientId === "grok"
                          ? "grok-imagine-image-quality"
                          : "gpt-image-1",
                      );
                    else if (next === "video") setModel("grok-imagine-video");
                    else if (["tts", "stt", "realtime"].includes(next))
                      setModel(
                        next === "stt"
                          ? "grok-stt"
                          : next === "tts"
                            ? "grok-voice-tts"
                            : "grok-voice",
                      );
                    else
                      setModel(
                        models?.defaultModel || defaults[gateway.clientId],
                      );
                  }}
                >
                  {availableModes.map((m) => (
                    <option key={m} value={m}>
                      {modes[m as ProviderTestMode]}
                    </option>
                  ))}
                </select>
              </label>
            ) : (
              <label className="field">
                组并发
                <select
                  value={concurrency}
                  onChange={(e) => setConcurrency(Number(e.target.value))}
                >
                  {[1, 2, 3].map((n) => (
                    <option key={n}>{n}</option>
                  ))}
                </select>
              </label>
            )}
          </div>
          <label className="field">
            模型
            <input
              list="provider-test-models"
              value={model}
              onChange={(e) => setModel(e.target.value)}
            />
            <datalist id="provider-test-models">
              {models?.models.map((m) => (
                <option key={m.id} value={m.id}>
                  {m.name}
                </option>
              ))}
            </datalist>
          </label>
          <button
            className="secondary compact"
            disabled={!first}
            onClick={() => {
              setError("");
              setCatalogRefresh((n) => n + 1);
            }}
          >
            刷新模型目录
          </button>
          {models?.stale && <p role="status">{models.error}，暂用最近目录。</p>}
          {kind === "connection" && (
            <>
              <label className="field">
                测试输入
                <textarea
                  rows={2}
                  value={input}
                  maxLength={32768}
                  placeholder="留空使用默认测试内容"
                  onChange={(e) => setInput(e.target.value)}
                />
              </label>
              {["text", "image", "video", "stt"].includes(effectiveMode) && (
                <label className="field">
                  {effectiveMode === "stt" ? "音频素材" : "图像素材"}
                  <input
                    type="file"
                    accept={
                      effectiveMode === "stt"
                        ? "audio/*"
                        : "image/png,image/jpeg,image/webp,image/gif"
                    }
                    onChange={(e) => {
                      const file = e.target.files?.[0];
                      setMedia(null);
                      if (!file) return;
                      if (file.size > 20 * 1024 * 1024) {
                        setError("素材不能超过 20 MiB");
                        return;
                      }
                      const reader = new FileReader();
                      reader.onload = () =>
                        setMedia({
                          dataUrl: String(reader.result),
                          mime: file.type,
                        });
                      reader.onerror = () => setError("素材读取失败");
                      reader.readAsDataURL(file);
                    }}
                  />
                </label>
              )}
              {media && (
                <button
                  className="secondary compact"
                  onClick={() => setMedia(null)}
                >
                  移除素材
                </button>
              )}
            </>
          )}
          <div className="modal-actions">
            <button
              className="primary"
              disabled={busy || !selected.length || !model}
              onClick={() => void start()}
            >
              {selected.length > 1 ? "批量测试" : "开始测试"}
            </button>
            <button
              className="secondary"
              disabled={!selected.length}
              onClick={() => setTab("schedule")}
            >
              预约普通测试
            </button>
          </div>
        </>
      )}
      {tab === "schedule" && (
        <>
          <p>
            已选 {selected.length} 个供应商，模型 {model}。
          </p>
          <label className="field">
            预约时间（UTC+08:00）
            <input
              type="datetime-local"
              step={1}
              value={at}
              onChange={(e) => setAt(e.target.value)}
            />
          </label>
          <div className="modal-actions">
            <button
              className="secondary"
              disabled={busy || !selected.length}
              onClick={() =>
                void perform(async () =>
                  setAt(
                    scheduleTime(
                      await command("provider_test_next_cycle", {
                        clientId: gateway.clientId,
                        providers: selected,
                      }),
                    ),
                  ),
                )
              }
            >
              最近周期 +3 秒
            </button>
            <button
              className="primary"
              disabled={busy || !selected.length}
              onClick={() =>
                void perform(async () => {
                  await command("provider_test_schedule", {
                    requests: await requests(true),
                    at: Date.parse(`${at}+08:00`),
                  });
                  setNotice("预约已保存");
                })
              }
            >
              保存预约
            </button>
          </div>
          {state?.schedules
            .filter((s) =>
              s.providers.some((p) => p.clientId === gateway.clientId),
            )
            .map((s) => (
              <div className="provider-test-row" key={s.id}>
                <span>
                  {date(s.at)} · {s.providers.length} 项 ·{" "}
                  {statuses[s.status] ?? s.status}
                </span>
                {s.status === "pending" && (
                  <button
                    onClick={() =>
                      void perform(() =>
                        command("provider_test_cancel_schedule", { id: s.id }),
                      )
                    }
                  >
                    取消预约
                  </button>
                )}
                {s.error && <p role="alert">{s.error}</p>}
              </div>
            ))}
        </>
      )}
      {tab === "detection" && detection && (
        <>
          <label className="setting-row">
            <span>启用定时检测</span>
            <input
              type="checkbox"
              checked={detection.enabled}
              onChange={(e) =>
                setDetection({ ...detection, enabled: e.target.checked })
              }
            />
          </label>
          <div className="provider-test-fields">
            <label className="field">
              检测模式
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
            <label className="field">
              {detection.mode === "smart"
                ? "最小间隔（分钟）"
                : "检测周期（分钟）"}
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
          </div>
          <label className="field">
            全局检测模型
            <input
              value={detection.model}
              onChange={(e) =>
                setDetection({ ...detection, model: e.target.value })
              }
            />
          </label>
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
                  <label className="setting-row">
                    <span>参与检测</span>
                    <input
                      type="checkbox"
                      checked={override?.enabled ?? true}
                      onChange={(e) =>
                        setOverride(p.id, { enabled: e.target.checked })
                      }
                    />
                  </label>
                  <label className="field">
                    覆盖模型
                    <input
                      placeholder={detection.model}
                      value={override?.model ?? ""}
                      onChange={(e) =>
                        setOverride(p.id, { model: e.target.value || null })
                      }
                    />
                  </label>
                  {status && (
                    <p>
                      {statuses[status.status] ?? status.status} · 最近一小时{" "}
                      {status.recentRequests} 次
                      {status.intervalMinutes
                        ? ` · ${status.intervalMinutes} 分钟`
                        : ""}{" "}
                      · 下次 {date(status.nextAt)}
                      {status.cooldownUntil
                        ? ` · 冷却至 ${date(status.cooldownUntil)}`
                        : ""}
                      {status.error && <span role="alert">{status.error}</span>}
                    </p>
                  )}
                  {mark && (
                    <div role="status">
                      降智：{mark.requestedModel} → {mark.predictedModel}（
                      {(mark.probability * 100).toFixed(1)}%）
                      {mark.error && <p role="alert">{mark.error}</p>}
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
                      <span>重新入队请使用供应商队列开关。</span>
                    </div>
                  )}
                </div>
              );
            })}
          <div className="modal-actions">
            <button
              className="primary"
              disabled={busy}
              onClick={() =>
                void perform(async () => {
                  await command("provider_test_set_detection", {
                    detection,
                    expectedRevision: detectionRevision,
                  });
                  setDetection(null);
                  setNotice("检测设置已保存");
                })
              }
            >
              保存检测设置
            </button>
            <button
              className="secondary"
              onClick={() => {
                setDetection(null);
                void refresh().catch(fail);
              }}
            >
              重新读取
            </button>
          </div>
          <details>
            <summary>检测历史</summary>
            {state?.history.map((j) => (
              <button
                className="provider-test-row"
                key={j.id}
                onClick={() => {
                  setDetailId(j.id);
                  setTab("history");
                }}
              >
                {date(j.completedAt)} · {j.providerName} ·{" "}
                {j.report?.prediction ?? statuses[j.status]}
              </button>
            ))}
          </details>
        </>
      )}
      {tab === "bank" && state && (
        <>
          <p>
            {state.bank.source === "cache" ? "已更新指纹库" : "内置指纹库"} ·{" "}
            {String(state.bank.version?.revision ?? "—").slice(0, 12)}
          </p>
          <p>最近检查 {date(state.bank.checkedAt ?? null)}</p>
          {state.bank.error && <p role="alert">{state.bank.error}</p>}
          <button
            className="secondary"
            disabled={busy || state.bank.updating}
            onClick={() =>
              void perform(async () => {
                await command("provider_test_update_bank");
              })
            }
          >
            {state.bank.updating ? "更新中…" : "更新指纹库"}
          </button>
          <ul>
            {state.bank.models.map((m) => (
              <li key={m.id}>
                {m.name} · {m.id}
              </li>
            ))}
          </ul>
        </>
      )}
      {["test", "history"].includes(tab) && (
        <>
          <div className="provider-test-results">
            {jobs.map((job) => (
              <div className="provider-test-row" key={job.id}>
                <button onClick={() => setDetailId(job.id)}>
                  {job.providerName} ·{" "}
                  {job.kind === "fingerprint" ? "模型指纹" : modes[job.mode]} ·{" "}
                  {statuses[job.status]}
                  {job.report
                    ? ` · ${job.report.prediction} ${(job.report.probability * 100).toFixed(1)}%`
                    : ""}
                </button>
                {active(job) && (
                  <button
                    onClick={() =>
                      void perform(() =>
                        command("provider_test_cancel", { id: job.id }),
                      )
                    }
                  >
                    停止
                  </button>
                )}
                {active(job) && job.batchId && (
                  <button
                    onClick={() =>
                      void perform(() =>
                        command("provider_test_stop_batch", {
                          id: job.batchId!,
                        }),
                      )
                    }
                  >
                    停止批次
                  </button>
                )}
              </div>
            ))}
          </div>
          {visibleDetail && detail && (
            <TestResult
              detail={detail}
              now={now}
              retry={() =>
                void perform(async () => {
                  await command("provider_test_retry", { id: detail.job.id });
                })
              }
              busy={busy}
            />
          )}
        </>
      )}
      {(error || state?.error) && (
        <div className="form-error" role="alert">
          {error || state?.error}
        </div>
      )}
      {notice && <p role="status">{notice}</p>}
    </Modal>
  );
}
function TestResult({
  detail: { job, output },
  now,
  retry,
  busy,
}: {
  detail: ProviderTestDetail;
  now: number;
  retry: () => void;
  busy: boolean;
}) {
  return (
    <section className="provider-test-detail" aria-label="测试详情">
      <dl className="import-preview">
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
            {(
              (active(job) && job.startedAt
                ? Math.max(0, now - job.startedAt)
                : job.elapsedMs) / 1000
            ).toFixed(2)}
            s · 首字节{" "}
            {job.firstByteMs == null
              ? "—"
              : `${(job.firstByteMs / 1000).toFixed(2)}s`}
          </dd>
        </div>
      </dl>
      {job.report && (
        <p>
          预测模型：{job.report.prediction} · 匹配度{" "}
          {(job.report.probability * 100).toFixed(2)}% · 有效组{" "}
          {job.report.used_outputs}
        </p>
      )}
      {job.completionReason && (
        <p>
          {job.completionReason === "confidence99"
            ? "匹配度达到 99%，提前完成"
            : "首组命中降智规则，提前完成"}
        </p>
      )}
      {job.error && <p role="alert">{job.error}</p>}
      {job.canRetry && (
        <button className="secondary" disabled={busy} onClick={retry}>
          重试失败组
        </button>
      )}
      {output?.text && <pre>{output.text}</pre>}
      {output?.media.map((media, index) =>
        media.kind === "image" ? (
          <img key={index} src={media.dataUrl} alt="测试返回图像" />
        ) : media.kind === "video" ? (
          <video key={index} src={media.dataUrl} controls />
        ) : (
          <audio key={index} src={media.dataUrl} controls />
        ),
      )}
      {job.groups.map((group) => (
        <details key={group.index}>
          <summary>
            第 {group.index + 1} 组 · {statuses[group.status] ?? group.status} ·{" "}
            {(group.elapsedMs / 1000).toFixed(2)}s
            {group.report
              ? ` · ${group.report.prediction} ${(group.report.probability * 100).toFixed(2)}%`
              : ""}
          </summary>
          <p>
            目标 {group.expected} 个整数 · 已派发 {group.attempts} 次 · 返回模型{" "}
            {group.returnedModel ?? "—"}
          </p>
          {group.error && <p role="alert">{group.error}</p>}
          {group.uncertain && <p>上游结果未知，未自动重放。</p>}
          <pre>{JSON.stringify(group.diagnostics, null, 2)}</pre>
          {output?.groups[String(group.index)] && (
            <pre>{output.groups[String(group.index)]}</pre>
          )}
        </details>
      ))}
      {!output && <p>本次运行的原始输出和媒体已释放，仅保留摘要。</p>}
    </section>
  );
}
