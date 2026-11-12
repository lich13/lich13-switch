import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import {
  AlertTriangle,
  ArrowLeftRight,
  Check,
  FolderOpen,
  KeyRound,
  LoaderCircle,
  MoreHorizontal,
  Pencil,
  Plus,
  RefreshCw,
  Shield,
  Trash2,
  UserRound,
} from "lucide-react";
import { command } from "./bridge";
import { subscriptions } from "./subscriptions";
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
  | { type: "import"; item?: AntigravityAccountItem }
  | { type: "rename"; item: AntigravityAccountItem }
  | { type: "delete"; item: AntigravityAccountItem };

function parseAdcPreview(raw: string): {
  valid: boolean;
  project: string | null;
  hint: string | null;
} {
  if (!raw.trim()) return { valid: false, project: null, hint: null };
  try {
    const obj = JSON.parse(raw) as Record<string, unknown>;
    if (!obj || typeof obj !== "object" || Array.isArray(obj)) {
      return { valid: false, project: null, hint: "顶层必须是 JSON 对象" };
    }
    const field = (key: string) =>
      typeof obj[key] === "string" ? obj[key].trim() : "";
    const project = field("quota_project_id") || field("project_id") || null;
    let hint: string | null = null;
    if (field("type") === "service_account") {
      if (
        !field("private_key").includes("PRIVATE KEY") ||
        !field("client_email") ||
        !project
      ) {
        hint = "service_account 需包含 private_key、client_email 与 project_id";
      }
    } else if (field("type") === "authorized_user") {
      if (
        !field("client_id") ||
        !field("client_secret") ||
        !field("refresh_token")
      ) {
        hint =
          "authorized_user 需包含 client_id、client_secret 与 refresh_token";
      }
    } else {
      hint = "仅支持 service_account 或 authorized_user 类型";
    }
    return { valid: hint === null, project, hint };
  } catch {
    return { valid: false, project: null, hint: "JSON 语法无效" };
  }
}

export default function AntigravityAccounts({ notify, setError }: Props) {
  const [state, setState] = useState<AntigravityAuthState | null>(null);
  const [busy, setBusy] = useState(false);
  const [tab, setTab] = useState<"all" | "oauth" | "adc" | "vertex_key">("all");
  const [dialog, setDialog] = useState<DialogState>(null);

  const [name, setName] = useState("");
  const [jsonText, setJsonText] = useState("");
  const [quotaProjectId, setQuotaProjectId] = useState("");
  const [activate, setActivate] = useState(true);
  const sequence = useRef(0);

  const [importType, setImportType] = useState<"adc" | "vertex_key">("adc");
  const [apiKey, setApiKey] = useState("");
  const [projectId, setProjectId] = useState("");
  const [location, setLocation] = useState("global");
  const [models, setModels] = useState("");
  const [dialogError, setDialogError] = useState("");
  const [dialogRevision, setDialogRevision] = useState("");
  const closeDialog = () => {
    setDialog(null);
    setApiKey("");
    setJsonText("");
    setDialogError("");
  };

  const adcPreview = useMemo(() => parseAdcPreview(jsonText), [jsonText]);

  const load = useCallback(async () => {
    const id = ++sequence.current;
    try {
      const next = await command("get_antigravity_auth_state");
      if (id === sequence.current) setState(next);
    } catch (e) {
      if (id === sequence.current) setError(errorOf(e).message);
    }
  }, [setError]);

  useEffect(() => {
    void load();
    const events = subscriptions((e) => setError(errorOf(e).message));
    events.watch("antigravity-auth-state", (next) => {
      sequence.current++;
      setState(next);
    });
    return () => {
      sequence.current++;
      events.dispose();
    };
  }, [load, setError]);

  const runAction = async (
    fn: () => Promise<AntigravityAuthState>,
    formatNotice: (next: AntigravityAuthState) => string,
  ) => {
    setBusy(true);
    const id = ++sequence.current;
    setError("");
    setDialogError("");
    try {
      const next = await fn();
      if (id === sequence.current) setState(next);
      const msg = formatNotice(next);
      if (next.relaunchFailed)
        notify(`${msg}；自动重启 Antigravity 失败，请手动打开应用`);
      else if (msg) notify(msg);
      closeDialog();
    } catch (e) {
      if (dialog) setDialogError(errorOf(e).message);
      else setError(errorOf(e).message);
      void load();
    } finally {
      setBusy(false);
    }
  };

  const [candidates, setCandidates] = useState<
    { id: string; preview: boolean }[]
  >([]);
  const [catalogError, setCatalogError] = useState("");
  const [catalogBusy, setCatalogBusy] = useState(false);
  const catalogRequest = useRef(0);
  const loadModels = useCallback(async (refresh: boolean) => {
    const request = ++catalogRequest.current;
    setCatalogBusy(true);
    setCatalogError("");
    try {
      const next = await command("get_antigravity_vertex_models", { refresh });
      if (request === catalogRequest.current) {
        setCandidates(next.models);
        setCatalogError(next.error ?? "");
      }
    } catch (e) {
      if (request === catalogRequest.current)
        setCatalogError(errorOf(e).message);
    } finally {
      if (request === catalogRequest.current) setCatalogBusy(false);
    }
  }, []);
  const vertexOpen = dialog?.type === "import" && importType === "vertex_key";
  useEffect(() => {
    if (vertexOpen) void loadModels(false);
    return () => {
      catalogRequest.current++;
    };
  }, [vertexOpen, loadModels]);

  const openImport = (item?: AntigravityAccountItem) => {
    setName(item?.name ?? "");
    setJsonText("");
    setQuotaProjectId("");
    setApiKey("");
    setProjectId(item?.projectId ?? "");
    setLocation(item?.vertex?.location ?? "global");
    setModels((item?.vertex?.models ?? []).join("\n"));
    setImportType(item ? "vertex_key" : "adc");
    setActivate(item ? item.current : true);
    setDialogError("");
    setDialogRevision(state?.revision ?? "");
    setDialog({ type: "import", item });
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
  const oauthCount = (state?.accounts ?? []).filter(
    (a) => a.kind === "oauth",
  ).length;
  const adcCount = (state?.accounts ?? []).filter(
    (a) => a.kind === "adc",
  ).length;
  const vertexCount = (state?.accounts ?? []).filter(
    (a) => a.kind === "vertex_key",
  ).length;
  const isMixed =
    state?.mode === "mixed_adc_file_without_flag" ||
    state?.mode === "mixed_flag_without_adc_file";

  return (
    <div className="antigravity-accounts">
      <div
        className={"ag-status-card" + (isMixed ? " warning" : "")}
        aria-label="Antigravity 认证状态"
      >
        <div className="ag-status-main">
          <div className="ag-status-header">
            {isMixed ? <AlertTriangle size={16} /> : <Shield size={16} />}
            <strong>
              当前模式：
              {state?.mode === "vertex_key"
                ? "Vertex API Key"
                : state?.mode === "adc"
                  ? "ADC 模式（已启用 ADC）"
                  : state?.mode === "mixed_adc_file_without_flag"
                    ? "不一致（存在 ADC 文件但未启用设置）"
                    : state?.mode === "mixed_flag_without_adc_file"
                      ? "不一致（已启用设置但缺少 ADC 文件）"
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
                  ? "已禁用"
                  : state?.adcStatus === "backup_only"
                    ? "仅备份 (.vela.bak)"
                    : "未检测到"}
            </span>
          </div>
        </div>
        <div className="ag-status-actions">
          <div
            className="exchange-button-group"
            role="group"
            aria-label="切换认证模式"
          >
            <button
              type="button"
              className={
                "exchange-button" + (state?.mode === "oauth" ? " active" : "")
              }
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
              className={
                "exchange-button" + (state?.mode === "adc" ? " active" : "")
              }
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
        <div
          className="exchange-button-group"
          role="tablist"
          aria-label="凭据类型筛选"
        >
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
          <button
            role="tab"
            aria-selected={tab === "vertex_key"}
            className={
              "exchange-button" + (tab === "vertex_key" ? " active" : "")
            }
            onClick={() => setTab("vertex_key")}
          >
            Vertex Key ({vertexCount})
          </button>
        </div>
        <div className="account-actions">
          <button
            type="button"
            className="primary"
            disabled={busy}
            onClick={() => openImport()}
          >
            <Plus size={15} />
            导入账号
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
          </div>
        ) : (
          items.map((item) => {
            const isEffective =
              item.current &&
              ((item.kind === "oauth" &&
                state.mode === "oauth" &&
                state.oauthPresent) ||
                (item.kind === "adc" &&
                  state.mode === "adc" &&
                  state.adcStatus === "active") ||
                (item.kind === "vertex_key" && state.mode === "vertex_key"));
            return (
              <div
                key={item.id}
                className={"account-row" + (isEffective ? " selected" : "")}
              >
                <div
                  className={
                    "avatar " + (item.kind === "oauth" ? "chatgpt" : "apikey")
                  }
                >
                  {item.kind === "oauth" ? (
                    <UserRound size={20} />
                  ) : (
                    <KeyRound size={20} />
                  )}
                </div>
                <div className="account-copy">
                  <div className="account-title">
                    <strong title={item.name}>{item.name}</strong>
                    <span className="plan-badge">
                      {item.kind === "oauth"
                        ? `OAuth · ${item.sourceType}`
                        : item.kind === "vertex_key"
                          ? "Vertex API Key"
                          : `ADC · ${item.sourceType}`}
                    </span>
                  </div>
                  <div className="ag-account-meta">
                    {item.email && <span title={item.email}>{item.email}</span>}
                    {item.projectId && (
                      <span title={item.projectId}>项目: {item.projectId}</span>
                    )}
                    {item.kind === "vertex_key" ? (
                      <span>{item.vertex?.location ?? "global"}</span>
                    ) : (
                      !item.email &&
                      !item.projectId && <span>{item.sourceType}</span>
                    )}
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
                            command("switch_antigravity_account", {
                              id: item.id,
                              expectedRevision: state.revision,
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
                      {item.kind === "vertex_key" && (
                        <button
                          type="button"
                          onClick={(e) => {
                            e.currentTarget
                              .closest("details")
                              ?.removeAttribute("open");
                            openImport(item);
                          }}
                        >
                          <KeyRound size={14} />
                          编辑凭据
                        </button>
                      )}
                      <button
                        type="button"
                        onClick={(e) => {
                          e.currentTarget
                            .closest("details")
                            ?.removeAttribute("open");
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
                          e.currentTarget
                            .closest("details")
                            ?.removeAttribute("open");
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
            dialog.type === "import"
              ? dialog.item
                ? "编辑 Vertex API Key"
                : "导入账号"
              : dialog.type === "rename"
                ? "重命名凭据"
                : "删除凭据"
          }
          onClose={closeDialog}
        >
          {dialog.type === "import" ? (
            <form
              className="modal-content ag-account-form ag-adc-form"
              onSubmit={(e) => {
                e.preventDefault();
                void runAction(
                  () =>
                    importType === "vertex_key"
                      ? command("save_antigravity_vertex_key", {
                          input: {
                            id: dialog.item?.id ?? null,
                            name,
                            apiKey,
                            projectId: projectId.trim() || null,
                            location,
                            models: models
                              .split(/[,\n]/)
                              .map((m) => m.trim())
                              .filter(Boolean),
                            activate,
                            expectedRevision: dialogRevision,
                          },
                        })
                      : command("add_antigravity_adc", {
                          input: {
                            name: name.trim() || null,
                            jsonText,
                            quotaProjectId: quotaProjectId.trim() || null,
                            activate,
                          },
                        }),
                  (next) =>
                    next.appRestarted
                      ? "账号已保存，Antigravity 已重启"
                      : "账号已保存",
                );
              }}
            >
              {!dialog.item && (
                <div
                  className="exchange-button-group ag-import-tabs"
                  role="tablist"
                  aria-label="导入类型"
                >
                  <button
                    type="button"
                    role="tab"
                    aria-selected={importType === "adc"}
                    className={
                      "exchange-button" +
                      (importType === "adc" ? " active" : "")
                    }
                    onClick={() => {
                      setImportType("adc");
                      setApiKey("");
                      setDialogError("");
                    }}
                  >
                    ADC JSON
                  </button>
                  <button
                    type="button"
                    role="tab"
                    aria-selected={importType === "vertex_key"}
                    className={
                      "exchange-button" +
                      (importType === "vertex_key" ? " active" : "")
                    }
                    onClick={() => {
                      setImportType("vertex_key");
                      setJsonText("");
                      setDialogError("");
                    }}
                  >
                    Vertex API Key
                  </button>
                </div>
              )}
              <label className="field">
                名称
                <input value={name} onChange={(e) => setName(e.target.value)} />
              </label>
              {importType === "vertex_key" ? (
                <>
                  <label className="field">
                    API Key
                    <input
                      type="password"
                      autoComplete="off"
                      spellCheck={false}
                      required={!dialog.item}
                      value={apiKey}
                      onChange={(e) => setApiKey(e.target.value)}
                      placeholder={dialog.item ? "留空保留现有密钥" : ""}
                    />
                  </label>
                  <div className="ag-key-location">
                    <label className="field">
                      Project ID（可选）
                      <input
                        value={projectId}
                        onChange={(e) => setProjectId(e.target.value)}
                        spellCheck={false}
                      />
                    </label>
                    <label className="field">
                      地区
                      <input
                        required
                        value={location}
                        onChange={(e) => setLocation(e.target.value)}
                        spellCheck={false}
                      />
                    </label>
                  </div>
                  <div className="ag-model-catalog">
                    <div className="ag-json-header">
                      <span>模型目录</span>
                      <button
                        type="button"
                        disabled={catalogBusy}
                        onClick={() => void loadModels(true)}
                      >
                        <RefreshCw size={14} />
                        {catalogBusy ? "正在加载…" : "刷新模型"}
                      </button>
                    </div>
                    <select
                      aria-label="添加目录模型"
                      value=""
                      disabled={catalogBusy || !candidates.length}
                      onChange={(e) => {
                        const selected = models
                          .split(/[,\n]/)
                          .map((m) => m.trim())
                          .filter(Boolean);
                        if (
                          e.target.value &&
                          !selected.includes(e.target.value)
                        )
                          setModels([...selected, e.target.value].join("\n"));
                      }}
                    >
                      <option value="">选择模型</option>
                      {candidates.map((m) => (
                        <option key={m.id} value={m.id}>
                          {m.id}
                          {m.preview ? " · 预览" : ""}
                        </option>
                      ))}
                    </select>
                    {catalogError && (
                      <div className="form-error" role="alert">
                        {catalogError}
                      </div>
                    )}
                  </div>
                  <label className="field">
                    模型
                    <textarea
                      required
                      rows={3}
                      value={models}
                      onChange={(e) => setModels(e.target.value)}
                      spellCheck={false}
                    />
                  </label>
                </>
              ) : (
                <>
                  <div className="ag-json-header">
                    <span>ADC JSON</span>
                    <button
                      type="button"
                      className="text-button"
                      onClick={() => void pickFile()}
                    >
                      <FolderOpen size={14} />
                      选择文件
                    </button>
                  </div>
                  <textarea
                    rows={6}
                    required
                    aria-label="凭据 JSON 内容"
                    value={jsonText}
                    onChange={(e) => setJsonText(e.target.value)}
                    spellCheck={false}
                  />
                  {jsonText.trim() !== "" && !adcPreview.valid && (
                    <p className="ag-form-error" role="alert">
                      {adcPreview.hint}
                    </p>
                  )}
                  <label className="field">
                    配额项目 ID（可选）
                    <input
                      value={quotaProjectId}
                      onChange={(e) => setQuotaProjectId(e.target.value)}
                      placeholder={adcPreview.project ?? ""}
                    />
                  </label>
                </>
              )}
              <label className="ag-checkbox-row">
                <input
                  type="checkbox"
                  checked={activate}
                  disabled={dialog.item?.current}
                  onChange={(e) => setActivate(e.target.checked)}
                />
                立即启用
              </label>
              {dialogError && (
                <p className="ag-form-error" role="alert">
                  {dialogError}
                </p>
              )}
              <div className="modal-actions">
                <button type="button" onClick={closeDialog}>
                  取消
                </button>
                <button
                  type="submit"
                  className="primary"
                  disabled={
                    busy ||
                    (importType === "adc"
                      ? !adcPreview.valid
                      : !models.split(/[,\n]/).some((m) => m.trim()))
                  }
                >
                  {busy ? "保存中…" : "保存"}
                </button>
              </div>
            </form>
          ) : dialog.type === "rename" ? (
            <form
              className="modal-content ag-account-form"
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
              <label className="field">
                新名称
                <input
                  required
                  value={name}
                  onChange={(e) => setName(e.target.value)}
                />
              </label>
              {dialogError && (
                <p className="ag-form-error" role="alert">
                  {dialogError}
                </p>
              )}
              <div className="modal-actions">
                <button type="button" onClick={closeDialog}>
                  取消
                </button>
                <button type="submit" className="primary" disabled={busy}>
                  保存
                </button>
              </div>
            </form>
          ) : (
            <div className="modal-content ag-account-form">
              <p>
                确定要从多账号库中删除「{dialog.item.name}
                」吗？正在使用中的凭据无法直接删除。
              </p>
              {dialogError && (
                <p className="ag-form-error" role="alert">
                  {dialogError}
                </p>
              )}
              <div className="modal-actions">
                <button type="button" onClick={closeDialog}>
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
