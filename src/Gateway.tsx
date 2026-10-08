import ClaudeProfile from "./ClaudeProfile";
import ClientSelection, {
  useClientSelection,
  clientName,
} from "./ClientSelection";
import type { ClientId } from "./types";
import { saveGatewayEdit, type EditRevision } from "./gateway-edit";
import { confirmAction } from "./confirmation";
import ProviderSettings from "./ProviderSettings";
import ProviderControls from "./ProviderControls";
import ProviderNameEditor from "./ProviderNameEditor";
import SortableProviders, { type ProviderCommit } from "./SortableProviders";
import Modal from "./Modal";
import { providerStatus } from "./provider-status";
import ModelPolicyDialog from "./ModelPolicyDialog";
import { QuotaInfo, useProviderQuota } from "./Quota";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Plus,
  Power,
  Download,
  Pencil,
  Trash2,
  Network,
  Settings2,
  MoreHorizontal,
  Check,
  RotateCcw,
  LoaderCircle,
} from "lucide-react";
import { command, subscribe } from "./bridge";
import {
  errorOf,
  type GatewayState,
  type GatewaySettings,
  type Provider,
} from "./types";

type Edit = Record<string, unknown>;
type Dialog =
  | { kind: "provider"; item?: Provider }
  | { kind: "models"; item: Provider }
  | { kind: "settings"; item: Provider }
  | { kind: "quota"; item: Provider }
  | null;
type GatewayProps = {
  quotaRefreshSeconds?: number;
  notify: (s: string) => void;
  onDirtyChange?: (dirty: boolean) => void;
  onFocusHandled?: () => void;
  focusProvider?: { id: string; sequence: number; clientId?: ClientId } | null;
};
export default function Gateway(props: GatewayProps) {
  const [clientId, select] = useClientSelection("main");
  const dirty = useRef(false);
  const changed = useCallback(
    (value: boolean) => {
      dirty.current = value;
      props.onDirtyChange?.(value);
    },
    [props.onDirtyChange],
  );
  useEffect(() => {
    if (props.focusProvider)
      select(props.focusProvider.clientId ?? "codex", dirty.current);
  }, [props.focusProvider?.sequence]);
  return (
    <GatewayContent
      key={clientId}
      {...props}
      clientId={clientId}
      onClientChange={(id) => {
        select(id, dirty.current);
      }}
      onDirtyChange={changed}
    />
  );
}
function GatewayContent({
  quotaRefreshSeconds = 60,
  clientId,
  onClientChange,
  notify,
  onDirtyChange,
  onFocusHandled,
  focusProvider,
}: {
  quotaRefreshSeconds?: number;
  clientId: ClientId;
  onClientChange: (id: ClientId) => void;
  notify: (s: string) => void;
  onDirtyChange?: (dirty: boolean) => void;
  onFocusHandled?: () => void;
  focusProvider?: { id: string; sequence: number } | null;
}) {
  const [state, setState] = useState<GatewayState | null>(null);
  const [error, setError] = useState("");
  const [busy, setBusy] = useState(false);
  const [dialog, setDialog] = useState<Dialog>(null);
  const [renameRequest, setRenameRequest] = useState<{
    id: string;
    sequence: number;
  } | null>(null);
  const quota = useProviderQuota(
    state?.providers ?? [],
    true,
    "app-visibility",
    clientId,
    quotaRefreshSeconds,
  );
  useEffect(() => {
    let disposed = false,
      clean: (() => void) | undefined;
    void command<GatewayState>("get_gateway", { clientId })
      .then((s) => {
        if (!disposed && (s.clientId ?? "codex") === clientId) setState(s);
      })
      .catch((e) => setError(errorOf(e).message));
    void subscribe<GatewayState>("gateway-state", (s) => {
      if (!disposed && (s.clientId ?? "codex") === clientId) setState(s);
    }).then((c) => {
      if (disposed) c();
      else clean = c;
    });
    return () => {
      disposed = true;
      clean?.();
    };
  }, []);
  const focusHandled = useRef<number | null>(null);
  const [dialogVersion, setDialogVersion] = useState(0);
  useEffect(() => {
    if (
      focusProvider &&
      state &&
      focusHandled.current !== focusProvider.sequence
    ) {
      const p = state.providers.find((p) => p.id === focusProvider.id);
      focusHandled.current = focusProvider.sequence;
      if (p) {
        setDialogVersion(focusProvider.sequence);
        setDialog({ kind: "settings", item: p });
      }
      onFocusHandled?.();
    }
  }, [focusProvider, onFocusHandled, state]);
  const action = async (fn: () => Promise<void>, surfaceError = true) => {
    setBusy(true);
    setError("");
    try {
      await fn();
    } catch (e) {
      if (surfaceError) setError(errorOf(e).message);
      throw e;
    } finally {
      setBusy(false);
    }
  };
  const edit = async (payload: Edit, expected?: EditRevision) => {
    if (!state) return;
    await saveGatewayEdit(clientId, state, payload, expected, setState);
  };
  const run = (fn: () => Promise<void>) => {
    void action(fn).catch(() => {});
  };
  const providerEdit: ProviderCommit = (payload, expected) =>
    action(() => edit(payload, expected), false);
  if (!state)
    return (
      <div className="empty" role="status">
        {error || "正在读取网关…"}
      </div>
    );
  const toggle = () =>
    run(async () => {
      setState(
        await command<GatewayState>(
          state.running || state.recoveryPending
            ? "stop_gateway"
            : "start_gateway",
          {
            clientId,
            expectedRevision: state.revision,
            expectedConfigRevision: state.configRevision,
          },
        ),
      );
      notify(`配置已切换，请重新打开 ${clientName(clientId)}`);
    });
  const choose = (p: Provider) =>
    run(async () => {
      await edit({ op: "select", id: p.id });
      notify(
        state.running
          ? "已切换供应商，新请求立即生效"
          : `文件已切换，请重新打开 ${clientName(clientId)}`,
      );
    });
  const resetProvider = (p: Provider) =>
    run(async () => {
      await edit({ op: "reset", id: p.id });
      notify("已重置熔断");
    });
  const renameProvider = (
    payload: { op: "renameProvider"; id: string; name: string },
    revision: EditRevision,
  ) => edit(payload, revision);
  const menuClose = (e: React.MouseEvent) =>
    e.currentTarget.closest("details")?.removeAttribute("open");
  return (
    <section className="gateway-page">
      {clientId==="claude"&&<ClaudeProfile disabled={busy||!!dialog} notify={notify}/>}
      <div className="page-heading gateway-heading">
        <ClientSelection
          client={clientId}
          select={onClientChange}
          disabled={busy}
        />
        <div className="gateway-toolbar">
          <div className="segmented" aria-label="路由模式">
            <button
              disabled={busy}
              aria-pressed={state.mode === "manual"}
              onClick={() => run(() => edit({ op: "mode", mode: "manual" }))}
            >
              手动
            </button>
            <button
              disabled={busy}
              aria-pressed={state.mode === "auto"}
              onClick={() => run(() => edit({ op: "mode", mode: "auto" }))}
            >
              自动
            </button>
          </div>
          <button
            className={state.running ? "secondary" : "primary"}
            disabled={
              busy ||
              (!state.running &&
                !state.recoveryPending &&
                !state.providers.length)
            }
            onClick={toggle}
          >
            <Power size={15} />
            {state.running
              ? "停用"
              : state.recoveryPending
                ? "处理事务"
                : "启用"}
          </button>
          <button
            className="secondary"
            disabled={busy}
            onClick={() => setDialog({ kind: "provider" })}
          >
            <Plus size={15} />
            添加
          </button>
          <details className="row-menu">
            <summary aria-label="网关操作">
              <MoreHorizontal size={17} />
            </summary>
            <div className="menu-popover" onClick={menuClose}>
              <button
                onClick={() => quota.refreshAll()}
                disabled={!state.providers.length}
              >
                刷新全部额度
              </button>
              <button
                disabled={busy}
                onClick={() =>
                  run(async () => {
                    await edit({ op: "import" });
                    notify("已导入当前供应商");
                  })
                }
              >
                <Download size={14} />
                从配置导入
              </button>
            </div>
          </details>
        </div>
      </div>
      {state.configWarning && (
        <div className="inline-error" role="status">
          {state.configWarning}
        </div>
      )}
      {(error || state.error || state.configError) && (
        <div className="banner error" role="alert">
          {error || state.error || state.configError}
        </div>
      )}
      <>
        {state.waitingRequests > 0 && (
          <div className="gateway-waiting">等待 {state.waitingRequests}</div>
        )}
        <div className="provider-list">
          {!state.providers.length && (
            <div className="empty">
              <Network size={24} />
              <strong>尚未添加供应商</strong>
            </div>
          )}
          <SortableProviders
            providers={state.providers}
            revision={state.revision}
            disabled={busy}
            commit={providerEdit}
            report={setError}
            rowClass={(p) =>
              `provider-row ${state.mode === "manual" && state.selected === p.id ? "selected" : ""}`
            }
          >
            {(p, priority, handle, rowBusy) => {
              const selected =
                state.mode === "manual" && state.selected === p.id;
              const status = providerStatus(p);
              return (
                <>
                  <div className="provider-main">
                    {handle}
                    {priority && <span className="priority">P{priority}</span>}
                    <ProviderNameEditor
                      provider={p}
                      revision={state.revision}
                      disabled={rowBusy}
                      commit={renameProvider}
                      report={setError}
                      request={
                        renameRequest?.id === p.id
                          ? renameRequest.sequence
                          : undefined
                      }
                    />
                    {status && (
                      <button
                        className="provider-alert"
                        onClick={() => setDialog({ kind: "settings", item: p })}
                      >
                        {status}
                      </button>
                    )}
                    {state.configProvider === p.id && !selected && (
                      <span className="provider-binding">配置使用</span>
                    )}
                    {state.mode === "auto" && state.lastSuccessful === p.id && (
                      <span className="provider-binding">最近使用</span>
                    )}
                    {clientId === "codex" && !p.supportsWebsocket && (
                      <span className="provider-transport">HTTP 桥接</span>
                    )}
                    <button
                      className="secondary compact"
                      disabled={rowBusy}
                      onClick={() => choose(p)}
                    >
                      {selected ? (
                        <>
                          <Check size={14} />
                          已选择
                        </>
                      ) : (
                        "选择"
                      )}
                    </button>
                    <button
                      type="button"
                      className="icon-button compact provider-reset"
                      aria-label={`${p.name} 重置熔断`}
                      title="重置熔断"
                      disabled={rowBusy}
                      onClick={() => resetProvider(p)}
                    >
                      <RotateCcw size={14} />
                    </button>
                    <details className="row-menu">
                      <summary
                        aria-label={`${p.name} 操作`}
                        aria-disabled={rowBusy}
                        onClick={(e) => {
                          if (rowBusy) e.preventDefault();
                        }}
                      >
                        <MoreHorizontal size={17} />
                      </summary>
                      <div className="menu-popover" onClick={menuClose}>
                        <button
                          onClick={() =>
                            setDialog({ kind: "settings", item: p })
                          }
                        >
                          <Settings2 size={14} />
                          供应商设置
                        </button>
                        <button
                          onClick={() =>
                            setDialog({ kind: "provider", item: p })
                          }
                        >
                          <Pencil size={14} />
                          编辑 API
                        </button>
                        <button
                          onClick={() =>
                            setRenameRequest({
                              id: p.id,
                              sequence: Date.now(),
                            })
                          }
                        >
                          <Pencil size={14} />
                          重命名
                        </button>
                        <button
                          className="danger"
                          disabled={busy}
                          onClick={async () => {
                            if (
                              await confirmAction(
                                `删除供应商“${p.name}”？`,
                                "删除",
                              )
                            )
                              run(() =>
                                edit({ op: "deleteProvider", id: p.id }),
                              );
                          }}
                        >
                          <Trash2 size={14} />
                          删除
                        </button>
                      </div>
                    </details>
                  </div>
                  <div className="provider-secondary">
                    <span className="provider-domain" title={p.baseUrl}>
                      {new URL(p.baseUrl).host}
                    </span>
                    <QuotaInfo
                      compact
                      provider={p}
                      quota={quota.quotaFor(p)}
                      refresh={() => void quota.refresh(p.id)}
                      details={() => setDialog({ kind: "quota", item: p })}
                    />
                    <ProviderControls
                      provider={p}
                      revision={state.revision}
                      disabled={rowBusy}
                      commit={providerEdit}
                      report={setError}
                    />
                  </div>
                </>
              );
            }}
          </SortableProviders>
        </div>
        <details className="advanced">
          <summary>高级设置</summary>
          <div className="gateway-diagnostics">
            <code>{state.address}</code>
            <span>{state.running ? "运行中" : "已关闭"}</span>
            <span>活动连接 {state.activeConnections}</span>
            <span>
              配置使用{" "}
              {state.configState === "gateway"
                ? "本地网关"
                : (state.providers.find((p) => p.id === state.configProvider)
                    ?.name ?? "未匹配供应商")}
            </span>
          </div>
          <Advanced
            clientId={clientId}
            onDirtyChange={onDirtyChange}
            settings={state.settings}
            revision={state.revision}
            busy={busy}
            save={async (settings, revision) => {
              await action(() => edit({ op: "settings", settings }, revision));
              notify("网关参数已保存");
            }}
          />
        </details>
      </>
      {dialog?.kind === "settings" && (
        <ProviderSettings
          key={`${dialog.item.id}:${dialogVersion}`}
          provider={dialog.item}
          onDirtyChange={onDirtyChange}
          clientId={clientId}
          revision={state.revision}
          disabled={busy}
          runtime={
            state.providers.find((p) => p.id === dialog.item.id) ?? dialog.item
          }
          close={() => setDialog(null)}
          models={() => setDialog({ kind: "models", item: dialog.item })}
          save={async (payload, revision) => {
            await edit(payload, revision);
            notify(
              payload.supportsWebsocket
                ? "已启用原生 WebSocket"
                : "已启用 HTTP 桥接",
            );
          }}
        />
      )}
      {dialog?.kind === "quota" && (
        <Modal title={`${dialog.item.name} 额度`} close={() => setDialog(null)}>
          <QuotaInfo
            provider={dialog.item}
            quota={quota.quotaFor(
              state.providers.find((p) => p.id === dialog.item.id) ??
                dialog.item,
            )}
            refresh={() => void quota.refresh(dialog.item.id)}
          />
        </Modal>
      )}
      {dialog?.kind === "models" && (
        <ModelPolicyDialog
          clientId={clientId}
          provider={dialog.item}
          revision={state.revision}
          version={
            state.providers.find((p) => p.id === dialog.item.id)
              ?.quotaVersion ?? "deleted"
          }
          onDirtyChange={onDirtyChange}
          close={() => setDialog(null)}
          save={async (allowedModels, revision) => {
            await edit({
              op: "modelsProvider",
              id: dialog.item.id,
              allowedModels,
            }, revision);
            setDialog(null);
          }}
        />
      )}
      {dialog?.kind === "provider" && (
        <GatewayDialog
          clientId={clientId}
          dialog={dialog}
          revision={state.revision}
          onDirtyChange={onDirtyChange}
          close={() => setDialog(null)}
          save={async (payload, revision) => {
            await edit(payload, revision);
            setDialog(null);
          }}
        />
      )}
    </section>
  );
}
function Advanced({
  clientId,
  onDirtyChange,
  settings,
  revision,
  busy,
  save,
}: {
  clientId: ClientId;
  onDirtyChange?: (dirty: boolean) => void;
  settings: GatewaySettings;
  revision: string;
  busy: boolean;
  save: (s: GatewaySettings, revision: EditRevision) => Promise<void>;
}) {
  const [draft, setDraft] = useState(settings),
    [error, setError] = useState("");
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const [editing, setEditing] = useState(false);
  const baseRevision = useRef<EditRevision>(revision);
  useEffect(() => {
    if (!editing) {
      setDraft(settings);
      baseRevision.current = revision;
    }
  }, [settings, revision, editing]);
  const fields: [keyof GatewaySettings, string][] = [
    ["port", "本地端口"],
    ["maxRetries", "最大重试次数"],
    ["failureThreshold", "连续失败阈值"],
    ["successThreshold", "恢复成功次数"],
    ["cooldownSeconds", "熔断等待 / 秒"],
    ["rateLimitSeconds", "429 默认冷却 / 秒"],
    ...(clientId === "codex"
      ? [
          ["capacityRetrySeconds", "容量错误等待 / 秒"] as [
            keyof GatewaySettings,
            string,
          ],
        ]
      : []),
    ["errorRate", "错误率阈值 (0–1)"],
    ["minRequests", "最小请求数"],
    ["firstByteSeconds", "首字节 / 秒"],
    ["idleSeconds", "流静默 / 秒"],
    ["totalSeconds", "非流式总时限 / 秒"],
    ["connectSeconds", "连接超时 / 秒"],
    ["queueSeconds", "排队等待 / 秒"],
    ["maxWaiting", "最多等待请求"],
  ];
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        setError("");
        void save(draft, baseRevision.current)
          .then(() => {
            setEditing(false);
            onDirtyChange?.(false);
          })
          .catch((e) => {
            baseRevision.current = null;
            setError(errorOf(e).message);
          });
      }}
    >
      <div className="settings-grid">
        {fields.map(([key, label]) => (
          <label key={key}>
            {label}
            <input
              type="number"
              required
              min={key === "maxRetries" ? 0 : key === "errorRate" ? 0.01 : 1}
              max={key === "capacityRetrySeconds" ? 86400 : undefined}
              step={key === "errorRate" ? 0.01 : 1}
              value={draft[key]}
              onChange={(e) => {
                if (!editing) baseRevision.current = revision;
                setEditing(true);
                onDirtyChange?.(true);
                setDraft({ ...draft, [key]: Number(e.target.value) });
              }}
            />
          </label>
        ))}
      </div>
      {error && (
        <div role="alert" className="form-error">
          {error}
        </div>
      )}
      <button disabled={busy} className="secondary">
        {baseRevision.current === null ? "重试" : "保存参数"}
      </button>
    </form>
  );
}
function GatewayDialog({
  clientId,
  dialog,
  revision,
  close,
  save,
  onDirtyChange,
}: {
  clientId: ClientId;
  dialog: Extract<NonNullable<Dialog>, { kind: "provider" }>;
  close: () => void;
  revision: string;
  save: (p: Edit, revision: EditRevision) => Promise<void>;
  onDirtyChange?: (dirty: boolean) => void;
}) {
  const baseline = useRef<EditRevision>(revision);
  const [busy, setBusy] = useState(false),
    [error, setError] = useState("");
  const [name, setName] = useState(dialog.item ? dialog.item.name : ""),
    [baseUrl, setBaseUrl] = useState(dialog.item?.baseUrl ?? ""),
    [token, setToken] = useState("");
  const initialDraft = useRef(JSON.stringify([name, baseUrl, token]));
  useEffect(() => {
    onDirtyChange?.(JSON.stringify([name, baseUrl, token]) !== initialDraft.current);
  }, [name, baseUrl, token, onDirtyChange]);
  useEffect(() => () => onDirtyChange?.(false), [onDirtyChange]);
  const title = `${dialog.item ? "编辑" : "添加"}供应商`;
  return (
    <Modal title={title} close={close} busy={busy}>
      <form
        className="gateway-form"
        onSubmit={(e) => {
          e.preventDefault();
          setError("");
          setBusy(true);
          const payload: Edit = {
            op: "saveProvider",
            id: dialog.item?.id ?? null,
            baseUrl,
            token,
          };
          if (!dialog.item) payload.name = name;
          void save(payload, baseline.current)
            .catch((e) => {
              baseline.current = null;
              setError(errorOf(e).message);
            })
            .finally(() => setBusy(false));
        }}
      >
        <>
          {!dialog.item && (
            <label>
              名称
              <input
                type="text"
                maxLength={120}
                value={name}
                onChange={(e) => setName(e.target.value)}
                placeholder="可选"
              />
            </label>
          )}
          <label>
            {clientId === "claude" ? "ANTHROPIC_BASE_URL" : "base_url"}
            <input
              required
              type="url"
              autoFocus
              value={baseUrl}
              onChange={(e) => setBaseUrl(e.target.value)}
              placeholder={
                clientId === "claude"
                  ? "https://api.example.com"
                  : "https://api.example.com/v1"
              }
            />
          </label>
          <label>
            {clientId === "claude"
              ? "ANTHROPIC_AUTH_TOKEN"
              : "experimental_bearer_token"}
            <input
              required={!dialog.item}
              type="password"
              autoComplete="new-password"
              value={token}
              onChange={(e) => setToken(e.target.value)}
              placeholder={dialog.item ? "留空保留当前 Token" : "输入 Token"}
            />
          </label>
        </>
        {error && (
          <div className="banner error" role="alert">
            {error}
          </div>
        )}
        <div className="form-actions">
          <button
            type="button"
            className="secondary"
            disabled={busy}
            onClick={close}
          >
            取消
          </button>
          <button className="primary" disabled={busy}>
            {busy && <LoaderCircle size={15} className="spin" />}
            {baseline.current === null ? "重试" : "保存"}
          </button>
        </div>
      </form>
    </Modal>
  );
}
