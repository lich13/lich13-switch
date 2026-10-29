import QuickWaits from "./QuickWaits";
import QuickFreeze from "./QuickFreeze";
import { subscriptions } from "./subscriptions";
import type { GatewayEdit } from "./bridge-contracts";
import ClientSelection, {
  useClientSelection,
  clientName,
  clients,
} from "./ClientSelection";
import { saveGatewayEdit, type EditRevision } from "./gateway-edit";
import type { ClientId } from "./types";
import { useEffect, useState } from "react";
import {
  ArrowUpRight,
  Check,
  Pin,
  Power,
  Search,
  X,
  Settings2,
  RotateCcw,
} from "lucide-react";
import { command, preview } from "./bridge";
import { errorOf, type GatewayState, type ViewState } from "./types";
import { QuotaInfo, useProviderQuota } from "./Quota";
import {
  providerRuntimeStatus,
  providerHandoffStatus,
  providerFault,
} from "./provider-status";
import QuickControls from "./QuickControls";
import AuthSyncNotice from "./AuthSyncNotice";
import AccountPlan from "./AccountPlan";
import ProviderControls from "./ProviderControls";
import ProviderNameEditor from "./ProviderNameEditor";
import ConnectionTestIcon from "./ConnectionTestIcon";
import SortableProviders, { type ProviderCommit } from "./SortableProviders";
import type { QuickPreferences as Preferences } from "./bridge-contracts";
export default function QuickPanel() {
  const [clientId, select] = useClientSelection("quick");
  return (
    <div className="quick-panel">
      <QuickFreeze />
      <QuickContent
        key={clientId}
        clientId={clientId}
        onClientChange={select}
      />
    </div>
  );
}
function QuickContent({
  clientId,
  onClientChange,
}: {
  clientId: ClientId;
  onClientChange: (id: ClientId) => void;
}) {
  const [accounts, setAccounts] = useState<ViewState | null>(null);
  const [gateway, setGateway] = useState<GatewayState | null>(null);
  const [prefs, setPrefs] = useState<Preferences>({
    pinned: false,
    tab: "providers",
  });
  const [visible, setVisible] = useState(preview);
  const [query, setQuery] = useState("");
  const [error, setError] = useState("");
  const [notice, setNotice] = useState("");
  const [busy, setBusy] = useState(false);
  const [systemDark, setSystemDark] = useState(
    matchMedia("(prefers-color-scheme: dark)").matches,
  );
  const visibleClients = clients.filter(
    (id) => !prefs.hiddenClients?.includes(id),
  );
  const clientVisible = visibleClients.includes(clientId);
  useEffect(() => {
    if (!clientVisible && visibleClients[0]) onClientChange(visibleClients[0]);
  }, [clientVisible, visibleClients.join(","), onClientChange]);
  const quota = useProviderQuota(
    gateway?.providers ?? [],
    visible && prefs.tab === "providers" && clientVisible,
    "panel-visibility",
    clientId,
    accounts?.preferences.quotaRefreshSeconds ?? 60,
    setError,
  );
  useEffect(() => {
    let disposed = false;
    let receivedAccountState = false;
    let receivedGatewayState = false;
    let receivedQuickState = false;
    let receivedVisibility = false;
    const events = subscriptions((e) => setError(errorOf(e).message));
    const watch = events.watch;
    watch("switch-state", (next) => {
      receivedAccountState = true;
      setAccounts(next);
    });
    watch("gateway-state", (s) => {
      if ((s.clientId ?? "codex") === clientId) {
        receivedGatewayState = true;
        setGateway(s);
      }
    });
    watch("quick-state", (p) => {
      receivedQuickState = true;
      setPrefs(p);
    });
    watch("panel-visibility", (value) => {
      receivedVisibility = true;
      setVisible(value);
    });
    watch("switch-notice", setNotice);
    watch("switch-error", (e) => setError(errorOf(e).message));
    void Promise.all([
      command("get_state"),
      command("get_gateway", { clientId }),
      command("get_quick"),
    ])
      .then(([a, g, p]) => {
        if (!disposed) {
          if (!receivedAccountState) setAccounts(a);
          if (!receivedGatewayState) setGateway(g);
          if (!receivedQuickState) setPrefs(p);
          if (!receivedVisibility && !receivedQuickState)
            setVisible(p.visible ?? preview);
        }
      })
      .catch((e) => {
        if (!disposed) setError(errorOf(e).message);
      });
    void command("get_startup_error")
      .then((e) => {
        if (e && !disposed) setError(e.message);
      })
      .catch(() => {});
    const media = matchMedia("(prefers-color-scheme: dark)");
    const change = () => setSystemDark(media.matches);
    media.addEventListener("change", change);
    return () => {
      disposed = true;
      events.dispose();
      media.removeEventListener("change", change);
    };
  }, []);
  useEffect(() => {
    document.documentElement.dataset.theme =
      !accounts || accounts.preferences.theme === "system"
        ? systemDark
          ? "dark"
          : "light"
        : accounts.preferences.theme;
  }, [accounts?.preferences.theme, systemDark]);
  useEffect(() => {
    if (!notice) return;
    const timer = setTimeout(() => setNotice(""), 6500);
    return () => clearTimeout(timer);
  }, [notice]);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (
        event.key === "Escape" &&
        !event.isComposing &&
        !document.querySelector("dialog[open]") &&
        !event.defaultPrevented
      )
        void command("hide_quick").catch((e) => setError(errorOf(e).message));
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, []);
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
  const run = (fn: () => Promise<void>) => void action(fn).catch(() => {});
  const edit = async (edit: GatewayEdit, expected?: EditRevision) => {
    if (!gateway) return;
    await saveGatewayEdit(clientId, gateway, edit, expected, setGateway);
    if (edit.op === "select")
      setNotice(
        gateway.running
          ? "供应商已切换，新请求已生效"
          : `配置已切换，请重新打开 ${clientName(clientId)}`,
      );
  };
  const providerEdit: ProviderCommit = (payload, expected) =>
    action(() => edit(payload, expected), false);
  const resetProvider = (provider: GatewayState["providers"][number]) =>
    run(async () => {
      await edit({ op: "reset", id: provider.id });
      setNotice("已重置熔断");
    });
  const account = async (id: string) => {
    if (!accounts) return;
    setAccounts(
      await command("switch_account", {
        id,
        expectedRevision: accounts.authRevision,
      }),
    );
    setNotice("文件已切换，请重新打开 Codex");
  };
  const open = (page?: string) =>
    run(async () => {
      await command("open_main", { page: page ?? null });
    });
  return (
    <>
      <header className="quick-toolbar">
        <nav className="segmented" aria-label="快捷面板">
          {(["providers", "accounts"] as const).map((tab) => (
            <button
              key={tab}
              aria-pressed={prefs.tab === tab}
              onClick={() =>
                run(async () => {
                  setPrefs(await command("set_quick", { tab }));
                  setQuery("");
                })
              }
            >
              {tab === "providers" ? "供应商" : "账号"}
            </button>
          ))}
        </nav>
        <span className="quick-spacer" />
        <button
          className="icon-button"
          aria-label={prefs.pinned ? "取消固定面板" : "固定面板"}
          aria-pressed={prefs.pinned}
          onClick={() =>
            run(async () =>
              setPrefs(await command("set_quick", { pinned: !prefs.pinned })),
            )
          }
        >
          <Pin size={15} />
        </button>
        <button
          className="icon-button"
          aria-label="关闭快捷面板"
          onClick={() =>
            void command("hide_quick").catch((e) =>
              setError(errorOf(e).message),
            )
          }
        >
          <X size={16} />
        </button>
      </header>
      <div className="quick-scroll">
        <div>
          {prefs.tab === "providers" && visibleClients.length > 0 && (
            <ClientSelection
              client={clientVisible ? clientId : visibleClients[0]}
              visibleClients={visibleClients}
              disabled={busy}
              select={onClientChange}
            />
          )}
          {(error || gateway?.error || accounts?.error) && (
            <div className="quick-feedback error" role="alert">
              {error || gateway?.error || accounts?.error}
            </div>
          )}
          {notice && (
            <div className="quick-feedback" role="status">
              {notice}
            </div>
          )}
          {prefs.tab === "providers" ? (
            !clientVisible ? (
              <div className="quick-empty">
                未显示平台
                <button
                  className="text-button"
                  onClick={() => open("settings")}
                >
                  设置平台
                </button>
              </div>
            ) : (
              <>
                <div className="quick-status">
                  <span
                    className={`status-pill ${gateway?.running ? "online" : ""}`}
                  >
                    <span className="dot" />
                    {gateway?.running ? "运行中" : "已关闭"}
                  </span>
                  <span className="quick-spacer" />
                  <button
                    className="text-button"
                    disabled={busy || !gateway}
                    aria-pressed={gateway?.mode === "auto"}
                    onClick={() =>
                      run(() =>
                        edit({
                          op: "mode",
                          mode: gateway?.mode === "auto" ? "manual" : "auto",
                        }),
                      )
                    }
                  >
                    {gateway?.mode === "auto" ? "自动" : "手动"}
                  </button>
                  <button
                    className="icon-button"
                    aria-label={gateway?.running ? "停用网关" : "启用网关"}
                    disabled={
                      busy ||
                      !gateway ||
                      (!gateway.running && !gateway.providers.length)
                    }
                    onClick={() =>
                      run(async () => {
                        if (!gateway) return;
                        setGateway(
                          await command(
                            gateway.running || gateway.recoveryPending
                              ? "stop_gateway"
                              : "start_gateway",
                            {
                              expectedRevision: gateway.revision,
                              expectedConfigRevision: gateway.configRevision,
                              clientId,
                            },
                          ),
                        );
                        setNotice(
                          `配置已切换，请重新打开 ${clientName(clientId)}`,
                        );
                      })
                    }
                  >
                    <Power size={16} />
                  </button>
                </div>
                {Boolean(gateway?.waitingRequests) && (
                  <div className="quick-waiting">
                    等待 {gateway?.waitingRequests}
                  </div>
                )}
                {gateway && (
                  <QuickWaits
                    gateway={gateway}
                    clientId={clientId}
                    visible={visible}
                    notify={setNotice}
                    error={setError}
                  />
                )}
                {!gateway ? (
                  <div className="quick-empty">正在读取供应商…</div>
                ) : !gateway.providers.length ? (
                  <div className="quick-empty">
                    尚无供应商
                    <button
                      className="text-button"
                      onClick={() => open("gateway")}
                    >
                      添加供应商
                    </button>
                  </div>
                ) : (
                  <SortableProviders
                    providers={gateway.providers}
                    revision={gateway.revision}
                    disabled={busy}
                    visible={visible && prefs.tab === "providers"}
                    commit={providerEdit}
                    report={setError}
                    rowClass={(p) =>
                      `quick-provider ${gateway.mode === "manual" && gateway.selected === p.id ? "selected" : ""}`
                    }
                  >
                    {(p, priority, handle, rowBusy) => (
                      <>
                        <div className="quick-provider-main">
                          {handle}
                          {priority && (
                            <span className="quick-priority">P{priority}</span>
                          )}
                          <ProviderNameEditor
                            provider={p}
                            revision={gateway.revision}
                            disabled={rowBusy}
                            commit={(payload, revision) =>
                              edit(payload, revision)
                            }
                            report={setError}
                            className="quick-name"
                          />
                          <QuotaInfo
                            compact
                            provider={p}
                            quota={quota.quotaFor(p)}
                            refresh={() => void quota.refresh(p.id)}
                          />

                          <button
                            className="quick-select"
                            disabled={rowBusy}
                            onClick={() =>
                              run(() => edit({ op: "select", id: p.id }))
                            }
                            aria-label={`选择 ${p.name}`}
                          >
                            {gateway.mode === "manual" &&
                              gateway.selected === p.id && <Check size={15} />}
                            <span>
                              {gateway.mode === "manual" &&
                              gateway.selected === p.id
                                ? "已选择"
                                : "选择"}
                            </span>
                          </button>
                          <button
                            type="button"
                            className="icon-button compact quick-provider-reset"
                            aria-label={`${p.name} 重置熔断`}
                            title="重置熔断"
                            disabled={rowBusy}
                            onClick={() => resetProvider(p)}
                          >
                            <RotateCcw size={14} />
                          </button>
                          <button
                            className="icon-button quick-provider-settings"
                            disabled={rowBusy}
                            aria-label={`${p.name} ${clientId === "dsh" ? "模型映射" : "白名单"}`}
                            title={clientId === "dsh" ? "模型映射" : "白名单"}
                            onClick={() =>
                              void command("open_main", {
                                page: "gateway",
                                providerId: p.id,
                                clientId,
                              })
                            }
                          >
                            <Settings2 size={14} />
                          </button>
                          <button
                            type="button"
                            className="icon-button compact"
                            aria-label={`${p.name} 连接测试`}
                            title="连接测试"
                            disabled={rowBusy}
                            onClick={() =>
                              run(() =>
                                command("open_main", {
                                  page: "gateway",
                                  providerId: p.id,
                                  clientId,
                                  providerAction: "testing",
                                }),
                              )
                            }
                          >
                            <ConnectionTestIcon />
                          </button>
                        </div>
                        {providerHandoffStatus(p, gateway) && (
                          <span className="provider-binding provider-handoff">
                            {providerHandoffStatus(p, gateway)}
                          </span>
                        )}
                        {providerRuntimeStatus(p, gateway) && (
                          <button
                            type="button"
                            className="quick-provider-alert"
                            onClick={() =>
                              void command("open_main", {
                                page: "logs",
                                eventId: providerFault(p)?.eventId ?? null,
                                providerId: providerFault(p) ? p.id : null,
                                clientId,
                              })
                            }
                          >
                            {providerRuntimeStatus(p, gateway)}
                          </button>
                        )}
                        <div className="quick-provider-secondary">
                          <ProviderControls
                            clientId={clientId}
                            provider={p}
                            revision={gateway.revision}
                            disabled={rowBusy}
                            visible={visible && prefs.tab === "providers"}
                            commit={providerEdit}
                            report={setError}
                          />
                        </div>
                      </>
                    )}
                  </SortableProviders>
                )}
              </>
            )
          ) : (
            <>
              <label className="quick-search">
                <Search size={14} />
                <input
                  aria-label="搜索账号"
                  placeholder="搜索账号"
                  value={query}
                  onChange={(e) => setQuery(e.target.value)}
                />
              </label>
              {accounts && (
                <AuthSyncNotice
                  state={accounts}
                  busy={busy}
                  apply={(id) => run(() => account(id))}
                />
              )}
              {accounts?.accounts
                .filter((a) =>
                  `${a.name} ${a.email ?? ""}`
                    .toLowerCase()
                    .includes(query.toLowerCase()),
                )
                .map((a) => (
                  <button
                    className="quick-account"
                    key={a.id}
                    disabled={busy || a.current}
                    onClick={() => run(() => account(a.id))}
                    aria-label={`切换到 ${a.name}`}
                  >
                    <span className="quick-name">
                      <span className="account-title">
                        <strong title={a.name}>{a.name}</strong>
                        <AccountPlan account={a} />
                      </span>
                      <span>
                        {a.kind === "chatgpt" ? "ChatGPT" : "API Key"}
                        {a.current ? " · 当前文件" : ""}
                      </span>
                    </span>
                    {a.current && <Check size={15} />}
                  </button>
                ))}
              {!accounts?.accounts.length && (
                <div className="quick-empty">
                  尚无已保存账号
                  <button
                    className="text-button"
                    onClick={() => open("accounts")}
                  >
                    添加账号
                  </button>
                </div>
              )}
            </>
          )}
        </div>
      </div>
      <QuickControls visible={visible} notify={setNotice} error={setError} />
      <footer className="quick-footer">
        <button onClick={() => open()}>
          打开 lich13-switch <ArrowUpRight size={12} />
        </button>
        <button
          onClick={() =>
            open(prefs.tab === "providers" ? "gateway" : "accounts")
          }
        >
          {prefs.tab === "providers" ? "管理供应商" : "管理账号"}
        </button>
      </footer>
    </>
  );
}
