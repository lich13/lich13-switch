import {
  useEffect,
  useRef,
  useState,
  type Dispatch,
  type ReactNode,
  type SetStateAction,
} from "react";
import {
  ArrowLeftRight,
  Check,
  ChevronRight,
  Copy,
  ExternalLink,
  FileInput,
  FolderOpen,
  Github,
  LoaderCircle,
  LogIn,
  Monitor,
  Moon,
  RefreshCw,
  Sun,
  UserRound,
  X,
} from "lucide-react";
import { command } from "./bridge";
import NotificationPermission from "./NotificationPermission";
import PowerSettings from "./PowerSettings";
import QuickPlatformSettings from "./QuickPlatformSettings";
import StartupSettings from "./StartupSettings";
import {
  errorOf,
  type Account,
  type LoginState,
  type Preferences,
  type UpdateInfo,
  type ViewState,
} from "./types";

export function AppModal({
  title,
  onClose,
  children,
}: {
  title: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const ref = useRef<HTMLDialogElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  useEffect(() => {
    const previous = document.activeElement as HTMLElement | null;
    const fallback = previous?.closest("details")?.querySelector("summary");
    const dialog = ref.current;
    dialog?.showModal();
    if (dialog && !dialog.contains(document.activeElement)) {
      heading.current?.focus({ preventScroll: true });
    }
    return () => {
      dialog?.close();
      if (fallback instanceof HTMLElement && fallback.isConnected)
        fallback.focus();
      else if (previous?.isConnected) previous.focus();
    };
  }, []);
  return (
    <dialog
      ref={ref}
      className="modal"
      aria-label={title}
      onKeyDown={(e) => {
        if (e.key !== "Tab") return;
        const controls = Array.from(
          ref.current?.querySelectorAll<HTMLElement>(
            'button:not([disabled]), input:not([disabled]), select:not([disabled]), textarea:not([disabled]), a[href], [tabindex="0"]',
          ) ?? [],
        ).filter((element) => element.getClientRects().length > 0);
        const first = controls[0],
          last = controls.at(-1);
        if (!first || !last) {
          e.preventDefault();
          heading.current?.focus();
        } else if (
          e.shiftKey &&
          (document.activeElement === first ||
            document.activeElement === heading.current)
        ) {
          e.preventDefault();
          last.focus();
        } else if (!e.shiftKey && document.activeElement === last) {
          e.preventDefault();
          first.focus();
        }
      }}
      onCancel={(e) => {
        e.preventDefault();
        onClose();
      }}
      onClick={(e) => {
        if (e.target === e.currentTarget) onClose();
      }}
    >
      <div className="modal-heading">
        <h2 ref={heading} tabIndex={-1}>
          {title}
        </h2>
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

export function AddAccount({
  onImport,
  target,
  login,
  setLogin,
  onDone,
}: {
  onImport: () => void;
  target?: Account;
  login: LoginState;
  setLogin: Dispatch<SetStateAction<LoginState>>;
  onDone: (s: ViewState) => void;
}) {
  const [tab, setTab] = useState<"login" | "import">("login"),
    [error, setError] = useState(""),
    [busy, setBusy] = useState(false);
  const running = ["starting", "waiting", "cancelling"].includes(login.phase);
  const started = useRef(false);
  const startDevice = async () =>
    setLogin(
      await command("start_login", {
        mode: "device",
        ...(target
          ? {
              targetAccountId: target.id,
              expectedCredentialRevision: target.credentialRevision,
            }
          : {}),
      }),
    );
  useEffect(() => {
    if (!target || started.current) return;
    started.current = true;
    setBusy(true);
    void startDevice()
      .catch((e) => setError(errorOf(e).message))
      .finally(() => setBusy(false));
  }, []);
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
  const canCopy =
    login.mode === "device" && ["starting", "waiting"].includes(login.phase);
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
      {!target && (
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
      )}
      {tab === "login" ? (
        <>
          <div className="login-illustration">
            <img src="/icon.svg" alt="" />
            <ArrowLeftRight size={20} />
            <div>
              <UserRound size={24} />
            </div>
          </div>
          <h3 className="center">
            {target ? target.name : "连接你的 ChatGPT 账号"}
          </h3>
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
                  const next = await command("complete_login_callback", {
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
              <button
                className="secondary"
                disabled={busy || !callbackDraft.trim()}
              >
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
                        onClick={() =>
                          void perform(async () => {
                            setCopied(null);
                            await command("copy_login_value", { kind });
                            setCopied(kind);
                          })
                        }
                      >
                        {copied === kind ? (
                          <Check size={15} />
                        ) : (
                          <Copy size={15} />
                        )}
                        {copied === kind
                          ? "已复制"
                          : kind === "url"
                            ? "复制链接"
                            : "复制设备码"}
                      </button>
                    ))}
                  </div>
                )}
                <button
                  className="secondary"
                  disabled={busy || login.phase === "cancelling"}
                  onClick={() =>
                    void perform(async () => {
                      const cancelling: LoginState = {
                        ...login,
                        phase: "cancelling",
                        message: "正在取消登录…",
                      };
                      setLogin(cancelling);
                      try {
                        await command("cancel_login");
                      } catch (error) {
                        setLogin((current) =>
                          current === cancelling ? login : current,
                        );
                        throw error;
                      }
                    })
                  }
                >
                  取消登录
                </button>
              </>
            ) : (
              <>
                {!target && (
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
                )}
                <button
                  className="text-button"
                  disabled={busy}
                  onClick={() => void perform(startDevice)}
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
            <FileInput size={26} />
            <h3>导入 Codex、Sub2API 或 CLIProxyAPI</h3>
            <button className="primary" disabled={busy} onClick={onImport}>
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

export function SettingsForm({
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
  useEffect(() => () => onThemePreview(null), [onThemePreview]);
  const checkForUpdates = () => {
    setChecking(true);
    setError("");
    void command("check_for_updates")
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
        setBusy(true);
        void command("set_preferences", { preferences: prefs })
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
        <label className="setting-row">
          <span>系统提醒</span>
          <input
            type="checkbox"
            checked={prefs.systemNotifications ?? true}
            onChange={(e) =>
              setPrefs({ ...prefs, systemNotifications: e.target.checked })
            }
          />
        </label>
        <NotificationPermission />
      </div>
      <QuickPlatformSettings />
      <StartupSettings />
      {error && (
        <p className="form-error" role="alert">
          {error}
        </p>
      )}
      <button className="primary full" disabled={busy}>
        保存设置
      </button>
      <PowerSettings />
      <div className="settings-update-row">
        <button
          type="button"
          className="secondary"
          disabled={checking}
          onClick={checkForUpdates}
        >
          <RefreshCw
            size={15}
            className={checking ? "quota-spin" : undefined}
          />
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
    </form>
  );
}

export function AccountEdit({
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
        void command(
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
