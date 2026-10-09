import { usageCommands, usagePreview } from "./usage-preview";
import { powerCommands, powerPreview } from "./power-preview";
import { gatewayPreview } from "./gateway-preview";
import type { ViewState, ConfigDocument, LoginState } from "./types";
import { densePreview } from "./preview-fixtures";
const callbacks = new Map<string, Set<(p: never) => void>>();
export function subscribe<T>(event: string, fn: (p: T) => void) {
  if (!callbacks.has(event)) callbacks.set(event, new Set());
  callbacks.get(event)!.add(fn as (p: never) => void);
  return () => {
    callbacks.get(event)?.delete(fn as (p: never) => void);
  };
}
function emit(event: string, data: unknown) {
  callbacks.get(event)?.forEach((fn) => fn(data as never));
}
export const demo: ViewState = {
  accounts: [
    {
      id: "personal",
      name: "日常开发",
      kind: "chatgpt",
      email: "personal@example.invalid",
      current: true,
      updatedAt: 0,
    },
    {
      id: "work",
      name: "工作空间",
      kind: "chatgpt",
      email: "work@example.invalid",
      current: false,
      updatedAt: 0,
    },
    {
      id: "api",
      name: "API 开发账号",
      kind: "apiKey",
      email: null,
      current: false,
      updatedAt: 0,
    },
  ],
  authRevision: "preview",
  configRevision: "preview",
  currentState: "saved",
  preferences: {
    claudeHome: "~/.claude",
    codexHome: "~/.codex",
    cliPath: "",
    theme: "system",
    quotaRefreshSeconds: 60,
    systemNotifications: true,
  },
  authSource: {
    provider: "openai",
    credentialStore: "file",
    inlineToken: false,
    envKey: false,
    commandAuth: false,
    requiresOpenaiAuth: true,
    warning: null,
  },
  error: null,
  officialMode: {
    enabled: false,
    state: "disabled",
    accountId: null,
    error: null,
  },
};
if (densePreview) {
  demo.accounts.push(...Array.from({ length: 12 }, (_, index) => ({
    id: `fixture-account-${index + 1}`,
    name: `团队工作空间 ${index + 1} · 长名称账号`,
    kind: "chatgpt" as const,
    email: null,
    current: false,
    updatedAt: 0,
  })));
}
let doc: ConfigDocument = {
  clientId: "codex",
  guarded: false,
  canRestore: false,
  text: '# Codex 全局配置\nmodel = "gpt-example"\nmodel_reasoning_effort = "high"\n\n# 保留你的注释与其他设置\n[features]\nweb_search_request = true\n',
  revision: "preview",
  path: "~/.codex/config.toml",
};
let claudeDoc: ConfigDocument = {
  clientId: "claude",
  guarded: false,
  canRestore: false,
  path: "~/.claude/settings.json",
  revision: "preview-claude-config",
  text:
    JSON.stringify(
      {
        model: "sonnet",
        env: {
          ANTHROPIC_BASE_URL: "https://claude.example.com",
          ANTHROPIC_AUTH_TOKEN: "fixture-only-token",
          CLAUDE_CODE_EFFORT_LEVEL: "high",
        },
        permissions: { defaultMode: "default", allow: ["Read"] },
        language: "简体中文",
        autoMemoryEnabled: true,
        enabledPlugins: { "example@official": true },
      },
      null,
      2,
    ) + "\n",
};
const previousConfig: Record<string, string> = {};
let login: LoginState = {
  phase: "idle",
  mode: "",
  url: null,
  code: null,
  message: "",
  callbackReady: false,
};
let startup = {
  launchOnBoot: false,
  restoreGateway: false,
  revision: "preview-startup",
};
let quick = { pinned: false, tab: "providers", visible: true };
let linkHandler = "com.lich13.studio";
const importPreview = new URLSearchParams(location.search).get("importPreview");
let imports = importPreview
  ? [
      {
        id: "preview-import",
        name:
          importPreview === "long"
            ? "研发团队使用的跨区域供应商与长名称展示验证".repeat(3)
            : "sub2api",
        baseUrl:
          importPreview === "long"
            ? `https://api.example.invalid/${"deployment/".repeat(12)}v1`
            : "https://api.example.invalid/team/v1",
      },
    ]
  : [];
const linkApps = [
  {
    id: "com.lich13.gpt-switch",
    name: "lich13-switch",
    path: "/Applications/lich13-switch.app",
  },
  {
    id: "com.lich13.studio",
    name: "lich13studio",
    path: "/Applications/lich13studio.app",
  },
];
let claudeProfile = {
  mode: "api",
  revision: "preview-profile",
  initialized: false,
  conflict: null,
  files: [],
  warnings: [],
};
let claudeLogin = { phase: "idle", authenticated: false, error: null };
let apiProfileText = claudeDoc.text;
let officialProfileText = '{\n  "env": {}\n}\n';
export async function run(
  name: string,
  args: Record<string, unknown>,
): Promise<unknown> {
  if (name === "get_claude_profile" || name === "recover_claude_profile")
    return structuredClone(claudeProfile);
  if (name === "switch_claude_profile") {
    const official = args.mode === "official";
    if (official) apiProfileText = claudeDoc.text;
    else officialProfileText = claudeDoc.text;
    claudeDoc = {
      ...claudeDoc,
      text: official ? officialProfileText : apiProfileText,
      guarded: official,
      revision: `preview-claude-${Date.now()}`,
    };
    claudeProfile = {
      ...claudeProfile,
      mode: official ? "official" : "api",
      initialized: true,
      revision: `preview-profile-${Date.now()}`,
    };
    emit("claude-profile-state", claudeProfile);
    emit("config-state", {
      clientId: "claude",
      revision: claudeDoc.revision,
      guarded: official,
    });
    return structuredClone(claudeProfile);
  }
  if (name === "claude_login_status") return structuredClone(claudeLogin);
  if (name === "start_claude_login" || name === "cancel_claude_login") {
    claudeLogin = {
      ...claudeLogin,
      phase: name === "start_claude_login" ? "waiting" : "cancelled",
    };
    emit("claude-login-state", claudeLogin);
    return structuredClone(claudeLogin);
  }

  if (name === "get_link_handler_state")
    return { current: linkHandler, apps: linkApps, systemPicker: false };
  if (name === "set_link_handler") {
    linkHandler = String(args.appId);
    return { current: linkHandler, apps: linkApps, systemPicker: false };
  }
  if (name === "get_provider_imports") return structuredClone(imports);
  if (name === "confirm_provider_import" || name === "cancel_provider_import") {
    if (name === "confirm_provider_import" && importPreview === "error")
      throw { code: "CONFLICT", message: "供应商列表已更新，请重新导入" };
    imports = imports.filter((item) => item.id !== args.id);
    emit("provider-imports", null);
    return;
  }
  if (name === "cleanup_retired_data") return;
  if (name === "check_for_updates")
    return {
      hasUpdate: false,
      currentVersion: "0.12.1",
      latestVersion: "0.12.1",
      releaseUrl: "https://github.com/lich13/lich13-switch/releases",
      asset: null,
    };
  if (name === "open_github" || name === "open_update_release") return;
  if (usageCommands.includes(name)) return usagePreview(name, args, emit);
  if (powerCommands.includes(name)) return powerPreview(name, args, emit);
  if (name === "get_quick") return { ...quick };
  if (name === "set_quick") {
    quick = { ...quick, ...args };
    emit("quick-state", { ...quick });
    return { ...quick };
  }
  if (name === "resize_quick" || name === "hide_quick") return;
  if (name === "open_main") {
    location.search = "";
    return;
  }
  if (name === "list_provider_models")
    return {
      providerId: args.providerId,
      version: "preview-quota",
      models: ["gpt-example", "gpt-example-mini", "gpt-example-pro"],
      checkedAt: Date.now() / 1000,
      stale: false,
      error: null,
      retryAt: null,
    };
  if (name === "get_startup") return { ...startup };
  if (name === "set_startup") {
    startup = {
      ...startup,
      ...(args.preferences as {
        restoreGateway: boolean;
      }),
      launchOnBoot: Boolean(args.enabled),
      revision: crypto.randomUUID(),
    };
    return { ...startup };
  }
  if (
    [
      "get_gateway",
      "start_gateway",
      "stop_gateway",
      "update_gateway",
      "query_provider_quota",
    ].includes(name)
  ) {
    const result = gatewayPreview(name, args);
    if (name === "query_provider_quota")
      emit("provider-quota", {
        clientId: args.clientId ?? "codex",
        quota: result,
      });
    else emit("gateway-state", result);
    return result;
  }
  switch (name) {
    case "notification_permission":
      return { permission: "granted", error: null, delivery: "idle" };
    case "test_notification":
      return { permission: "granted", error: null, delivery: "accepted" };
    case "open_notification_settings":
      return;
    case "get_app_events": {
      const filter = (args.filter ?? {}) as Record<string, unknown>;
      const rows = demoEvents.filter(
        (r) =>
          r.reason !== "recovered" &&
          Object.entries(filter).every(
            ([key, v]) =>
              !v ||
              key === "page" ||
              (key === "statusGroup"
                ? v === "no_status"
                  ? r.status == null
                  : r.status != null && Math.floor(r.status / 100) === ({ success: 2, client_error: 4, server_error: 5 } as Record<string, number>)[String(v)]
                : key === "from"
                ? r.lastAt >= Number(v)
                : key === "to"
                  ? r.firstAt <= Number(v)
                  : r[key as keyof typeof r] === v),
          ),
      );
      const page = Math.max(
        1,
        Math.min(Number(filter.page) || 1, Math.ceil(rows.length / 50) || 1),
      );
      return {
        items: rows.slice((page - 1) * 50, page * 50),
        total: rows.length,
        page,
        error: null,
      };
    }
    case "get_app_event":
      return demoEvents.find((r) => r.id === args.id) ?? null;
    case "clear_app_events":
      demoEvents = [];
      emit("app-event", {});
      return;

    case "get_state":
      return structuredClone(demo);
    case "frontend_ready":
      return;
    case "switch_account":
      demo.accounts.forEach((a) => (a.current = a.id === args.id));
      emit("switch-state", structuredClone(demo));
      return structuredClone(demo);
    case "use_official_account":
      demo.officialMode = {
        enabled: true,
        state: "enabled",
        accountId: String(args.accountId),
        error: null,
      };
      emit("switch-state", structuredClone(demo));
      return structuredClone(demo);
    case "disable_official_account":
      demo.officialMode = {
        enabled: false,
        state: "disabled",
        accountId: null,
        error: null,
      };
      emit("switch-state", structuredClone(demo));
      return structuredClone(demo);
    case "rename_account":
      demo.accounts.find((a) => a.id === args.id)!.name = String(args.name);
      break;
    case "delete_account":
      demo.accounts = demo.accounts.filter((a) => a.id !== args.id);
      break;
    case "read_config":
      return { ...(args.clientId === "claude" ? claudeDoc : doc) };
    case "read_previous_config":
      if (!previousConfig[String(args.clientId)])
        throw new Error("没有可恢复的配置");
      return previousConfig[String(args.clientId)];
    case "validate_config":
      if (args.clientId === "claude") {
        (await import("./claude-settings")).tree(String(args.text));
        return;
      }
      if (String(args.text).includes("INVALID"))
        throw { code: "TOML", message: "TOML 语法错误", line: 1, column: 1 };
      return;
    case "save_config": {
      await run("validate_config", args);
      const current = args.clientId === "claude" ? claudeDoc : doc;
      if (args.expectedRevision !== current.revision)
        throw { code: "CONFLICT", message: "配置已被外部修改，草稿已保留" };
      previousConfig[current.clientId] = current.text;
      const next = {
        ...current,
        text: String(args.text),
        revision: crypto.randomUUID(),
        canRestore: true,
      };
      if (args.clientId === "claude") claudeDoc = next;
      else {
        doc = next;
        demo.configRevision = doc.revision;
        emit("switch-state", structuredClone(demo));
      }
      emit("config-state", {
        clientId: next.clientId,
        revision: next.revision,
        guarded: next.guarded,
      });
      return { ...next };
    }
    case "set_quota_refresh":
      if (
        args.seconds !== 0 &&
        (!Number.isInteger(args.seconds) ||
          Number(args.seconds) < 10 ||
          Number(args.seconds) > 86400)
      )
        throw { code: "PREFERENCES", message: "刷新间隔需为 10–86400 秒" };
      if (demo.preferences.quotaRefreshSeconds !== args.expectedSeconds)
        throw new Error("额度刷新设置已变化，请重试");
      demo.preferences.quotaRefreshSeconds = args.seconds as number;
      emit("switch-state", structuredClone(demo));
      return structuredClone(demo);
    case "set_preferences":
      demo.preferences = {
        ...(args.preferences as ViewState["preferences"]),
        quotaRefreshSeconds: demo.preferences.quotaRefreshSeconds,
      };
      break;
    case "get_login":
      return login;
    case "start_login":
      login = {
        targetAccountId: args.targetAccountId as string | undefined,
        phase: "waiting",
        mode: String(args.mode),
        url: "https://auth.openai.com/codex/device",
        code: args.mode === "device" ? "DEMO-CODE" : null,
        message: "预览模式：在桌面客户端中完成真实登录",
        callbackReady: args.mode === "browser",
      };
      emit("login-state", login);
      return login;
    case "complete_login_callback":
      login = { ...login, message: "已提交，等待登录完成" };
      emit("login-state", login);
      return login;
    case "copy_login_value": {
      const value =
        args.kind === "url"
          ? login.url
          : args.kind === "code"
            ? login.code
            : null;
      if (
        login.mode !== "device" ||
        !["starting", "waiting"].includes(login.phase) ||
        !value
      )
        throw new Error("登录链接或设备码不可用");
      await navigator.clipboard.writeText(value);
      return;
    }
    case "cancel_login":
      login = {
        ...login,
        phase: "cancelled",
        url: null,
        code: null,
        callbackReady: false,
        message: "登录已取消",
      };
      emit("login-state", login);
      return;
    case "pick_path":
    case "import_auth_file":
      return null;
    case "import_current":
      break;
    default:
      return;
  }
  emit("switch-state", structuredClone(demo));
  return structuredClone(demo);
}

let demoEvents = [
  {
    id: "fixture-event-1",
    firstAt: Math.floor(Date.now() / 1000) - 180,
    lastAt: Math.floor(Date.now() / 1000) - 20,
    count: 3,
    clientId: "codex",
    providerId: "primary",
    model: "gpt-example",
    reason: "model_unavailable",
    action: "trying_next",
    level: "warning",
    status: 404,
    attempt: 1,
    details: {
      upstreamCode: "model_not_found",
      upstreamType: "invalid_request_error",
      parameter: "model",
      message: "指定模型不存在或当前渠道不支持",
      phase: "response",
      countedFailure: false,
    },
  },
  {
    id: "fixture-event-2",
    firstAt: Math.floor(Date.now() / 1000) - 600,
    lastAt: Math.floor(Date.now() / 1000) - 600,
    count: 1,
    clientId: "claude",
    providerId: null,
    model: null,
    reason: "config_conflict",
    action: "stopped",
    level: "error",
    status: null,
    attempt: null,
  },
  {
    id: "fixture-event-ws",
    firstAt: Math.floor(Date.now() / 1000) - 30,
    lastAt: Math.floor(Date.now() / 1000) - 30,
    count: 1,
    clientId: "codex",
    providerId: "primary",
    model: "gpt-example",
    reason: "network",
    action: "reconnecting",
    level: "warning",
    status: 101,
    attempt: 1,
    details: {
      phase: "ws_receive",
      wsCloseCode: 1011,
      message: "上游在生成完成前关闭连接",
      countedFailure: true,
      waitSeconds: 60,
    },
  },
  {
    id: "fixture-event-circuit",
    firstAt: Math.floor(Date.now() / 1000) - 120,
    lastAt: Math.floor(Date.now() / 1000) - 120,
    count: 1,
    clientId: "claude",
    providerId: "primary",
    model: "claude-example",
    reason: "circuit_open",
    action: "stopped",
    level: "error",
    status: 503,
    attempt: 4,
    details: {
      upstreamCode: "service_unavailable",
      phase: "headers",
      message: "上游服务暂时不可用",
      countedFailure: true,
      circuit: {
        failures: 4,
        failureThreshold: 4,
        failedRequests: 4,
        requests: 8,
        errorRate: 0.6,
        minRequests: 10,
        trigger: "consecutive_failures",
      },
    },
  },
];
