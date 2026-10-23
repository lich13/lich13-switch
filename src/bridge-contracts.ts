// IPC contracts mirror the registered Rust commands. Credentials only cross
// the explicit reveal/login commands and are never added to shared state.
import type {
  AppError,
  ClientId,
  CurrentConnection,
  DshProfileCurrent,
  ConfigDocument,
  GatewaySettings,
  GatewayState,
  GatewayListener,
  GrokModel,
  GrokProtocol,
  LoginState,
  ModelCatalog,
  ModelMetadata,
  Preferences,
  ProviderQuota,
  StartupState,
  UpdateInfo,
  ViewState,
} from "./types";
import type {
  Dashboard,
  ModelsDevState,
  InterruptionSummary,
  PriceComparison,
  PricingConfig,
  PricingView,
  UsageFilter,
  UsagePage,
  UsageRecord,
  UsageSettings,
  UsageState,
} from "./usage-types";
import type { EventRecord, EventPage } from "./EventLog";
import type { PowerState } from "./PowerSettings";
import type { PreparedTemplate } from "./TemplateChoice";

export type { QuotaCheck, LimitEvidence } from "./limit-labels";

export type DshOfficialModel = {
  id: string;
  name: string;
  contextWindow?: number;
  maxTokens?: number;
  input?: string[];
};
export type DshOfficialCatalog = {
  models: DshOfficialModel[];
  error: string | null;
};

export type QuickPreferences = {
  pinned: boolean;
  tab: "providers" | "accounts";
  visible?: boolean;
};
export type NotificationState = {
  permission: string;
  error: string | null;
  delivery?: "idle" | "pending" | "accepted" | "failed";
};
export type ClaudeProfileState = {
  mode: "official" | "api";
  revision: string;
  initialized: boolean;
  conflict: string | null;
  files: { role: string; exists: boolean; revision: string }[];
  warnings: string[];
};
export type ClaudeLoginState = {
  phase: string;
  authenticated: boolean;
  error: string | null;
};
export type LinkHandlers = {
  current: string | null;
  apps: { id: string; name: string; path: string }[];
  systemPicker: boolean;
};
export type PendingImport = {
  clientId?: ClientId;
  id: string;
  name: string;
  baseUrl: string;
};
export type AccountImportPreview = {
  token: string;
  revision: string;
  rows: {
    id: number;
    name: string;
    format: string;
    identityHint: string | null;
    duplicate: boolean;
    error: string | null;
  }[];
};
export type AccountExportPreview = {
  revision: string;
  rows: { id: string; name: string; error: string | null }[];
};
export type ProviderImportPreview = {
  token: string;
  revision: string;
  unsupportedProxies: number;
  template?: PreparedTemplate | null;
  rows: {
    id: number;
    name: string;
    baseUrl: string | null;
    concurrency: number | null;
    error: string | null;
  }[];
};
export type DshOptions = {
  profile: string;
  protocol: GrokProtocol;
  model: string | null;
  makeDefault: boolean;
};
export type DshPreview = {
  profiles: string[];
  profile: string | null;
  revision: string;
  models: string[];
  attached: boolean;
  protocol: GrokProtocol;
  model: string | null;
  makeDefault: boolean;
  warning: string | null;
  current: CurrentConnection | null;
  states: DshProfileCurrent[];
  providers: {
    profile: string;
    name: string;
    models: string[];
    error: string | null;
  }[];
};
export type GrokImportPreview = {
  layers?: string[];
  modelSource?: string | null;
  unknown?: string[];
  token: string | null;
  revision: string;
  models: GrokModel[];
  selected: string;
  environmentAvailable: boolean;
  environmentLoaded: boolean;
  clearEnvironment: string[];
  error: string | null;
};
export type GatewayModels = {
  clientId: ClientId;
  passthrough: boolean;
  hint: string;
  models: {
    id: string;
    providers: { id: string; name: string }[];
    manual: boolean;
    source: string | null;
    pinned: string | null;
    metadata: ModelMetadata | null;
    price: {
      source: string;
      inputPerM: string | null;
      outputPerM: string | null;
      cacheReadPerM: string | null;
      cacheWritePerM: string | null;
      deviation: boolean;
    } | null;
  }[];
  providers: {
    id: string;
    name: string;
    count: number;
    checkedAt: number | null;
    stale: boolean;
    error: string | null;
  }[];
  modelsDev: ModelsDevState;
  error: string | null;
};
export type ModelSearch = {
  hits: {
    source: string;
    provider: string;
    providerName: string;
    model: string;
    name: string | null;
    context: number | null;
    output: number | null;
    cost: Record<string, unknown> | null;
  }[];
  error: string | null;
};
export type DshFileKey =
  | "profilePatch"
  | "homePatch"
  | "settings"
  | "environment"
  | "credentials";
export type DshFileView = {
  profiles: string[];
  profile: string | null;
  key: DshFileKey;
  files: DshFileKey[];
  text: string;
  revision: string;
  readOnly: boolean;
  canRestore: boolean;
};
export type DshBatch = {
  state: GatewayState;
  results: {
    profile: string;
    status: "success" | "failed" | "pending";
    error: string | null;
  }[];
};
export type GatewayEdit =
  | {
      op: "saveProvider";
      id?: string | null;
      baseUrl: string;
      token: string;
      quotaAccessToken?: string;
      name?: string | null;
    }
  | {
      op: "saveGrokProvider" | "saveDshProvider";
      id?: string | null;
      baseUrl: string;
      token: string;
      quotaAccessToken?: string;
      name?: string | null;
      protocol: GrokProtocol;
    }
  | { op: "import" }
  | {
      op: "dshModelsProvider";
      id: string;
      mapping: Record<string, string | null>;
    }
  | { op: "renameProvider"; id: string; name: string }
  | { op: "endpointsProvider"; id: string; baseUrls: string[] }
  | { op: "swapEndpoint"; id: string; baseUrl: string }
  | { op: "deleteProvider" | "select" | "reset"; id: string }
  | { op: "queueProvider"; id: string; queued: boolean }
  | { op: "concurrencyProvider"; id: string; maxConcurrency: number }
  | { op: "rpmProvider"; id: string; maxRpm: number }
  | { op: "modelsProvider"; id: string; allowedModels: string[] | null }
  | {
      op: "policyProvider";
      id: string;
      allowedModels: string[] | null;
      handoffAfterCompaction?: boolean;
      takeNewThreads?: boolean;
    }
  | {
      op: "takeoverProvider";
      id: string;
      handoffAfterCompaction?: boolean;
      takeNewThreads?: boolean;
    }
  | { op: "reorder"; ids: string[] }
  | { op: "mode" | "connection"; mode: string }
  | { op: "settings"; settings: GatewaySettings };
type Call<A, R> = { args: A; result: R };
type None = Record<string, never>;
type Client = { clientId: ClientId };
type Provider = Client & { providerId: string };
type Revision = { expectedRevision: string };
export type CommandMap = {
  get_state: Call<None, ViewState>;
  switch_account: Call<{ id: string } & Revision, ViewState>;
  use_official_account: Call<
    {
      accountId: string;
      expectedAuthRevision: string;
      expectedConfigRevision: string;
    },
    ViewState
  >;
  disable_official_account: Call<{ expectedConfigRevision: string }, ViewState>;
  import_current: Call<None, ViewState>;
  import_auth_file: Call<None, ViewState | null>;
  rename_account: Call<{ id: string; name: string }, ViewState>;
  delete_account: Call<{ id: string }, ViewState>;
  read_config: Call<{ clientId?: ClientId }, ConfigDocument>;
  read_previous_config: Call<Client, string>;
  validate_config: Call<{ clientId?: ClientId; text: string }, void>;
  save_config: Call<
    { clientId?: ClientId; text: string } & Revision,
    ConfigDocument
  >;
  set_preferences: Call<{ preferences: Preferences }, ViewState>;
  set_quota_refresh: Call<
    { seconds: number; expectedSeconds: number },
    ViewState
  >;
  get_startup: Call<None, StartupState>;
  set_startup: Call<{ enabled: boolean } & Revision, StartupState>;
  pick_path: Call<{ kind: string }, string | null>;
  get_login: Call<None, LoginState>;
  start_login: Call<
    {
      mode: "browser" | "device";
      targetAccountId?: string | null;
      expectedCredentialRevision?: string | null;
    },
    LoginState
  >;
  cancel_login: Call<None, void>;
  open_login_url: Call<None, void>;
  copy_login_value: Call<{ kind: "url" | "code" }, void>;
  complete_login_callback: Call<{ callbackUrl: string }, LoginState>;
  check_for_updates: Call<None, UpdateInfo>;
  open_github: Call<None, void>;
  open_update_release: Call<None, void>;
  frontend_ready: Call<None, void>;
  open_main: Call<
    {
      page?: string | null;
      clientId?: ClientId;
      providerId?: string | null;
      eventId?: string | null;
    },
    void
  >;
  get_quick: Call<None, QuickPreferences>;
  set_quick: Call<
    { pinned?: boolean; tab?: "providers" | "accounts" },
    QuickPreferences
  >;
  hide_quick: Call<None, void>;
  resize_quick: Call<{ height: number }, void>;
  get_gateway: Call<Client, GatewayState>;
  get_gateway_listener: Call<None, GatewayListener>;
  set_gateway_listener: Call<{ port: number } & Revision, GatewayListener>;
  get_gateway_models: Call<Client, GatewayModels>;
  refresh_gateway_models: Call<
    Client & { providerIds?: string[] | null },
    GatewayModels
  >;
  search_models_dev: Call<{ query: string; limit?: number }, ModelSearch>;
  edit_model_list: Call<
    Client & {
      op: "add" | "remove" | "pin" | "unpin";
      id: string;
      source?: string | null;
    },
    GatewayModels
  >;
  dsh_official_models: Call<{ refresh: boolean }, DshOfficialCatalog>;
  read_dsh_file: Call<{ profile: string | null; key: DshFileKey }, DshFileView>;
  read_previous_dsh_file: Call<
    { profile: string | null; key: DshFileKey },
    string
  >;
  save_dsh_file: Call<
    { profile: string | null; key: DshFileKey; text: string } & Revision,
    DshFileView
  >;
  restore_dsh_file: Call<
    { profile: string | null; key: DshFileKey } & Revision,
    DshFileView
  >;
  attach_dsh_profiles: Call<
    { options: DshOptions; all: boolean; previewRevision: string } & Revision,
    DshBatch
  >;
  preview_dsh_profile: Call<
    { profile?: string | null; protocol: GrokProtocol },
    DshPreview
  >;
  attach_dsh_profile: Call<
    { options: DshOptions; previewRevision: string } & Revision,
    GatewayState
  >;
  preview_grok_import: Call<
    { selected?: string | null; useEnvironment: boolean },
    GrokImportPreview
  >;
  commit_grok_import: Call<{ token: string } & Revision, GatewayState>;
  cancel_grok_import: Call<{ token?: string | null }, void>;
  reveal_provider_token: Call<Provider & Revision, string>;
  probe_provider_endpoints: Call<Provider, GatewayState>;
  update_gateway: Call<
    Client &
      Revision & { edit: GatewayEdit; expectedConfigRevision?: string | null },
    GatewayState
  >;
  start_gateway: Call<
    Client & Revision & { expectedConfigRevision?: string | null },
    GatewayState
  >;
  stop_gateway: Call<
    Client & { expectedConfigRevision?: string | null },
    GatewayState
  >;
  query_provider_quota: Call<Provider & { force: boolean }, ProviderQuota>;
  list_provider_models: Call<Provider & { force: boolean }, ModelCatalog>;
  get_clamshell_state: Call<None, PowerState>;
  set_clamshell_awake: Call<{ enabled: boolean } & Revision, PowerState>;
  install_power_helper: Call<None, PowerState>;
  remove_power_helper: Call<None, PowerState>;
  force_quit_agent_clients: Call<
    None,
    { terminated: number; failed: number; terminalCleanupFailed?: number }
  >;
  get_startup_error: Call<None, AppError | null>;
  cleanup_retired_data: Call<None, void>;
  get_app_events: Call<
    {
      filter: {
        from?: number | null;
        to?: number | null;
        clientId?: ClientId | null;
        providerId?: string | null;
        level?: EventRecord["level"] | null;
        category?: EventRecord["category"] | null;
        reason?: EventRecord["reason"] | null;
        statusGroup?:
          | "success"
          | "client_error"
          | "server_error"
          | "no_status"
          | null;
        page?: number;
      };
    },
    EventPage
  >;
  get_app_event: Call<{ id: string }, EventRecord | null>;
  clear_app_events: Call<None, void>;
  preview_sub2api_import: Call<Client, ProviderImportPreview | null>;
  commit_sub2api_import: Call<
    Client & Revision & { token: string; ids: number[] },
    GatewayState
  >;
  cancel_sub2api_import: Call<Client & { token: string }, void>;
  prepare_sub2api_export: Call<
    Client & { replaceTemplate?: boolean },
    PreparedTemplate | null
  >;
  select_sub2api_template: Call<Client & { token: string; id: number }, void>;
  export_sub2api_providers: Call<
    Client & Revision & { ids: string[] },
    boolean
  >;
  preview_account_import: Call<None, AccountImportPreview | null>;
  commit_account_import: Call<
    Revision & { token: string; selected: { id: number; update: boolean }[] },
    ViewState
  >;
  cancel_account_exchange: Call<{ token: string }, void>;
  preview_account_export: Call<None, AccountExportPreview>;
  prepare_account_template: Call<
    { ids: string[]; replaceTemplate?: boolean },
    PreparedTemplate | null
  >;
  select_account_template: Call<{ token: string; id: number }, void>;
  export_accounts: Call<Revision & { ids: string[]; format: string }, boolean>;
  get_link_handler_state: Call<None, LinkHandlers>;
  set_link_handler: Call<{ appId: string }, LinkHandlers>;
  get_provider_imports: Call<None, PendingImport[]>;
  confirm_provider_import: Call<{ id: string } & Revision, GatewayState>;
  cancel_provider_import: Call<{ id: string }, void>;
  get_claude_profile: Call<None, ClaudeProfileState>;
  switch_claude_profile: Call<
    Revision & { mode: "official" | "api" },
    ClaudeProfileState
  >;
  recover_claude_profile: Call<None, ClaudeProfileState>;
  claude_login_status: Call<None, ClaudeLoginState>;
  start_claude_login: Call<None, ClaudeLoginState>;
  cancel_claude_login: Call<None, void>;
  notification_permission: Call<{ request?: boolean }, NotificationState>;
  test_notification: Call<None, NotificationState>;
  open_notification_settings: Call<None, void>;
  get_usage_state: Call<None, UsageState>;
  get_usage_dashboard: Call<{ filter: UsageFilter }, Dashboard>;
  get_usage_logs: Call<{ filter: UsageFilter }, UsagePage>;
  get_usage_detail: Call<{ id: string; source?: string }, UsageRecord>;
  set_usage_settings: Call<{ settings: UsageSettings }, UsageState>;
  sync_usage: Call<{ rebuild?: string }, UsageState>;
  get_pricing: Call<None, PricingView>;
  get_models_dev: Call<None, ModelsDevState>;
  get_usage_interruptions: Call<{ filter: UsageFilter }, InterruptionSummary>;
  update_models_dev: Call<None, ModelsDevState>;
  compare_model_prices: Call<
    { model: string; context: number },
    PriceComparison
  >;
  configure_pricing: Call<Revision & { config: PricingConfig }, PricingView>;
  update_pricing: Call<None, PricingView>;
  reload_pricing: Call<None, PricingView>;
  open_pricing_directory: Call<None, void>;
};
export type CommandName = keyof CommandMap;
export type CommandArgs<K extends CommandName> = CommandMap[K]["args"];
export type CommandResult<K extends CommandName> = CommandMap[K]["result"];
export type CommandParams<K extends CommandName> =
  {} extends CommandArgs<K> ? [args?: CommandArgs<K>] : [args: CommandArgs<K>];
export type EventMap = {
  "switch-state": ViewState;
  "login-state": LoginState;
  "switch-error": AppError;
  "switch-notice": string;
  "gateway-state": GatewayState;
  "provider-quota": { clientId: ClientId; quota: ProviderQuota };
  "config-state": { clientId: ClientId; revision: string; guarded: boolean };
  "clamshell-state": PowerState;
  "notification-state": NotificationState;
  "claude-profile-state": ClaudeProfileState;
  "claude-login-state": ClaudeLoginState;
  "quick-state": QuickPreferences;
  "app-visibility": boolean;
  "panel-visibility": boolean;
  "provider-imports": null;
  "usage-state": null;
  "pricing-state": null;
  "app-event": EventRecord;
  "event-log": { id: string; clientId: ClientId; providerId: string | null };
  "provider-settings": string | { id: string; clientId: ClientId };
  navigate: string;
};
