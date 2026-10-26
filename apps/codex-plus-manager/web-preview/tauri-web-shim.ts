type CommandResult = {
  status: string;
  message: string;
  [key: string]: unknown;
};

const ok = <T extends Record<string, unknown>>(payload: T = {} as T): CommandResult & T => ({
  status: "ok",
  message: "演示模式",
  ...payload,
});

const demoSettings = {
  revision: "web-preview",
  codexAppPath: "/Applications/Codex.app",
  codexAppAutoTakeover: false,
  codexAppAutoTakeoverWhenActive: "defer",
  codexExtraArgs: [],
  dictation: {
    enabled: false,
    provider: "api",
    baseUrl: "https://api.groq.com/openai/v1",
    apiKey: "",
    apiKeyEnv: "",
    model: "whisper-large-v3-turbo",
    language: "",
    timeoutSeconds: 120,
  },
  providerSyncEnabled: false,
  ccsDbPath: "",
  providerSyncSavedProviders: [],
  providerSyncManualProviders: [],
  providerSyncLastSelectedProvider: "",
  relayProfilesEnabled: true,
  localConfigProviderImported: false,
  enhancementsEnabled: true,
  codexAppStandaloneQuotaUnlock: false,
  codexAppPluginMarketplaceUnlock: true,
  codexAppModelWhitelistUnlock: true,
  codexAppSessionDelete: true,
  codexAppMarkdownExport: true,
  codexAppUsageStatusLine: true,
  codexAppPasteFix: false,
  codexAppForceChineseLocale: false,
  codexAppTypingEffect: "off",
  codexAppThreadIdBadge: false,
  codexAppConversationView: false,
  codexAppThreadScrollRestore: true,
  codexAppNativeMenuPlacement: true,
  codexAppNativeBrowserRequireIdentification: false,
  codexAppNativeFastUnlock: false,
  codexAppPetRealMouseLook: false,
  codexAppActivityOverlayRight: false,
  protocolProxyAutoRetry: true,
  protocolProxyAutoRetryMaxAttempts: 5,
  protocolProxyAutoRetryIntervalSeconds: 2,
  protocolProxyAutoRetryMode: "streaming",
  protocolProxyAutoRetryCommitDelayMs: 1000,
  protocolProxyMapUnlistedModels: false,
  helperMaxRequestBodyMib: 128,
  codexAppCapacityAutoRetry: false,
  codexAppStepwiseEnabled: false,
  codexAppAnswerOutlineEnabled: false,
  codexAppStepwiseDirectSend: false,
  codexAppStepwiseProtocol: "chat_completions",
  codexAppStepwiseGenerationMode: "auto",
  codexAppStepwiseBaseUrl: "",
  codexAppStepwiseApiKey: "",
  codexAppStepwiseApiKeyEnv: "CODEX_STEPWISE_API_KEY",
  codexAppStepwiseModel: "",
  codexAppStepwiseMaxItems: 4,
  codexAppStepwiseMaxInputChars: 6000,
  codexAppStepwiseMaxOutputTokens: 500,
  codexAppStepwiseTimeoutMs: 8000,
  codexAppImageOverlayEnabled: false,
  codexAppImageOverlayPath: "",
  codexAppImageOverlayOpacity: 35,
  codexAppImageOverlayFitMode: "fit",
  codexAppDreamSkinEnabled: false,
  codexAppDreamSkinPaused: false,
  codexAppDreamSkinTheme: "pink",
  codexAppDreamSkinThemeConfig: {},
  codexAppDreamSkinImagePath: "",
  codexGoalsEnabled: false,
  weixinConnectEnabled: false,
  weixinConnectBaseUrl: "https://ilinkai.weixin.qq.com",
  weixinConnectToken: "",
  weixinConnectAccountId: "",
  weixinConnectAllowFrom: "",
  weixinConnectRouteTag: "",
  weixinConnectWorkDir: "",
  weixinConnectModel: "",
  weixinConnectSandbox: "read-only",
  weixinConnectCodexPath: "",
  relayBaseUrl: "",
  relayApiKey: "",
  relayProfiles: [{
    id: "default",
    name: "默认中转",
    model: "deepseek-v4-pro[1M]",
    baseUrl: "https://api.deepseek.com/v1",
    upstreamBaseUrl: "https://api.deepseek.com/v1",
    apiKey: "",
    protocol: "responses",
    relayMode: "pureApi",
    officialMixApiKey: false,
    hideOfficialUsageAlert: false,
    testModel: "deepseek-v4-pro",
    configContents: "",
    authContents: "",
    useCommonConfig: true,
    contextSelection: { mcpServers: [], skills: [], plugins: [] },
    contextSelectionInitialized: true,
    contextWindow: "1000000",
    autoCompactLimit: "800000",
    modelList: "deepseek-v4-pro[1M]\ndeepseek-v4-flash[128K]",
    modelWindows: "",
    modelAutoCompact: "",
    modelMetadata: "",
    modelVlm: "",
    vlmApiKey: "",
    vlmModel: "",
    vlmBaseUrl: "",
    userAgent: "",
    customHeaders: [],
    sub2apiEnabled: false,
    noAuth: false,
    sub2apiMultiplier: "",
    standardOpenaiProtocol: false,
    webSearchHistoryCompat: false,
    strictResponsesReasoning: false,
    anthropicAuthHeader: "x-api-key",
    anthropicMaxTokens: 0,
    anthropicPromptCache: true,
    anthropicImpersonateClaudeCode: false,
    anthropicThinkingMode: "",
  }],
  relayCommonConfigContents: "",
  relayPreserveNonproviderSettings: false,
  relayContextConfigContents: "",
  activeRelayId: "default",
  aggregateRelayProfiles: [],
  activeAggregateRelayId: "",
  relayTestModel: "gpt-5.4-mini",
  tools: {},
  activeTool: "codex",
  codexAppWhaleWidgetEnabled: false,
  codexAppWhaleBalanceProtocol: "auto",
  codexAppWhaleBalancePath: "",
  codexAppWhaleBalanceField: "",
  codexAppWhaleBalanceCurrency: "USD",
  codexAppWhaleBalanceScale: 1,
};

const overview = ok({
  codex_app: { status: "found", path: "/Applications/Codex.app" },
  codex_version: "1.10.0",
  silent_shortcut: { status: "installed", path: null },
  management_shortcut: { status: "installed", path: null },
  latest_launch: null,
  runtime_health: {
    codex_app: { status: "stopped", address: null, message: "等待启动" },
    local_server: { status: "running", address: "127.0.0.1:63122", message: "本地服务已就绪" },
    debugger: { status: "disconnected", address: null, message: "等待 Codex 连接" },
    protocol_conversion_enabled: true,
  },
  current_version: "1.12.1",
  update_status: "up_to_date",
  settings_path: "演示模式",
  logs_path: "演示模式",
});

const tools = ok({
  tools: [
    { id: "codex", name: "Codex", homeDir: "~/.codex", switchable: true, active: true, activeRelayName: "默认中转", relayCount: 3 },
    { id: "grok", name: "Grok", homeDir: "~/.grok", switchable: false, active: false, activeRelayName: null, relayCount: 0 },
    { id: "claude", name: "Claude", homeDir: "~/.claude", switchable: false, active: false, activeRelayName: null, relayCount: 0 },
  ],
  activeTool: "codex",
});

// 网关演示数据：Claude Code 已接入、Grok 未接入，池里三家供应商，其中一家有 Key 在冷却。
const gatewayBinding = (model: string) => ({
  enabled: true,
  model,
  extraModels: [],
  claudeRoles: { opus: "", sonnet: "", haiku: "", subagent: "" },
  unknownModel: "mapToDefault",
  claudeEnableEffort: false,
  token: "demo",
});
const gateway = ok({
  gateway: { version: 1, bindings: { claude: gatewayBinding("deepseek-v4-pro") }, keyPolicy: { mode: "sticky" }, restoreOnExit: true },
  port: 63130,
  helperRunning: true,
  pool: [
    { id: "deepseek", name: "DeepSeek", baseUrl: "https://api.deepseek.com/v1", protocol: "chatCompletions", official: true, models: ["deepseek-v4-pro", "deepseek-v4-flash"], keyCount: 3 },
    { id: "kimi", name: "Kimi", baseUrl: "https://api.moonshot.cn/v1", protocol: "chatCompletions", official: true, models: ["kimi-k2-turbo"], keyCount: 2 },
    { id: "zhipu", name: "智谱 GLM", baseUrl: "https://open.bigmodel.cn/api/paas/v4", protocol: "chatCompletions", official: true, models: ["glm-5"], keyCount: 1 },
  ],
  groups: [],
  agents: [
    { agent: "claude", name: "Claude Code", enabled: true, baseUrl: "http://127.0.0.1:63130/claude", defaultModel: "deepseek-v4-pro", models: [] },
    { agent: "grok", name: "Grok", enabled: false, baseUrl: "http://127.0.0.1:63130/grok", defaultModel: "", models: [] },
  ],
  health: { keys: [{ relayId: "zhipu", keyId: "k1", state: "cooling", remainingMs: 42000 }], providers: [] },
  activity: {
    agents: [{
      agent: "claude",
      requests: 396,
      rerouted: 14,
      errors: 1,
      inFlight: [],
      last: {
        requestedModel: "claude-sonnet-4-5",
        model: "deepseek-v4-pro",
        planned: [{ relayId: "deepseek", relayName: "DeepSeek", model: "deepseek-v4-pro" }],
        steps: [{ relayId: "deepseek", relayName: "DeepSeek", keyId: "k1", model: "deepseek-v4-pro", outcome: "answered", elapsedMs: 1840 }],
        result: "answered",
        finishedAtMs: Date.now() - 8000,
        elapsedMs: 1840,
      },
    }],
  },
  codexProviders: [],
  claudeProviders: [],
  grokProfiles: [],
});
const usageCounts = (calls: number, errors = 0) => ({ calls, errors, inputTokens: calls * 9000, outputTokens: calls * 700, cachedTokens: calls * 5600, reasoningTokens: 0 });
const gatewayUsage = ok({
  totals: usageCounts(396, 5),
  days: [{ date: new Date().toISOString().slice(0, 10), ...usageCounts(396, 5) }],
  byAgent: [{ key: "claude", label: "claude", ...usageCounts(396, 5) }],
  byProvider: [{ key: "deepseek", label: "DeepSeek", ...usageCounts(281, 2) }, { key: "kimi", label: "Kimi", ...usageCounts(115, 3) }],
  byModel: [],
  costs: [{ currency: "CNY", amount: 12.84 }],
});

const settingsResult = () => ok({ revision: "web-preview", settings: demoSettings, settings_path: "演示模式", user_scripts: { enabled: true, skip_next_start: false, scripts: [] } });

export async function invoke<T = unknown>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  switch (command) {
    case "list_tools": return tools as T;
    case "load_gateway": return gateway as T;
    case "load_gateway_usage": return gatewayUsage as T;
    case "startup_options": return ok({ showUpdate: false }) as T;
    case "consume_pending_manager_navigation": return null as T;
    case "load_overview": return overview as T;
    case "load_ads": return ok({ version: 1, ads: [], topAds: [] }) as T;
    case "load_settings":
    case "save_settings":
    case "reset_settings":
    case "set_settings_path":
    case "reset_settings_path":
    case "refresh_user_script_inventory":
      if (command === "save_settings" && args.settings) return ok({ revision: "web-preview", settings: args.settings, settings_path: "演示模式", user_scripts: { enabled: true, skip_next_start: false, scripts: [] } }) as T;
      return settingsResult() as T;
    case "relay_status": return ok({ authenticated: true, authSource: "演示", accountLabel: "Web preview", configPath: "演示模式", configured: true, requiresOpenaiAuth: false, hasBearerToken: true, backupPath: null }) as T;
    case "read_relay_files": return ok({ configPath: "演示模式", authPath: "演示模式", configContents: "[model_providers.custom]", authContents: "{}" }) as T;
    case "check_env_conflicts": return ok({ conflicts: [] }) as T;
    case "load_provider_sync_targets": return ok({ currentProvider: "deepseek", targets: [] }) as T;
    case "load_pending_provider_import": return ok({ pending: null }) as T;
    case "load_pending_dream_skin_community": return ok({ versionId: "" }) as T;
    case "load_watcher_state": return ok({ enabled: false, running: false, status: "disabled", message: "演示模式" }) as T;
    case "load_auto_takeover_state": return ok({ running: false, enabled: false, message: "演示模式" }) as T;
    case "update_check": return ok({ currentVersion: "1.12.1", latestVersion: "1.12.1", updateAvailable: false }) as T;
    case "refresh_script_market": return ok({ market: { status: "ok", message: "演示模式", scripts: [] }, user_scripts: { enabled: true, skip_next_start: false, scripts: [] } }) as T;
    case "load_ccs_providers": return ok({ providers: [] }) as T;
    case "weixin_connect_status": return ok({ state: "disconnected", message: "演示模式", accountId: "", hasToken: false, lastPeerId: "", lastMessageAtMs: 0, processedMessages: 0 }) as T;
    case "load_local_sessions":
    case "list_local_sessions": return ok({ dbPath: "演示模式", dbPaths: [], sessions: [], offset: 0, limit: 50, hasMore: false, totalCount: 0 }) as T;
    case "load_relay_environment":
    case "check_relay_environment": return ok({ clashVergeTun: { enabled: false, configPath: null }, proxyEnvironment: { variables: [] }, codexEnvFile: { exists: false, path: "" } }) as T;
    default: return ok() as T;
  }
}

export function convertFileSrc(path: string): string {
  return path;
}
