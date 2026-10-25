import ProviderTesting from "./ProviderTesting";
import ConnectionTestIcon from "./ConnectionTestIcon";
import Advanced from "./GatewayAdvanced";
import DshProfile, { connectDsh } from "./DshProfile";
import { preferredDshProfile } from "./dsh-profile";
import CurrentConnection from "./CurrentConnection";
import GatewayModels from "./GatewayModels";
import GrokImport from "./GrokImport";
import GatewayDialog from "./GatewayProviderDialog";
import { subscriptions } from "./subscriptions";
import type { GatewayEdit, ProviderAction } from "./bridge-contracts";
import QuotaRefreshSettings from "./QuotaRefreshSettings";
import ClaudeProfile from "./ClaudeProfile";
import ClientSelection, {
  useClientSelection,
  clientName,
} from "./ClientSelection";
import type { ClientId } from "./types";
import { saveGatewayEdit, type EditRevision } from "./gateway-edit";
import { confirmAction } from "./confirmation";
import ProviderSettings from "./ProviderSettings";
import ProviderExchange from "./ProviderExchange";
import ProviderEndpoints from "./ProviderEndpoints";
import ProviderControls from "./ProviderControls";
import ProviderNameEditor from "./ProviderNameEditor";
import SortableProviders, { type ProviderCommit } from "./SortableProviders";
import Modal from "./Modal";
import {
  providerRuntimeStatus,
  providerHandoffStatus,
  providerFault,
} from "./provider-status";
import { QuotaInfo, useProviderQuota } from "./Quota";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  Plus,
  Power,
  FileInput,
  FileOutput,
  Pencil,
  Trash2,
  Network,
  Settings2,
  MoreHorizontal,
  Check,
  RotateCcw,
} from "lucide-react";
import { command } from "./bridge";
import {
  errorOf,
  type GatewayState,
  type GatewaySettings,
  type Provider,
} from "./types";

type Edit = GatewayEdit;
type Dialog =
  | {
      kind: "testing";
      item: Provider;
    }
  | { kind: "grokImport" }
  | { kind: "dsh" }
  | { kind: "exchange"; mode: "import" | "export" }
  | { kind: "provider"; item?: Provider }
  | { kind: "settings"; item: Provider }
  | { kind: "endpoints"; item: Provider }
  | { kind: "quota"; item: Provider }
  | null;
type GatewayProps = {
  quotaRefreshSeconds?: number;
  notify: (s: string) => void;
  onDirtyChange?: (dirty: boolean) => void;
  onFocusHandled?: () => void;
  focusProvider?: {
    id: string;
    sequence: number;
    clientId?: ClientId;
    action?: ProviderAction;
  } | null;
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
      // App has already confirmed discarding drafts for explicit navigation.
      select(props.focusProvider.clientId ?? "codex");
  }, [props.focusProvider?.sequence]);
  return (
    <GatewayContent
      key={clientId}
      {...props}
      clientId={clientId}
      focusProvider={
        props.focusProvider &&
        (props.focusProvider.clientId ?? "codex") === clientId
          ? props.focusProvider
          : null
      }
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
  focusProvider?: {
    id: string;
    sequence: number;
    action?: ProviderAction;
  } | null;
}) {
  const [modelsOpen, setModelsOpen] = useState(false);
  const dirtyParts = useRef({ quota: false, advanced: false, dialog: false });
  const markQuota = useCallback(
    (value: boolean) => {
      dirtyParts.current.quota = value;
      onDirtyChange?.(Object.values(dirtyParts.current).some(Boolean));
    },
    [onDirtyChange],
  );
  const markAdvanced = useCallback(
    (value: boolean) => {
      dirtyParts.current.advanced = value;
      onDirtyChange?.(Object.values(dirtyParts.current).some(Boolean));
    },
    [onDirtyChange],
  );
  const markDialog = useCallback(
    (value: boolean) => {
      dirtyParts.current.dialog = value;
      onDirtyChange?.(Object.values(dirtyParts.current).some(Boolean));
    },
    [onDirtyChange],
  );
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
    setError,
  );
  useEffect(() => {
    let disposed = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    void command("get_gateway", { clientId })
      .then((s) => {
        if (!disposed && (s.clientId ?? "codex") === clientId) setState(s);
      })
      .catch((e) => setError(errorOf(e).message));
    events.watch("gateway-state", (s) => {
      if ((s.clientId ?? "codex") === clientId) setState(s);
    });
    return () => {
      disposed = true;
      events.dispose();
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
        setDialog({ kind: focusProvider.action ?? "settings", item: p });
      } else {
        setError("该供应商已不存在，请刷新后重试");
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
  const importDsh = () =>
    run(async () => {
      const preview = await command("preview_dsh_profile", {
        profile: preferredDshProfile(),
        protocol: "chat_completions",
      });
      if (!preview.profile || preview.providers?.some((p) => p.error)) {
        setDialog({ kind: "dsh" });
        return;
      }
      setState(await connectDsh(state, preview));
      notify(state.running ? "DSH 已同步" : "DSH 已接入，自动切换已启用");
    });
  const importGrok = () =>
    run(async () => {
      if (state.running) return;
      if (state.grokManaged) {
        setState(
          await command("start_gateway", {
            clientId: "grok",
            expectedRevision: state.revision,
            expectedConfigRevision: state.configRevision,
          }),
        );
      } else {
        const preview = await command("preview_grok_import", {
          useEnvironment: true,
        });
        if (!preview.token || preview.error) {
          setDialog({ kind: "grokImport" });
          return;
        }
        setState(
          await command("commit_grok_import", {
            token: preview.token,
            expectedRevision: preview.revision,
          }),
        );
      }
      notify("Grok 已接入");
    });
  const toggle = () => {
    if (
      clientId === "grok" &&
      !state.running &&
      !state.grokManaged &&
      !state.recoveryPending
    ) {
      importGrok();
      return;
    }
    if (
      clientId === "dsh" &&
      !state.running &&
      !state.recoveryPending &&
      !state.dshProfile
    ) {
      importDsh();
      return;
    }
    run(async () => {
      setState(
        await command(
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
  };
  const choose = (p: Provider) =>
    run(async () => {
      await edit({ op: "select", id: p.id });
      notify(
        state.running
          ? "已切换供应商，新请求立即生效"
          : clientId === "dsh"
            ? "已选择供应商，可在接入面板启用路由"
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
      <div className="gateway-navigation">
        <ClientSelection
          client={clientId}
          select={onClientChange}
          disabled={busy}
        />
        <div className="gateway-client-actions">
          {clientId === "claude" && (
            <ClaudeProfile disabled={busy || !!dialog} notify={notify} />
          )}
          {clientId === "grok" && !state.running && (
            <button disabled={busy} onClick={importGrok}>
              {state.grokManaged ? "接入当前配置" : "导入并接入"}
            </button>
          )}
          {clientId === "dsh" && (
            <>
              <button disabled={busy} onClick={importDsh}>
                {state.running ? "同步 DSH" : "导入并接入"}
              </button>
              {state.dshProfile && (
                <span className="muted">{state.dshProfile.profile}</span>
              )}
            </>
          )}
        </div>
      </div>
      <div className="gateway-heading gateway-action-row">
        <div className="gateway-route-actions">
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
        </div>
        <div className="gateway-provider-actions">
          <button
            className="secondary"
            disabled={busy}
            onClick={() => setDialog({ kind: "provider" })}
          >
            <Plus size={15} />
            添加
          </button>
          <div
            className="exchange-button-group"
            role="group"
            aria-label="供应商导入导出"
          >
            <button
              className="exchange-button"
              disabled={busy}
              onClick={() =>
                clientId === "dsh"
                  ? importDsh()
                  : setDialog({ kind: "exchange", mode: "import" })
              }
            >
              <FileInput size={16} />
              导入
            </button>
            <button
              className="exchange-button"
              disabled={busy || !state.providers.length}
              onClick={() => setDialog({ kind: "exchange", mode: "export" })}
            >
              <FileOutput size={16} />
              导出
            </button>
          </div>
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
              const status = providerRuntimeStatus(p, state);
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
                    <QuotaInfo
                      compact
                      provider={p}
                      quota={quota.quotaFor(p)}
                      refresh={() => void quota.refresh(p.id)}
                      details={() => setDialog({ kind: "quota", item: p })}
                    />
                    {status && (
                      <button
                        className="provider-alert"
                        onClick={() =>
                          void command("open_main", {
                            page: "logs",
                            eventId: providerFault(p)?.eventId ?? null,
                            providerId: providerFault(p) ? p.id : null,
                            clientId,
                          })
                        }
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

                    <button
                      className="secondary compact"
                      disabled={rowBusy || p.searchOnly}
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
                            setDialog({ kind: "testing", item: p })
                          }
                        >
                          <ConnectionTestIcon />
                          连接测试
                        </button>
                        <button
                          onClick={() =>
                            setDialog({ kind: "settings", item: p })
                          }
                        >
                          <Settings2 size={14} />
                          {clientId === "dsh" ? "模型映射" : "白名单"}
                        </button>
                        <button
                          onClick={() =>
                            setDialog({ kind: "endpoints", item: p })
                          }
                        >
                          <Network size={14} />
                          端点
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
                  {providerHandoffStatus(p, state) && (
                    <span className="provider-binding provider-handoff">
                      {providerHandoffStatus(p, state)}
                    </span>
                  )}
                  <div className="provider-secondary">
                    <span className="provider-domain" title={p.baseUrl}>
                      {new URL(p.baseUrl).host}
                    </span>
                    <ProviderControls
                      clientId={clientId}
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
        <div className="gateway-current" aria-label="当前客户端配置">
          <CurrentConnection
            current={state.current}
            providers={state.providers}
          />
          {state.clientId === "dsh" && !!state.dshProfiles?.length && (
            <details>
              <summary>各 profile 当前配置</summary>
              <ul className="dsh-results">
                {state.dshProfiles.map(({ profile, current }) => (
                  <li key={profile}>
                    <strong>{profile}</strong>
                    <CurrentConnection
                      current={current}
                      providers={state.providers}
                    />
                  </li>
                ))}
              </ul>
            </details>
          )}
        </div>
        <details className="advanced">
          <summary>高级设置</summary>
          <div className="gateway-diagnostics">
            <code>{state.address}</code>
            <span>{state.running ? "运行中" : "已关闭"}</span>
            <span>活动连接 {state.activeConnections}</span>
          </div>
          {clientId !== "dsh" && (
            <details
              className="model-list-details"
              id="gateway-model-list"
              open={modelsOpen}
              onToggle={(e) => setModelsOpen(e.currentTarget.open)}
            >
              <summary>模型列表</summary>
              {modelsOpen && (
                <GatewayModels key={clientId} clientId={clientId} />
              )}
            </details>
          )}
          <QuotaRefreshSettings
            seconds={quotaRefreshSeconds}
            onDirtyChange={markQuota}
          />
          <Advanced
            clientId={clientId}
            onDirtyChange={markAdvanced}
            settings={state.settings}
            listener={state.listener}
            revision={state.revision}
            busy={busy}
            save={async (settings, revision) => {
              await action(() => edit({ op: "settings", settings }, revision));
              notify("网关参数已保存");
            }}
          />
        </details>
      </>
      {dialog?.kind === "exchange" && (
        <ProviderExchange
          mode={dialog.mode}
          state={state}
          clientId={clientId}
          close={() => setDialog(null)}
          update={setState}
          notify={notify}
          onDirtyChange={markDialog}
        />
      )}
      {dialog?.kind === "dsh" && (
        <DshProfile
          state={state}
          close={() => setDialog(null)}
          update={setState}
          notify={notify}
        />
      )}
      {dialog?.kind === "grokImport" && (
        <GrokImport
          close={() => setDialog(null)}
          update={setState}
          notify={notify}
        />
      )}
      {dialog?.kind === "settings" && (
        <ProviderSettings
          key={`${dialog.item.id}:${dialogVersion}`}
          provider={dialog.item}
          onDirtyChange={markDialog}
          clientId={clientId}
          revision={state.revision}
          disabled={busy}
          runtime={
            state.providers.find((p) => p.id === dialog.item.id) ?? dialog.item
          }
          close={() => setDialog(null)}
          save={async (payload, revision) => {
            await edit(payload, revision);
            notify(clientId === "dsh" ? "模型映射已保存" : "白名单已保存");
          }}
        />
      )}
      {dialog?.kind === "testing" && (
        <ProviderTesting
          key={`${clientId}:${dialog.item.id}:${dialogVersion}`}
          onDirtyChange={markDialog}
          gateway={state}
          provider={
            state.providers.find((p) => p.id === dialog.item.id) ?? dialog.item
          }
          close={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "quota" && (
        <Modal
          compact
          title={`${dialog.item.name} 额度`}
          close={() => setDialog(null)}
        >
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
      {dialog?.kind === "endpoints" && (
        <ProviderEndpoints
          provider={dialog.item}
          runtime={
            state.providers.find((p) => p.id === dialog.item.id) ?? dialog.item
          }
          clientId={clientId}
          revision={state.revision}
          onDirtyChange={markDialog}
          update={setState}
          save={edit}
          close={() => setDialog(null)}
        />
      )}
      {dialog?.kind === "provider" && (
        <GatewayDialog
          key={`${clientId}:${dialog.item?.id ?? "new"}`}
          clientId={clientId}
          connectionMode={state.connectionMode}
          dialog={dialog}
          revision={state.revision}
          onDirtyChange={markDialog}
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
