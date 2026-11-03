import { useCallback, useEffect, useMemo, useState } from "react";
import {
  AlertTriangle,
  ArrowLeftRight,
  Check,
  Download,
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
  | { type: "add-adc" }
  | { type: "rename"; item: AntigravityAccountItem }
  | { type: "delete"; item: AntigravityAccountItem };

function parseAdcPreview(raw: string): {
  valid: boolean;
  type: string | null;
  identity: string | null;
  project: string | null;
  hint: string | null;
} {
  const trimmed = raw.trim();
  if (!trimmed) {
    return { valid: false, type: null, identity: null, project: null, hint: null };
  }
  try {
    const obj = JSON.parse(trimmed) as Record<string, unknown>;
    if (!obj || typeof obj !== "object" || Array.isArray(obj)) {
      return { valid: false, type: null, identity: null, project: null, hint: "顶层必须是 JSON 对象" };
    }
    const type = typeof obj.type === "string" ? obj.type.trim() : "";
    const project =
      (typeof obj.quota_project_id === "string" && obj.quota_project_id.trim()) ||
      (typeof obj.project_id === "string" && obj.project_id.trim()) ||
      null;
    if (type === "service_account") {
      const email = typeof obj.client_email === "string" ? obj.client_email.trim() : "";
      const pk = typeof obj.private_key === "string" ? obj.private_key.trim() : "";
      const uri = typeof obj.token_uri === "string" ? obj.token_uri.trim() : "";
      if (!pk.includes("PRIVATE KEY") || !email || !uri) {
        return {
          valid: false,
          type,
          identity: email || null,
          project,
          hint: "service_account 需包含 private_key、client_email 与 token_uri",
        };
      }
      return { valid: true, type, identity: email, project, hint: null };
    }
    if (type === "authorized_user") {
      const cid = typeof obj.client_id === "string" ? obj.client_id.trim() : "";
      const csec = typeof obj.client_secret === "string" ? obj.client_secret.trim() : "";
      const rt = typeof obj.refresh_token === "string" ? obj.refresh_token.trim() : "";
      if (!cid || !csec || !rt) {
        return {
          valid: false,
          type,
          identity: cid || null,
          project,
          hint: "authorized_user 需包含 client_id、client_secret 与 refresh_token",
        };
      }
      return { valid: true, type, identity: cid, project, hint: null };
    }
    return {
      valid: false,
      type: type || null,
      identity: null,
      project,
      hint: "仅支持 service_account 或 authorized_user 类型",
    };
  } catch {
    return { valid: false, type: null, identity: null, project: null, hint: "JSON 语法无效" };
  }
}

export default function AntigravityAccounts({ notify, setError }: Props) {
  const [state, setState] = useState<AntigravityAuthState | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"all" | "oauth" | "adc">("all");
  const [dialog, setDialog] = useState<DialogState>(null);

  const [name, setName] = useState("");
  const [jsonText, setJsonText] = useState("");
  const [quotaProjectId, setQuotaProjectId] = useState("");
  const [activate, setActivate] = useState(true);

  const adcPreview = useMemo(() => parseAdcPreview(jsonText), [jsonText]);

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

  const runAction = async (
    fn: () => Promise<AntigravityAuthState>,
    formatNotice: (next: AntigravityAuthState) => string,
  ) => {
    setBusy(true);
    setError("");
    try {
      const next = await fn();
      setState(next);
      const msg = formatNotice(next);
      if (msg) notify(msg);
      setDialog(null);
    } catch (e) {
      setError(errorOf(e).message);
      void load();
    } finally {
      setBusy(false);
    }
  };

  const openAddAdc = () => {
    setName("");
    setJsonText("");
    setQuotaProjectId("");
    setActivate(true);
    setDialog({ type: "add-adc" });
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
      <div
        className={"ag-status-card" + (state?.mode === "mixed" ? " warning" : "")}
        aria-label="Antigravity 认证状态"
      >
        <div className="ag-status-main">
          <div className="ag-status-header">
            {state?.mode === "mixed" ? <AlertTriangle size={16} /> : <Shield size={16} />}
            <strong>
              当前模式：
              {state?.mode === "adc"
                ? "ADC 模式（已启用 ADC）"
                : state?.mode === "mixed"
                  ? "混合状态（配置与 ADC 文件不一致）"
                  : "OAuth 模式（已禁用 ADC）"}
            </strong>
          </div>
          <div className="ag-status-pills">
            <span className="ag-pill">
              OAuth:{" "}
              {state?.oauthPresent
                ? state.oauthEmail
                  ? `${state.oauthEmail}${state.oauthAuthMethod ? ` (${state.oauthAuthMethod})` : ""}`
                  : "已登录"
                : "未检测到"}
            </span>
            <span className="ag-pill">
              ADC:{" "}
              {state?.adcStatus === "active"
                ? `已启用 (${state.adcType ?? "ADC"}${state.adcProject ? ` · ${state.adcProject}` : ""})`
                : state?.adcStatus === "disabled"
                  ? "已禁用 (.disabled)"
                  : state?.adcStatus === "backup_only"
                    ? "仅备份 (agswitch)"
                    : "未检测到"}
            </span>
            <span className="ag-pill">自动备份: {state?.backupCount ?? 0}/10</span>
          </div>
        </div>
        <div className="ag-status-actions">
          <div className="exchange-button-group" role="group" aria-label="切换认证模式">
            <button
              type="button"
              className={"exchange-button" + (state?.mode === "oauth" ? " active" : "")}
              disabled={!state || busy || state.mode === "oauth"}
              onClick={() =>
                state &&
                void runAction(
                  () =>
                    command("set_antigravity_auth_mode", {
                      mode: "oauth",
                      expectedRevision: state.revision,
                    }),
                  (next) =>
                    next.appRestarted
                      ? "已切换到 OAuth 模式并自动重启 Antigravity"
                      : "已切换到 OAuth 模式（禁用 ADC）",
                )
              }
            >
              OAuth 模式
            </button>
            <button
              type="button"
              className={"exchange-button" + (state?.mode === "adc" ? " active" : "")}
              disabled={!state || busy || state.mode === "adc"}
              onClick={() =>
                state &&
                void runAction(
                  () =>
                    command("set_antigravity_auth_mode", {
                      mode: "adc",
                      expectedRevision: state.revision,
                    }),
                  (next) =>
                    next.appRestarted
                      ? "已切换到 ADC 模式并自动重启 Antigravity"
                      : "已切换到 ADC 模式（启用 ADC）",
                )
              }
            >
              ADC 模式
            </button>
          </div>
          <button
            type="button"
            className="text-button"
            disabled={!state || busy}
            onClick={() =>
              void runAction(
                () => command("backup_antigravity_auth"),
                () => "已备份当前 Antigravity 认证配置",
              )
            }
          >
            <Save size={14} />
            备份当前
          </button>
          <button
            type="button"
            className="icon-button"
            aria-label="刷新 Antigravity 认证状态"
            disabled={busy}
            onClick={() => void load()}
          >
            <RefreshCw size={14} />
          </button>
        </div>
      </div>

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
          <button
            type="button"
            className="secondary"
            disabled={busy || !state?.oauthPresent}
            onClick={() =>
              void runAction(
                () => command("import_current_antigravity_oauth", {}),
                () => "已从当前登录读取并保存 OAuth 账号",
              )
            }
          >
            <Download size={15} />
            读取当前 OAuth
          </button>
          {state && state.adcStatus !== "missing" && !state.adcSaved && (
            <button
              type="button"
              className="secondary"
              disabled={busy}
              onClick={() =>
                void runAction(
                  () => command("import_current_antigravity_adc", {}),
                  () => "已保存当前磁盘上的 ADC 凭据",
                )
              }
            >
              <Download size={15} />
              保存当前 ADC
            </button>
          )}
          <button type="button" className="primary" disabled={busy} onClick={openAddAdc}>
            <Plus size={15} />
            导入 ADC
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
            <p>可点击「读取当前 OAuth」保存当前登录账号，或「导入 ADC」添加 Google Cloud ADC JSON。</p>
          </div>
        ) : (
          items.map((item) => {
            const isEffective =
              item.current &&
              ((item.kind === "oauth" && state.mode === "oauth" && state.oauthPresent) ||
                (item.kind === "adc" && state.mode === "adc" && state.adcStatus === "active"));
            return (
              <div
                key={item.id}
                className={"account-row" + (isEffective ? " selected" : "")}
              >
                <div className={"avatar " + (item.kind === "oauth" ? "chatgpt" : "apikey")}>
                  {item.kind === "oauth" ? <UserRound size={20} /> : <KeyRound size={20} />}
                </div>
                <div className="account-copy">
                  <div className="account-title">
                    <strong title={item.name}>{item.name}</strong>
                    <span className="plan-badge">
                      {item.kind === "oauth"
                        ? `OAuth · ${item.sourceType}`
                        : `ADC · ${item.sourceType}`}
                    </span>
                  </div>
                  <div className="ag-account-meta">
                    {item.email && <span title={item.email}>{item.email}</span>}
                    {item.projectId && <span title={item.projectId}>项目: {item.projectId}</span>}
                    {!item.email && !item.projectId && <span>{item.sourceType}</span>}
                  </div>
                </div>
                <div className="account-actions">
                  {isEffective ? (
                    <span className="current-badge">
                      <Check size={13} />
                      当前生效
                    </span>
                  ) : (
                    <button
                      type="button"
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
                                  disableAdc: true,
                                })
                              : command("switch_antigravity_adc", {
                                  id: item.id,
                                  expectedRevision: state.revision,
                                  enableAdc: true,
                                }),
                          (next) =>
                            next.appRestarted
                              ? `已切换到 ${item.name} 并自动重启 Antigravity`
                              : `已切换到 ${item.name}`,
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
                        type="button"
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
                        type="button"
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
            );
          })
        )}
      </div>

      {dialog && state && (
        <AppModal
          title={
            dialog.type === "add-adc"
              ? "导入 Google Cloud ADC 凭据"
              : dialog.type === "rename"
                ? "重命名凭据"
                : "删除凭据"
          }
          onClose={() => setDialog(null)}
        >
          {dialog.type === "add-adc" ? (
            <form
              className="modal-form ag-adc-form"
              onSubmit={(e) => {
                e.preventDefault();
                void runAction(
                  () =>
                    command("add_antigravity_adc", {
                      input: {
                        name: name.trim() || null,
                        jsonText,
                        quotaProjectId: quotaProjectId.trim() || null,
                        activate,
                      },
                    }),
                  (next) =>
                    activate
                      ? next.appRestarted
                        ? "已导入并启用 ADC 凭据，Antigravity 已自动重启"
                        : "已导入并启用 ADC 凭据"
                      : "已导入 ADC 凭据",
                );
              }}
            >
              <div className="ag-adc-guide">
                <p>
                  支持导入 <code>gcloud auth application-default login</code> 生成的{" "}
                  <code>authorized_user</code> JSON，或包含完整{" "}
                  <code>private_key / client_email / token_uri</code> 的{" "}
                  <code>service_account</code> 密钥文件。
                </p>
                {state.adcStatus !== "missing" && (
                  <button
                    type="button"
                    className="secondary"
                    disabled={busy}
                    onClick={() =>
                      void runAction(
                        () =>
                          command("import_current_antigravity_adc", {
                            name: name.trim() || null,
                          }),
                        () => "已导入当前磁盘上的 ADC 凭据",
                      )
                    }
                  >
                    <Download size={14} />
                    直接导入本机当前 ADC ({state.adcProject ?? state.adcType ?? "已检测"})
                  </button>
                )}
              </div>

              <label>
                备注名称（可选）
                <input
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                  placeholder="例如：Vertex AI 生产项目 ADC"
                />
              </label>

              <div className="ag-json-header">
                <span>ADC JSON 内容</span>
                <button type="button" className="text-button" onClick={() => void pickFile()}>
                  <FolderOpen size={14} />
                  选择 JSON 文件…
                </button>
              </div>
              <textarea
                rows={6}
                required
                aria-label="凭据 JSON 内容"
                value={jsonText}
                onChange={(e) => setJsonText(e.target.value)}
                placeholder='{"type":"authorized_user","client_id":"...","client_secret":"...","refresh_token":"...","quota_project_id":"..."}'
              />

              {jsonText.trim() !== "" && (
                <div className={"ag-json-preview" + (adcPreview.valid ? " ok" : " invalid")}>
                  {adcPreview.valid ? (
                    <>
                      <span>类型: {adcPreview.type}</span>
                      {adcPreview.identity && <span>标识: {adcPreview.identity}</span>}
                      <span>项目: {quotaProjectId.trim() || adcPreview.project || "未指定"}</span>
                    </>
                  ) : (
                    <span>{adcPreview.hint}</span>
                  )}
                </div>
              )}

              <label>
                配额项目 ID 覆盖（quota_project_id，可选）
                <input
                  value={quotaProjectId}
                  onChange={(e) => setQuotaProjectId(e.target.value)}
                  placeholder={adcPreview.project ?? "例如：my-gcp-project"}
                />
              </label>

              <label className="ag-checkbox-row">
                <input
                  type="checkbox"
                  checked={activate}
                  onChange={(e) => setActivate(e.target.checked)}
                />
                导入后立即切换为当前生效 ADC（若 Antigravity 正在运行将自动重启）
              </label>

              <div className="modal-actions">
                <button type="button" onClick={() => setDialog(null)}>
                  取消
                </button>
                <button type="submit" className="primary" disabled={busy || !adcPreview.valid}>
                  导入保存
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
                  () => "已重命名凭据",
                );
              }}
            >
              <label>
                新名称
                <input required value={name} onChange={(e) => setName(e.target.value)} />
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
                      () => "已删除凭据",
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
