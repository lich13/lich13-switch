import { confirmAction } from "./confirmation";
import {
  Suspense,
  lazy,
  useCallback,
  useEffect,
  useRef,
  useState,
} from "react";
import {
  ArrowLeftRight,
  Copy,
  Network,
  Shield,
  Users,
  FileCode2,
  Settings,
  Search,
  Plus,
  Check,
  KeyRound,
  UserRound,
  MoreHorizontal,
  X,
  Upload,
  ScrollText,
  ExternalLink,
  ChevronRight,
  AlertTriangle,
  LoaderCircle,
  FolderOpen,
  Sun,
  Moon,
  Monitor,
  Github,
  RefreshCw,
  LogIn,
  Trash2,
  Pencil,
} from "lucide-react";
import { command, subscribe } from "./bridge";
import AuthSyncNotice from "./AuthSyncNotice";
import StartupSettings from "./StartupSettings";
import PowerSettings from "./PowerSettings";
import LinkSettings from "./LinkSettings";
import ProviderImports from "./ProviderImports";
import NotificationPermission from "./NotificationPermission";
import { version } from "../package.json";
import {
  errorOf,
  type ClientId,
  type Account,
  type ViewState,
  type Preferences,
  type LoginState,
  type UpdateInfo,
} from "./types";
const EventLog = lazy(() => import("./EventLog"));
const Gateway = lazy(() => import("./Gateway"));
const ConfigEditor = lazy(() => import("./ConfigEditor"));
type Dialog =
  | "add"
  | "settings"
  | { action: "rename" | "delete"; account: Account }
  | null;
const emptyLogin: LoginState = {
  phase: "idle",
  mode: "",
  url: null,
  code: null,
  message: "",
  callbackReady: false,
};
export default function App() {
  const [state, setState] = useState<ViewState | null>(null),
    [page, setPage] = useState<"accounts" | "config" | "gateway" | "logs">("accounts"),
    [search, setSearch] = useState(""),
    [dialog, setDialog] = useState<Dialog>(null),
    [error, setError] = useState(""),
    [message, setMessage] = useState(""),
    [busy, setBusy] = useState(false),
    [dirty, setDirty] = useState(false),
    [themePreview, setThemePreview] = useState<Preferences["theme"] | null>(
      null,
    ),
    [systemDark, setSystemDark] = useState(
      window.matchMedia("(prefers-color-scheme: dark)").matches,
    ),
    [login, setLogin] = useState(emptyLogin);
  const dirtyRef = useRef(false),
    pageRef = useRef(page);
  const gatewayDirty = useRef(false);
  const [focusProvider, setFocusProvider] = useState<{
    id: string;
    clientId?: ClientId;
    sequence: number;
  } | null>(null);
  const gatewayDraftChanged = useCallback((value: boolean) => {
    gatewayDirty.current = value;
  }, []);
  dirtyRef.current = dirty;
  pageRef.current = page;
  const notify = useCallback((s: string) => setMessage(s), []);
  const navigate = useCallback(
    async (next: "accounts" | "config" | "gateway" | "logs") => {
      if (next === pageRef.current) return;
      if (
        gatewayDirty.current &&
        !(await confirmAction("离开当前页面会丢弃未保存的表单。"))
      )
        return;
      if (
        pageRef.current === "config" &&
        next !== "config" &&
        dirtyRef.current &&
        !(await confirmAction("离开配置页会丢弃未保存的草稿。"))
      )
        return;
      setPage(next);
      if (next !== "config") setDirty(false);
    },
    [],
  );
  useEffect(() => {
    const menus = () =>
      document.querySelectorAll<HTMLDetailsElement>(
        ".row-menu[open], .account-menu[open]",
      );
    const key = (event: KeyboardEvent) => {
      if (event.key !== "Escape" || document.querySelector("dialog[open]"))
        return;
      for (const menu of menus()) {
        menu.open = false;
        menu.querySelector<HTMLElement>("summary")?.focus();
      }
    };
    const outside = (event: PointerEvent) => {
      for (const menu of menus())
        if (!menu.contains(event.target as Node)) menu.open = false;
    };
    document.addEventListener("keydown", key);
    document.addEventListener("pointerdown", outside);
    return () => {
      document.removeEventListener("keydown", key);
      document.removeEventListener("pointerdown", outside);
    };
  }, []);
  useEffect(() => {
    let disposed = false;
    const cleaners: (() => void)[] = [];
    void Promise.all([
      command<ViewState>("get_state"),
      command<LoginState>("get_login"),
    ])
      .then(([s, l]) => {
        if (!disposed) {
          setState(s);
          setLogin(l);
          void command("frontend_ready").catch((e) =>
            setError(errorOf(e).message),
          );
        }
      })
      .catch((e) => setError(errorOf(e).message));
    const events: [string, (p: unknown) => void][] = [
      ["switch-state", (p) => setState(p as ViewState)],
      ["switch-notice", (p) => setMessage(String(p))],
      ["switch-error", (p) => setError(errorOf(p).message)],
      [
        "navigate",
        (p) =>
          p === "settings"
            ? (async () => {
                if (
                  (gatewayDirty.current || dirtyRef.current) &&
                  !(await confirmAction("打开设置会丢弃未保存的草稿。"))
                )
                  return;
                setPage("accounts");
                setDirty(false);
                setThemePreview(null);
                setDialog("settings");
              })()
            : navigate(p === "config" || p === "gateway" ? p : "accounts"),
      ],
      [
        "provider-settings",
        async (p) => {
          if (
            (gatewayDirty.current || dirtyRef.current) &&
            !(await confirmAction("打开供应商设置会丢弃未保存的草稿。"))
          )
            return;
          setPage("gateway");
          setDirty(false);
          setFocusProvider({
            ...(typeof p === "string"
              ? { id: p, clientId: "codex" as const }
              : (p as { id: string; clientId: ClientId })),
            sequence: Date.now(),
          });
        },
      ],
      ["login-state", (p) => setLogin(p as LoginState)],
    ];
    for (const [event, fn] of events)
      void subscribe(event, fn).then((clean) => {
        if (disposed) clean();
        else cleaners.push(clean);
      });
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const change = () => setSystemDark(media.matches);
    media.addEventListener("change", change);
    return () => {
      disposed = true;
      cleaners.forEach((c) => c());
      media.removeEventListener("change", change);
    };
  }, [navigate]);
  useEffect(() => {
    if (!message) return;
    const t = setTimeout(() => setMessage(""), 6500);
    return () => clearTimeout(t);
  }, [message]);
  const selectedTheme = themePreview ?? state?.preferences.theme ?? "system";
  const theme: "dark" | "light" =
    selectedTheme === "dark"
      ? "dark"
      : selectedTheme === "light"
        ? "light"
        : systemDark
          ? "dark"
          : "light";
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
  }, [theme]);
  const action = async (fn: () => Promise<unknown>) => {
    setBusy(true);
    setError("");
    try {
      await fn();
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };
  const switchAccount = (a: Account) =>
    void action(async () => {
      if (!state) return;
      const s = await command<ViewState>("switch_account", {
        id: a.id,
        expectedRevision: state.authRevision,
      });
      setState(s);
      notify("文件已切换，请重新打开 Codex");
    });
  const useOfficial = (a: Account) =>
    void action(async () => {
      if (!state) return;
      const s = await command<ViewState>("use_official_account", {
        accountId: a.id,
        expectedAuthRevision: state.authRevision,
        expectedConfigRevision: state.configRevision,
      });
      setState(s);
      notify("官方账号已启用，请重新打开 Codex");
    });
  const disableOfficial = () =>
    void action(async () => {
      if (!state) return;
      const s = await command<ViewState>("disable_official_account", {
        expectedConfigRevision: state.configRevision,
      });
      setState(s);
      notify("官方连接已关闭，配置已恢复");
    });
  const accounts =
      state?.accounts.filter((a) =>
        (a.name + " " + (a.email ?? ""))
          .toLowerCase()
          .includes(search.toLowerCase()),
      ) ?? [];
  return (
    <div className="app-shell">
      <aside className="sidebar">
        <div className="brand">
          <img src="/icon.svg" alt="" />
          <div>
            <strong>lich13-switch</strong>
          </div>
        </div>
        <nav aria-label="主导航">
          <button
            className={page === "accounts" ? "nav-item active" : "nav-item"}
            onClick={() => navigate("accounts")}
          >
            <Users size={17} />
            账号
          </button>
          <button
            className={page === "config" ? "nav-item active" : "nav-item"}
            onClick={() => navigate("config")}
          >
            <FileCode2 size={17} />
            配置
          </button>
          <button
            className={page === "gateway" ? "nav-item active" : "nav-item"}
            onClick={() => navigate("gateway")}
          >
            <Network size={17} />
            网关
          </button>
          <button className={page === "logs" ? "nav-item active" : "nav-item"} onClick={() => navigate("logs")}>
            <ScrollText size={17} />日志
          </button>
        </nav>
        <div className="sidebar-bottom">
          <button
            className="nav-item"
            onClick={() => {
              setThemePreview(null);
              setDialog("settings");
            }}
          >
            <Settings size={17} />
            设置<span className="version">v{version}</span>
          </button>
        </div>
      </aside>
      <main>
        {(error || state?.error) && (
          <div className="banner error global-error" role="alert">
            <AlertTriangle size={16} />
            <span>{error || state?.error}</span>
            <button
              className="icon-button"
              aria-label="关闭提示"
              onClick={() => setError("")}
            >
              <X size={15} />
            </button>
          </div>
        )}
        {page === "logs" ? (<Suspense fallback={null}><EventLog /></Suspense>) : page === "accounts" ? (
          <section className="accounts-page">
            <div className="page-heading">
              <div>
                <h1>账号</h1>
              </div>
              <button
                className="primary"
                onClick={() => setDialog("add")}
                disabled={!state}
              >
                <Plus size={16} />
                添加账号
              </button>
            </div>
            {(state?.currentState === "unsaved" ||
              state?.currentState === "missing" ||
              state?.currentState === "invalid") && (
              <div className="banner">
                <span>
                  {state.currentState === "unsaved"
                    ? "当前文件中的账号尚未保存"
                    : state.currentState === "missing"
                      ? "尚无 auth.json，可添加账号或登录 ChatGPT"
                      : "auth.json 无法识别，请重新导入或登录"}
                </span>
                {state.currentState === "unsaved" && (
                  <button
                    className="text-button"
                    onClick={() =>
                      void action(async () =>
                        setState(await command("import_current")),
                      )
                    }
                  >
                    保存当前账号
                  </button>
                )}
              </div>
            )}
            {state && (
              <AuthSyncNotice
                state={state}
                busy={busy}
                apply={(id) => {
                  const account = state.accounts.find((a) => a.id === id);
                  if (account) void switchAccount(account);
                }}
              />
            )}
            {state?.officialMode?.state === "conflict" ||
            state?.officialMode?.state === "unavailable" ? (
              <div className="banner error" role="alert">
                <AlertTriangle size={16} />
                <span>{state.officialMode.error}</span>
                {(state.officialMode.enabled || state.officialMode.accountId) && (
                  <button className="text-button" onClick={disableOfficial} disabled={busy}>
                    关闭官方连接
                  </button>
                )}
              </div>
            ) : state?.officialMode?.enabled ? (
              <div className="banner">
                <Shield size={16} />
                <span>官方账号已启用</span>
                <button className="text-button" onClick={disableOfficial} disabled={busy}>
                  关闭官方连接
                </button>
              </div>
            ) : null}
            <div className="list-tools">
              <h2>
                已保存账号 <span>{state?.accounts.length ?? 0}</span>
              </h2>
              <label className="search">
                <Search size={15} />
                <input
                  aria-label="搜索账号"
                  placeholder="搜索账号"
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                />
                <kbd>⌕</kbd>
              </label>
            </div>
            <div className="account-list" aria-label="已保存账号">
              {!state ? (
                <div className="empty">
                  <LoaderCircle className="spin" />
                  正在读取账号…
                </div>
              ) : accounts.length === 0 ? (
                <div className="empty">
                  <Users size={30} />
                  <h3>
                    {search ? "没有找到匹配账号" : "从你的第一个账号开始"}
                  </h3>
                  <p>
                    {search
                      ? "尝试其他名称或邮箱"
                      : "登录 ChatGPT，或导入已有 auth.json。"}
                  </p>
                  {!search && (
                    <button
                      className="secondary"
                      onClick={() => setDialog("add")}
                    >
                      <Plus size={15} />
                      添加账号
                    </button>
                  )}
                </div>
              ) : (
                accounts.map((a) => (
                  <div
                    key={a.id}
                    className={"account-row" + (a.current ? " selected" : "")}
                  >
                    <div className={"avatar " + a.kind}>
                      {a.kind === "chatgpt" ? (
                        <UserRound size={20} />
                      ) : (
                        <KeyRound size={20} />
                      )}
                    </div>
                    <div className="account-copy">
                      <strong title={a.name}>{a.name}</strong>
                      <div>
                        <span>
                          {a.kind === "chatgpt" ? "ChatGPT" : "API Key"}
                        </span>
                        {a.email && <span title={a.email}>{a.email}</span>}
                      </div>
                    </div>
                    <div className="account-actions">
                      {a.current ? (
                        <span className="current-badge">
                          <Check size={13} />
                          当前文件
                        </span>
                      ) : (
                        <button
                          className="switch-button"
                          disabled={busy}
                          aria-label={"切换到 " + a.name}
                          onClick={() => switchAccount(a)}
                        >
                          切换
                          <ArrowLeftRight size={13} />
                        </button>
                      )}
                      {a.kind === "chatgpt" && (
                        state?.officialMode?.enabled &&
                        state.officialMode.accountId === a.id ? (
                          <button
                            className="text-button"
                            disabled={busy}
                            onClick={disableOfficial}
                          >
                            官方连接
                          </button>
                        ) : (
                          <button
                            className="text-button"
                            disabled={busy || Boolean(state?.officialMode?.enabled)}
                            onClick={() => useOfficial(a)}
                          >
                            使用官方账号
                          </button>
                        )
                      )}
                      <details className="account-menu">
                        <summary aria-label={"管理 " + a.name}>
                          <MoreHorizontal size={18} />
                        </summary>
                        <div className="menu-popover">
                          <button
                            onClick={(e) => {
                              e.currentTarget
                                .closest("details")
                                ?.removeAttribute("open");
                              setDialog({ action: "rename", account: a });
                            }}
                          >
                            <Pencil size={14} />
                            重命名
                          </button>
                          <button
                            className="danger"
                            onClick={(e) => {
                              e.currentTarget
                                .closest("details")
                                ?.removeAttribute("open");
                              setDialog({ action: "delete", account: a });
                            }}
                          >
                            <Trash2 size={14} />
                            删除
                          </button>
                        </div>
                      </details>
                    </div>
                  </div>
                ))
              )}
            </div>
          </section>
        ) : page === "gateway" ? (
          <Suspense fallback={<div className="empty">正在打开网关…</div>}>
            <Gateway
              quotaRefreshSeconds={state?.preferences.quotaRefreshSeconds ?? 60}
              focusProvider={focusProvider}
              notify={notify}
              onDirtyChange={gatewayDraftChanged}
            />
          </Suspense>
        ) : (
          state && (
            <Suspense fallback={<div className="empty">正在打开编辑器…</div>}>
              <ConfigEditor
                revision={state.configRevision}
                home={state.preferences.codexHome}
                claudeHome={state.preferences.claudeHome ?? "~/.claude"}
                theme={theme}
                onDirty={setDirty}
                onMessage={notify}
              />
            </Suspense>
          )
        )}
      </main>
      {message && (
        <div className="toast" role="status">
          <Check size={17} />
          {message}
          <button
            aria-label="关闭成功提示"
            className="icon-button"
            onClick={() => setMessage("")}
          >
            <X size={14} />
          </button>
        </div>
      )}
      <ProviderImports notify={notify} blocked={dirty} />
      {dialog && state && (
        <Modal
          title={
            dialog === "add"
              ? "添加账号"
              : dialog === "settings"
                ? "设置"
                : dialog.action === "rename"
                  ? "重命名账号"
                  : "删除账号"
          }
          onClose={() => {
            if (dialog === "settings") setThemePreview(null);
            setDialog(null);
          }}
        >
          {dialog === "add" ? (
            <AddAccount
              login={login}
              setLogin={setLogin}
              onDone={(s) => {
                setState(s);
                setDialog(null);
              }}
            />
          ) : dialog === "settings" ? (
            <SettingsForm
              preferences={state.preferences}
              onThemePreview={setThemePreview}
              onDone={(s) => {
                setState(s);
                setThemePreview(null);
                setDialog(null);
              }}
            />
          ) : (
            <AccountEdit
              dialog={dialog}
              onDone={(s) => {
                setState(s);
                setDialog(null);
              }}
            />
          )}
        </Modal>
      )}
    </div>
  );
}
function Modal({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  useEffect(() => {
    ref.current?.showModal();
  }, []);
  return (
    <dialog
      ref={ref}
      className="modal"
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal-heading">
        <h2>{title}</h2>
        <button
          className="icon-button"
          aria-label="关闭对话框"
          onClick={onClose}
        >
          <X size={18} />
        </button>
      </div>
      {children}
    </dialog>
  );
}
function AddAccount({
  login,
  setLogin,
  onDone,
}: {
  login: LoginState;
  setLogin: (s: LoginState) => void;
  onDone: (s: ViewState) => void;
}) {
  const [tab, setTab] = useState<"login" | "import">("login"),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false);
  const running = ["starting", "waiting", "cancelling"].includes(login.phase);
  const [copied, setCopied] = useState<"url" | "code" | null>(null);
  const [callbackDraft, setCallbackDraft] = useState("");
  useEffect(() => setCopied(null), [login.phase, login.url, login.code]);
  useEffect(() => {
    if (
      login.mode !== "browser" ||
      !["starting", "waiting"].includes(login.phase) ||
      !login.callbackReady
    )
      setCallbackDraft("");
  }, [login.mode, login.phase, login.callbackReady]);
  const canCopy = login.mode === "device" && ["starting", "waiting"].includes(login.phase);
  const perform = async (fn: () => Promise<void>) => {
    setBusy(true);
    setError("");
    try {
      await fn();
    } catch (e) {
      setError(errorOf(e).message);
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="modal-content">
      <div className="segments">
        {(["login", "import"] as const).map((t) => (
          <button
            key={t}
            className={tab === t ? "active" : ""}
            onClick={() => setTab(t)}
          >
            {t === "login" ? "ChatGPT 登录" : "导入凭据"}
          </button>
        ))}
      </div>
      {tab === "login" ? (
        <>
          <div className="login-illustration">
            <img src="/icon.svg" alt="" />
            <ArrowLeftRight size={20} />
            <div>
              <UserRound size={24} />
            </div>
          </div>
          <h3 className="center">连接你的 ChatGPT 账号</h3>
          {login.message && login.phase !== "idle" && (
            <div
              className={"banner " + (login.phase === "error" ? "error" : "")}
            >
              {running ? (
                <LoaderCircle size={16} className="spin" />
              ) : (
                <Check size={16} />
              )}
              <span>{login.message}</span>
            </div>
          )}
          {login.mode === "browser" && running && login.callbackReady && (
            <form
              className="login-callback"
              onSubmit={(event) => {
                event.preventDefault();
                if (!callbackDraft.trim()) return;
                void perform(async () => {
                  const next = await command<LoginState>("complete_login_callback", {
                    callbackUrl: callbackDraft.trim(),
                  });
                  setLogin(next);
                  setCallbackDraft("");
                });
              }}
            >
              <label className="field">
                回调地址
                <input
                  name="callbackUrl"
                  type="password"
                  autoComplete="off"
                  aria-label="回调地址"
                  placeholder="粘贴浏览器回调地址"
                  value={callbackDraft}
                  onChange={(event) => setCallbackDraft(event.target.value)}
                  disabled={busy}
                />
              </label>
              <button className="secondary" disabled={busy || !callbackDraft.trim()}>
                完成登录
              </button>
            </form>
          )}
          {login.code && running && (
            <div className="device-code">
              <span>设备码</span>
              <strong>{login.code}</strong>
            </div>
          )}
          <div className="login-actions">
            {running ? (
              <>
                <button
                  className="primary"
                  disabled={busy || !login.url || login.phase === "cancelling"}
                  onClick={() =>
                    void perform(async () => {
                      await command("open_login_url");
                    })
                  }
                >
                  打开登录页面
                  <ExternalLink size={15} />
                </button>
                {login.mode === "device" && (
                  <div className="login-copy-actions">
                    {(["url", "code"] as const).map((kind) => (
                      <button
                        key={kind}
                        type="button"
                        className="secondary"
                        aria-label={kind === "url" ? "复制链接" : "复制设备码"}
                        disabled={busy || !canCopy || !login[kind]}
                        onClick={() => void perform(async () => {
                          setCopied(null);
                          await command("copy_login_value", { kind });
                          setCopied(kind);
                        })}
                      >
                        {copied === kind ? <Check size={15} /> : <Copy size={15} />}
                        {copied === kind ? "已复制" : kind === "url" ? "复制链接" : "复制设备码"}
                      </button>
                    ))}
                  </div>
                )}
                <button
                  className="secondary"
                  disabled={busy || login.phase === "cancelling"}
                  onClick={() =>
                    void perform(async () => {
                      await command("cancel_login");
                      setLogin({
                        ...login,
                        phase: "cancelling",
                        message: "正在取消登录…",
                      });
                    })
                  }
                >
                  取消登录
                </button>
              </>
            ) : (
              <>
                <button
                  className="primary"
                  disabled={busy}
                  onClick={() =>
                    void perform(async () =>
                      setLogin(
                        await command("start_login", { mode: "browser" }),
                      ),
                    )
                  }
                >
                  <LogIn size={16} />
                  使用浏览器登录
                </button>
                <button
                  className="text-button"
                  disabled={busy}
                  onClick={() =>
                    void perform(async () =>
                      setLogin(
                        await command("start_login", { mode: "device" }),
                      ),
                    )
                  }
                >
                  使用设备码
                  <ChevronRight size={15} />
                </button>
              </>
            )}
          </div>
        </>
      ) : (
        <>
          <div className="import-area">
            <Upload size={26} />
            <h3>导入已有 auth.json</h3>

            <button
              className="primary"
              disabled={busy}
              onClick={() =>
                void perform(async () => {
                  const s = await command<ViewState | null>("import_auth_file");
                  if (s) onDone(s);
                })
              }
            >
              <FolderOpen size={16} />
              选择凭据文件
            </button>
          </div>
          <button
            className="secondary full"
            disabled={busy}
            onClick={() =>
              void perform(async () => onDone(await command("import_current")))
            }
          >
            从当前 Codex 目录导入
          </button>
        </>
      )}
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
    </div>
  );
}
function SettingsForm({
  preferences,
  onThemePreview,
  onDone,
}: {
  preferences: Preferences;
  onThemePreview: (theme: Preferences["theme"] | null) => void;
  onDone: (s: ViewState) => void;
}) {
  const [prefs, setPrefs] = useState(preferences),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false),
    [checking, setChecking] = useState(false),
    [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [quotaInput, setQuotaInput] = useState(String(preferences.quotaRefreshSeconds ?? 60));
  useEffect(() => () => onThemePreview(null), [onThemePreview]);
  const checkForUpdates = () => {
    setChecking(true);
    setError("");
    void command<UpdateInfo>("check_for_updates")
      .then(setUpdate)
      .catch((e) => setError(errorOf(e).message))
      .finally(() => setChecking(false));
  };
  const openGitHub = () => {
    setError("");
    void command("open_github").catch((e) => setError(errorOf(e).message));
  };
  const openUpdate = () => {
    setError("");
    void command("open_update_release").catch((e) =>
      setError(errorOf(e).message),
    );
  };
  return (
    <form
      className="modal-content"
      noValidate
      onSubmit={(e) => {
        e.preventDefault();
        const quotaSeconds = prefs.quotaRefreshSeconds === 0 ? 0 : Number(quotaInput);
        if (quotaSeconds !== 0 && (!Number.isInteger(quotaSeconds) || quotaSeconds < 10 || quotaSeconds > 86400)) {
          setError("额度刷新间隔需为 10–86400 秒，或关闭自动刷新");
          return;
        }
        const nextPrefs = { ...prefs, quotaRefreshSeconds: quotaSeconds };
        setBusy(true);
        void command<ViewState>("set_preferences", { preferences: nextPrefs })
          .then(onDone)
          .catch((e) => setError(errorOf(e).message))
          .finally(() => setBusy(false));
      }}
    >
      <label className="field">外观</label>
      <div className="segments theme-options">
        {(["system", "dark", "light"] as const).map((t) => (
          <button
            key={t}
            type="button"
            className={prefs.theme === t ? "active" : ""}
            onClick={() => {
              setPrefs({ ...prefs, theme: t });
              onThemePreview(t);
            }}
          >
            {t === "system" ? (
              <Monitor size={16} />
            ) : t === "dark" ? (
              <Moon size={16} />
            ) : (
              <Sun size={16} />
            )}
            {{ system: "跟随系统", dark: "深色", light: "浅色" }[t]}
          </button>
        ))}
      </div>
      <div className="preference-rows">
        <label className="setting-row"><span>额度自动刷新</span><input type="checkbox" checked={(prefs.quotaRefreshSeconds ?? 60) !== 0} onChange={(e) => {
          const enabled = e.target.checked;
          const candidate = Number(quotaInput);
          const next = enabled && Number.isInteger(candidate) && candidate >= 10 && candidate <= 86400 ? candidate : enabled ? 60 : 0;
          if (enabled && next !== candidate) setQuotaInput(String(next));
          setPrefs({ ...prefs, quotaRefreshSeconds: next });
        }} /></label>
        {(prefs.quotaRefreshSeconds ?? 60) !== 0 && <label className="setting-row"><span>间隔 / 秒</span><input aria-label="额度刷新间隔" type="number" min={10} max={86400} step={1} value={quotaInput} onChange={(e) => {
          const value = e.target.value;
          setQuotaInput(value);
          if (value !== "") setPrefs({ ...prefs, quotaRefreshSeconds: Number(value) });
        }} /></label>}
        <label className="setting-row"><span>系统提醒</span><input type="checkbox" checked={prefs.systemNotifications ?? true} onChange={(e) => setPrefs({...prefs, systemNotifications:e.target.checked})} /></label>
        <NotificationPermission />
      </div>
      <StartupSettings />
      <PowerSettings />
      <LinkSettings />
      <div className="settings-update-row">
        <button
          type="button"
          className="secondary"
          disabled={checking}
          onClick={checkForUpdates}
        >
          <RefreshCw size={15} className={checking ? "quota-spin" : undefined} />
          检查更新
        </button>
        <button type="button" className="secondary" onClick={openGitHub}>
          <Github size={15} />
          GitHub
        </button>
        {update?.hasUpdate && (
          <button type="button" className="text-button" onClick={openUpdate}>
            更新 v{update.latestVersion}
          </button>
        )}
        {update && !update.hasUpdate && (
          <span className="settings-update-status" role="status">
            已是最新版本
          </span>
        )}
      </div>
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <button className="primary full" disabled={busy}>
        保存设置
      </button>
    </form>
  );
}
function AccountEdit({
  dialog,
  onDone,
}: {
  dialog: { action: "rename" | "delete"; account: Account };
  onDone: (s: ViewState) => void;
}) {
  const [name, setName] = useState(dialog.account.name),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false);
  return (
    <form
      className="modal-content"
      onSubmit={(e) => {
        e.preventDefault();
        setBusy(true);
        void command<ViewState>(
          dialog.action === "rename" ? "rename_account" : "delete_account",
          { id: dialog.account.id, name },
        )
          .then(onDone)
          .catch((e) => setError(errorOf(e).message))
          .finally(() => setBusy(false));
      }}
    >
      {dialog.action === "rename" ? (
        <label className="field">
          账号名称
          <input
            autoFocus
            required
            maxLength={100}
            value={name}
            onChange={(e) => setName(e.target.value)}
          />
        </label>
      ) : (
        <p className="delete-copy">
          从列表删除「{dialog.account.name}」？当前 Codex 登录文件会保留。
        </p>
      )}
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <button
        className={
          dialog.action === "delete" ? "danger-button full" : "primary full"
        }
        disabled={busy}
      >
        {dialog.action === "rename" ? "保存名称" : "删除账号"}
      </button>
    </form>
  );
}
