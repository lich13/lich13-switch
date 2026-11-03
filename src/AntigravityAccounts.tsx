import { useCallback, useEffect, useState } from "react";
import {
  ArrowLeftRight,
  Check,
  FolderOpen,
  KeyRound,
  LoaderCircle,
  MoreHorizontal,
  Pencil,
  Plus,
  RefreshCw,
  Save,
  Shield,
  Trash2,
  UserRound,
} from "lucide-react";
import { command } from "./bridge";
import type {
  AntigravityAccountItem,
  AntigravityAuthState,
} from "./bridge-contracts";
import { AppModal } from "./AppDialogs";
import { errorOf } from "./types";

type Props = {
  notify: (msg: string) => void;
  setError: (msg: string) => void;
};

type DialogState =
  | null
  | { type: "add"; kind: "oauth" | "adc" }
  | { type: "rename"; item: AntigravityAccountItem }
  | { type: "delete"; item: AntigravityAccountItem };

export default function AntigravityAccounts({ notify, setError }: Props) {
  const [state, setState] = useState<AntigravityAuthState | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"all" | "oauth" | "adc">("all");
  const [dialog, setDialog] = useState<DialogState>(null);

  const [inputMode, setInputMode] = useState<"json" | "key">("json");
  const [name, setName] = useState("");
  const [jsonText, setJsonText] = useState("");
  const [keyVal, setKeyVal] = useState("");
  const [refreshToken, setRefreshToken] = useState("");
  const [email, setEmail] = useState("");
  const [projectId, setProjectId] = useState("");
  const [clientId, setClientId] = useState("");
  const [clientSecret, setClientSecret] = useState("");
  const [clientEmail, setClientEmail] = useState("");
  const [activate, setActivate] = useState(true);

  const load = useCallback(async () => {
    try {
      setState(await command("get_antigravity_auth_state"));
    } catch (e) {
      setError(errorOf(e).message);
    }
  }, [setError]);

  useEffect(() => {
    void load();
  }, [load]);

  const runAction = async (fn: () => Promise<AntigravityAuthState>, okMsg: string) => {
    setBusy(true);
    setError("");
    try {
      const next = await fn();
      setState(next);
      if (okMsg) notify(okMsg);
      setDialog(null);
    } catch (e) {
      setError(errorOf(e).message);
      void load();
    } finally {
      setBusy(false);
    }
  };

  const openAdd = (kind: "oauth" | "adc") => {
    setInputMode("json");
    setName("");
    setJsonText("");
    setKeyVal("");
    setRefreshToken("");
    setEmail("");
    setProjectId("");
    setClientId("");
    setClientSecret("");
    setClientEmail("");
    setActivate(true);
    setDialog({ type: "add", kind });
  };

  const pickFile = async () => {
    try {
      const content = await command("pick_antigravity_credential_file");
      if (content != null) setJsonText(content);
    } catch (e) {
      setError(errorOf(e).message);
    }
  };

  const items = (state?.accounts ?? []).filter((a) =>
    tab === "all" ? true : a.kind === tab,
  );
  const oauthCount = (state?.accounts ?? []).filter((a) => a.kind === "oauth").length;
  const adcCount = (state?.accounts ?? []).filter((a) => a.kind === "adc").length;

  return (
    <div className="antigravity-accounts">
      <div className="banner" aria-label="Antigravity 认证状态">
        <Shield size={16} />
        <div style={{ flex: 1 }}>
          <strong>
            当前模式：
            {state?.mode === "both"
              ? "混合模式（OAuth + ADC）"
              : state?.mode === "adc"
                ? "ADC 模式（已启用 ADC）"
                : "OAuth 模式（已禁用 ADC）"}
          </strong>
          <div className="muted" style={{ fontSize: 12, marginTop: 2 }}>
            enableAdc: {state?.enableAdc ? "true" : "false"} · OAuth:{" "}
            {state?.oauthPresent ? "已存在" : "未检测到"} · ADC:{" "}
            {state?.adcStatus === "active"
              ? `已启用 (${state.adcType ?? "ADC"}${state.adcProject ? ` / ${state.adcProject}` : ""})`
              : state?.adcStatus === "disabled"
                ? "已禁用 (.disabled)"
                : state?.adcStatus === "backup_only"
                  ? "仅备份"
                  : "未检测到"}{" "}
            · 自动备份: {state?.backupCount ?? 0}/10
          </div>
        </div>
        <div className="account-actions">
          <button
            className="text-button"
            disabled={!state || busy || state.mode === "oauth"}
            onClick={() =>
              state &&
              void runAction(
                () =>
                  command("set_antigravity_auth_mode", {
                    mode: "oauth",
                    expectedRevision: state.revision,
                  }),
                "已切换到 OAuth 模式（禁用 ADC），重启 Antigravity 后生效",
              )
            }
          >
            OAuth 模式
          </button>
          <button
            className="text-button"
            disabled={!state || busy || state.mode === "adc"}
            onClick={() =>
              state &&
              void runAction(
                () =>
                  command("set_antigravity_auth_mode", {
                    mode: "adc",
                    expectedRevision: state.revision,
                  }),
                "已切换到 ADC 模式（启用 ADC），重启 Antigravity 后生效",
              )
            }
          >
            ADC 模式
          </button>
          <button
            className="text-button"
            disabled={!state || busy || state.mode === "both"}
            onClick={() =>
              state &&
              void runAction(
                () =>
                  command("set_antigravity_auth_mode", {
                    mode: "both",
                    expectedRevision: state.revision,
                  }),
                "已启用混合模式（保留 OAuth + 启用 ADC），重启 Antigravity 后生效",
              )
            }
          >
            混合模式
          </button>
          <button
            className="text-button"
            disabled={!state || busy}
            onClick={() =>
              void runAction(
                () => command("backup_antigravity_auth"),
                "已备份当前 Antigravity 认证配置",
              )
            }
          >
            <Save size={14} />
            备份当前
          </button>
          <button
            className="icon-button"
            aria-label="刷新 Antigravity 认证状态"
            disabled={busy}
            onClick={() => void load()}
          >
            <RefreshCw size={14} />
          </button>
        </div>
      </div>

      {state && ((state.oauthPresent && !state.oauthSaved) || (state.adcStatus !== "missing" && !state.adcSaved)) && (
        <div className="banner">
          <span>检测到当前磁盘上的 Antigravity 凭据尚未保存到多账号库</span>
          <div className="account-actions">
            {state.oauthPresent && !state.oauthSaved && (
              <button
                className="text-button"
                disabled={busy}
                onClick={() =>
                  void runAction(
                    () => command("import_current_antigravity_oauth", {}),
                    "已保存当前 OAuth 账号",
                  )
                }
              >
                保存当前 OAuth
              </button>
            )}
            {state.adcStatus !== "missing" && !state.adcSaved && (
              <button
                className="text-button"
                disabled={busy}
                onClick={() =>
                  void runAction(
                    () => command("import_current_antigravity_adc", {}),
                    "已保存当前 ADC 凭据",
                  )
                }
              >
                保存当前 ADC
              </button>
            )}
          </div>
        </div>
      )}

      <div className="list-tools">
        <div className="exchange-button-group" role="tablist" aria-label="凭据类型筛选">
          <button
            role="tab"
            aria-selected={tab === "all"}
            className={"exchange-button" + (tab === "all" ? " active" : "")}
            onClick={() => setTab("all")}
          >
            全部 ({state?.accounts.length ?? 0})
          </button>
          <button
            role="tab"
            aria-selected={tab === "oauth"}
            className={"exchange-button" + (tab === "oauth" ? " active" : "")}
            onClick={() => setTab("oauth")}
          >
            OAuth 账号 ({oauthCount})
          </button>
          <button
            role="tab"
            aria-selected={tab === "adc"}
            className={"exchange-button" + (tab === "adc" ? " active" : "")}
            onClick={() => setTab("adc")}
          >
            ADC 凭据 ({adcCount})
          </button>
        </div>
        <div className="account-actions">
          <button className="secondary" disabled={busy} onClick={() => openAdd("oauth")}>
            <Plus size={15} />
            添加 OAuth
          </button>
          <button className="primary" disabled={busy} onClick={() => openAdd("adc")}>
            <Plus size={15} />
            添加 ADC
          </button>
        </div>
      </div>

      <div className="account-list" aria-label="Antigravity 凭据列表">
        {!state ? (
          <div className="empty">
            <LoaderCircle className="spin" />
            正在读取 Antigravity 凭据…
          </div>
        ) : items.length === 0 ? (
          <div className="empty">
            <KeyRound size={30} />
            <h3>尚无已保存的 Antigravity 凭据</h3>
            <p>支持导入 OAuth / ADC JSON 文件，或直接输入 Token / Key。</p>
          </div>
        ) : (
          items.map((item) => (
            <div
              key={item.id}
              className={"account-row" + (item.current ? " selected" : "")}
            >
              <div className={"avatar " + (item.kind === "oauth" ? "chatgpt" : "apikey")}>
                {item.kind === "oauth" ? <UserRound size={20} /> : <KeyRound size={20} />}
              </div>
              <div className="account-copy">
                <div className="account-title">
                  <strong title={item.name}>{item.name}</strong>
                  <span className="plan-badge">
                    {item.kind === "oauth" ? "OAuth" : `ADC · ${item.sourceType}`}
                  </span>
                </div>
                <div>
                  {item.email && <span title={item.email}>{item.email}</span>}
                  {item.projectId && <span title={item.projectId}>项目: {item.projectId}</span>}
                  {!item.email && !item.projectId && <span>{item.sourceType}</span>}
                </div>
              </div>
              <div className="account-actions">
                {item.current &&
                ((item.kind === "oauth" && state.oauthPresent) ||
                  (item.kind === "adc" && state.adcStatus === "active" && state.enableAdc)) ? (
                  <span className="current-badge">
                    <Check size={13} />
                    当前生效
                  </span>
                ) : (
                  <button
                    className="switch-button"
                    disabled={busy}
                    aria-label={"切换到 " + item.name}
                    onClick={() =>
                      void runAction(
                        () =>
                          item.kind === "oauth"
                            ? command("switch_antigravity_oauth", {
                                id: item.id,
                                expectedRevision: state.revision,
                                disableAdc: state.mode === "oauth",
                              })
                            : command("switch_antigravity_adc", {
                                id: item.id,
                                expectedRevision: state.revision,
                                enableAdc: true,
                              }),
                        `已切换到 ${item.name}，请完全退出并重开 Antigravity`,
                      )
                    }
                  >
                    切换
                    <ArrowLeftRight size={13} />
                  </button>
                )}
                <details className="account-menu">
                  <summary aria-label={"管理 " + item.name}>
                    <MoreHorizontal size={18} />
                  </summary>
                  <div className="menu-popover">
                    <button
                      onClick={(e) => {
                        e.currentTarget.closest("details")?.removeAttribute("open");
                        setName(item.name);
                        setDialog({ type: "rename", item });
                      }}
                    >
                      <Pencil size={14} />
                      重命名
                    </button>
                    <button
                      className="danger"
                      onClick={(e) => {
                        e.currentTarget.closest("details")?.removeAttribute("open");
                        setDialog({ type: "delete", item });
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

      {dialog && state && (
        <AppModal
          title={
            dialog.type === "add"
              ? dialog.kind === "oauth"
                ? "添加 Antigravity OAuth 账号"
                : "添加 Antigravity ADC 凭据"
              : dialog.type === "rename"
                ? "重命名凭据"
                : "删除凭据"
          }
          onClose={() => setDialog(null)}
        >
          {dialog.type === "add" ? (
            <form
              className="modal-form"
              onSubmit={(e) => {
                e.preventDefault();
                if (dialog.kind === "oauth") {
                  void runAction(
                    () =>
                      command("add_antigravity_oauth", {
                        input: {
                          name: name || null,
                          inputMode,
                          jsonText: inputMode === "json" ? jsonText : null,
                          key: inputMode === "key" ? keyVal : null,
                          refreshToken: inputMode === "key" ? refreshToken || null : null,
                          email: email || null,
                          activate,
                        },
                      }),
                    activate
                      ? "已添加并切换 OAuth 账号，请重启 Antigravity"
                      : "已添加 OAuth 账号",
                  );
                } else {
                  void runAction(
                    () =>
                      command("add_antigravity_adc", {
                        input: {
                          name: name || null,
                          inputMode,
                          jsonText: inputMode === "json" ? jsonText : null,
                          key: inputMode === "key" ? keyVal : null,
                          clientId: inputMode === "key" ? clientId || null : null,
                          clientSecret: inputMode === "key" ? clientSecret || null : null,
                          projectId: projectId || null,
                          clientEmail: inputMode === "key" ? clientEmail || null : null,
                          activate,
                        },
                      }),
                    activate
                      ? "已添加并启用 ADC 凭据，请重启 Antigravity"
                      : "已添加 ADC 凭据",
                  );
                }
              }}
            >
              <div className="exchange-button-group" role="tablist" aria-label="录入方式">
                <button
                  type="button"
                  role="tab"
                  aria-selected={inputMode === "json"}
                  className={"exchange-button" + (inputMode === "json" ? " active" : "")}
                  onClick={() => setInputMode("json")}
                >
                  导入 JSON
                </button>
                <button
                  type="button"
                  role="tab"
                  aria-selected={inputMode === "key"}
                  className={"exchange-button" + (inputMode === "key" ? " active" : "")}
                  onClick={() => setInputMode("key")}
                >
                  输入 Key
                </button>
              </div>

              <label>
                名称（可选）
                <input
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder={dialog.kind === "oauth" ? "例如：个人 OAuth" : "例如：GCP 生产 ADC"}
                />
              </label>

              {inputMode === "json" ? (
                <>
                  <div style={{ display: "flex", justifyContent: "space-between", alignItems: "center" }}>
                    <span>JSON 内容</span>
                    <button type="button" className="text-button" onClick={() => void pickFile()}>
                      <FolderOpen size={14} />
                      选择本地文件…
                    </button>
                  </div>
                  <textarea
                    rows={6}
                    aria-label="凭据 JSON 内容"
                    value={jsonText}
                    onChange={(e) => setJsonText(e.target.value)}
                    placeholder={
                      dialog.kind === "oauth"
                        ? '{"access_token":"...","refresh_token":"..."}'
                        : '{"type":"authorized_user","client_id":"...","refresh_token":"..."}'
                    }
                  />
                </>
              ) : dialog.kind === "oauth" ? (
                <>
                  <label>
                    OAuth Token / Key
                    <input
                      required
                      aria-label="OAuth Token 或 Key"
                      value={keyVal}
                      onChange={(e) => setKeyVal(e.target.value)}
                      placeholder="输入 jetski 独立令牌或 access_token"
                    />
                  </label>
                  <label>
                    Refresh Token（可选）
                    <input
                      value={refreshToken}
                      onChange={(e) => setRefreshToken(e.target.value)}
                      placeholder="1//..."
                    />
                  </label>
                  <label>
                    邮箱标识（可选）
                    <input
                      value={email}
                      onChange={(e) => setEmail(e.target.value)}
                      placeholder="user@example.invalid"
                    />
                  </label>
                </>
              ) : (
                <>
                  <label>
                    ADC Key（Refresh Token / Private Key / API Key）
                    <input
                      required
                      aria-label="ADC Key"
                      value={keyVal}
                      onChange={(e) => setKeyVal(e.target.value)}
                      placeholder="输入 Refresh Token、API Key 或 Private Key"
                    />
                  </label>
                  <label>
                    GCP 项目 ID（可选）
                    <input
                      value={projectId}
                      onChange={(e) => setProjectId(e.target.value)}
                      placeholder="my-gcp-project"
                    />
                  </label>
                  <label>
                    Client ID（可选）
                    <input
                      value={clientId}
                      onChange={(e) => setClientId(e.target.value)}
                      placeholder="xxx.apps.googleusercontent.com"
                    />
                  </label>
                  <label>
                    Client Secret（可选）
                    <input
                      type="password"
                      value={clientSecret}
                      onChange={(e) => setClientSecret(e.target.value)}
                      placeholder="GOCSPX-..."
                    />
                  </label>
                  <label>
                    Service Account 邮箱（可选）
                    <input
                      value={clientEmail}
                      onChange={(e) => setClientEmail(e.target.value)}
                      placeholder="sa@project.iam.gserviceaccount.com"
                    />
                  </label>
                </>
              )}

              <label className="checkbox-row" style={{ display: "flex", alignItems: "center", gap: 8 }}>
                <input
                  type="checkbox"
                  checked={activate}
                  onChange={(e) => setActivate(e.target.checked)}
                />
                添加后立即切换为当前生效凭据
              </label>

              <div className="modal-actions">
                <button type="button" onClick={() => setDialog(null)}>
                  取消
                </button>
                <button type="submit" className="primary" disabled={busy}>
                  保存
                </button>
              </div>
            </form>
          ) : dialog.type === "rename" ? (
            <form
              className="modal-form"
              onSubmit={(e) => {
                e.preventDefault();
                void runAction(
                  () =>
                    command("rename_antigravity_account", {
                      id: dialog.item.id,
                      name,
                      expectedRevision: state.revision,
                    }),
                  "已重命名凭据",
                );
              }}
            >
              <label>
                新名称
                <input
                  required
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                />
              </label>
              <div className="modal-actions">
                <button type="button" onClick={() => setDialog(null)}>
                  取消
                </button>
                <button type="submit" className="primary" disabled={busy}>
                  保存
                </button>
              </div>
            </form>
          ) : (
            <div className="modal-form">
              <p>
                确定要从多账号库中删除「{dialog.item.name}」吗？正在使用中的凭据无法直接删除。
              </p>
              <div className="modal-actions">
                <button type="button" onClick={() => setDialog(null)}>
                  取消
                </button>
                <button
                  type="button"
                  className="danger"
                  disabled={busy}
                  onClick={() =>
                    void runAction(
                      () =>
                        command("delete_antigravity_account", {
                          id: dialog.item.id,
                          expectedRevision: state.revision,
                        }),
                      "已删除凭据",
                    )
                  }
                >
                  删除
                </button>
              </div>
            </div>
          )}
        </AppModal>
      )}
    </div>
  );
}
