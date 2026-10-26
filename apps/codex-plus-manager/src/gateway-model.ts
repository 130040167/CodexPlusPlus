/** 网关模式的前端数据模型：与 `codex_plus_core::gateway` 的序列化结构一一对应，外加表单转换。 */

export type GatewayAgentId = "claude" | "grok";
export type GatewayTargetKind = "provider" | "group";
export type UnknownModelPolicy = "mapToDefault" | "passthrough";
export type KeyRotationMode = "sticky" | "roundRobin";

export type GatewayTarget = { kind: GatewayTargetKind; id: string };

export type ClaudeRoleModels = { opus: string; sonnet: string; haiku: string; subagent: string };

export type AgentBinding = {
  enabled: boolean;
  target: GatewayTarget;
  model: string;
  extraModels: string[];
  claudeRoles: ClaudeRoleModels;
  unknownModel: UnknownModelPolicy;
  contextWindow?: number | null;
  claudeEnableEffort: boolean;
  token: string;
  restore?: { claudePreviousProviderId?: string; appliedPort?: number | null };
};

export type GatewaySettings = {
  version: number;
  bindings: Partial<Record<GatewayAgentId, AgentBinding>>;
  keyPolicy: { mode: KeyRotationMode };
  restoreOnExit: boolean;
};

export type GatewayPoolEntry = {
  id: string;
  name: string;
  protocol: "responses" | "chatCompletions" | "anthropicMessages";
  official: boolean;
  models: string[];
  keyCount: number;
};

export type GatewayGroupEntry = { id: string; name: string; strategy: string; members: string[] };

export type GatewayAgentView = {
  agent: GatewayAgentId;
  name: string;
  enabled: boolean;
  baseUrl: string;
  defaultModel: string;
  models: string[];
};

export type KeyHealthView = {
  relayId: string;
  keyId: string;
  state: "ok" | "cooling" | "disabled";
  remainingMs?: number | null;
  lastFailure?: string | null;
  lastFailureAtMs?: number | null;
};

export type ProviderHealthView = {
  relayId: string;
  breakerOpen: boolean;
  remainingMs?: number | null;
  consecutiveErrors: number;
};

export type GatewayPayload = {
  gateway: GatewaySettings;
  port: number;
  helperRunning: boolean;
  pool: GatewayPoolEntry[];
  groups: GatewayGroupEntry[];
  agents: GatewayAgentView[];
  health: { keys: KeyHealthView[]; providers: ProviderHealthView[] };
  claudeProviders: Array<{ id: string; name: string }>;
  grokProfiles: Array<{ id: string; name: string }>;
};

export type GatewayResult = { status: string; message: string } & Partial<GatewayPayload>;

/** 编辑表单：多行文本、数字都先当字符串处理，提交时再转。 */
export type BindingDraft = {
  target: string;
  model: string;
  extraModels: string;
  opus: string;
  sonnet: string;
  haiku: string;
  subagent: string;
  unknownModel: UnknownModelPolicy;
  contextWindow: string;
  claudeEnableEffort: boolean;
};

/** 目标下拉框的值：`provider:<id>` / `group:<id>`。 */
export function targetValue(target: GatewayTarget | undefined): string {
  return target && target.id ? `${target.kind}:${target.id}` : "";
}

export function parseTargetValue(value: string): GatewayTarget | null {
  const index = value.indexOf(":");
  if (index <= 0) return null;
  const kind = value.slice(0, index);
  const id = value.slice(index + 1).trim();
  if ((kind !== "provider" && kind !== "group") || !id) return null;
  return { kind, id };
}

export function blankBinding(): AgentBinding {
  return {
    enabled: false,
    target: { kind: "provider", id: "" },
    model: "",
    extraModels: [],
    claudeRoles: { opus: "", sonnet: "", haiku: "", subagent: "" },
    unknownModel: "mapToDefault",
    contextWindow: null,
    claudeEnableEffort: false,
    token: "",
  };
}

export function draftFromBinding(binding: AgentBinding | undefined): BindingDraft {
  const source = binding ?? blankBinding();
  return {
    target: targetValue(source.target),
    model: source.model,
    extraModels: source.extraModels.join("\n"),
    opus: source.claudeRoles.opus,
    sonnet: source.claudeRoles.sonnet,
    haiku: source.claudeRoles.haiku,
    subagent: source.claudeRoles.subagent,
    unknownModel: source.unknownModel,
    contextWindow: source.contextWindow ? String(source.contextWindow) : "",
    claudeEnableEffort: source.claudeEnableEffort,
  };
}

/** 窗口支持纯数字和 `200k` / `1M` 写法。无效时返回 NaN。 */
export function parseContextWindow(value: string): number | null {
  const text = value.trim().toLowerCase();
  if (!text) return null;
  const match = /^(\d+(?:\.\d+)?)\s*([km]?)$/.exec(text);
  if (!match) return Number.NaN;
  const base = Number(match[1]);
  const scale = match[2] === "m" ? 1_000_000 : match[2] === "k" ? 1_000 : 1;
  return Math.round(base * scale);
}

export function splitModels(text: string): string[] {
  const seen = new Set<string>();
  return text
    .split(/[\n,]/)
    .map((line) => line.trim())
    .filter((line) => {
      if (!line || seen.has(line)) return false;
      seen.add(line);
      return true;
    });
}

/** 表单 → 提交给后端的绑定。token 与恢复现场由后端维护，这里沿用旧值。 */
export function bindingFromDraft(draft: BindingDraft, previous: AgentBinding | undefined): AgentBinding {
  const base = previous ?? blankBinding();
  const window = parseContextWindow(draft.contextWindow);
  return {
    ...base,
    target: parseTargetValue(draft.target) ?? { kind: "provider", id: "" },
    model: draft.model.trim(),
    extraModels: splitModels(draft.extraModels),
    claudeRoles: {
      opus: draft.opus.trim(),
      sonnet: draft.sonnet.trim(),
      haiku: draft.haiku.trim(),
      subagent: draft.subagent.trim(),
    },
    unknownModel: draft.unknownModel,
    contextWindow: window && Number.isFinite(window) ? window : null,
    claudeEnableEffort: draft.claudeEnableEffort,
  };
}

/** 表单校验：返回错误文案（中文原文，界面再翻译），没问题返回 null。 */
export function validateDraft(draft: BindingDraft, payload: GatewayPayload | null): string | null {
  const target = parseTargetValue(draft.target);
  if (!target) return "请选择供应商或路由组";
  if (payload) {
    const exists = target.kind === "provider"
      ? payload.pool.some((entry) => entry.id === target.id)
      : payload.groups.some((group) => group.id === target.id);
    if (!exists) return "所选供应商或路由组已不存在";
    if (!draft.model.trim() && target.kind === "provider") {
      const entry = payload.pool.find((item) => item.id === target.id);
      if (!entry?.models.length) return "该供应商没有默认模型，请填写主模型";
    }
  }
  const window = parseContextWindow(draft.contextWindow);
  if (window !== null && (!Number.isFinite(window) || window < 1_000)) return "上下文窗口格式不对，例如 200000、200k 或 1M";
  return null;
}

/** 目标可选的模型（供下拉提示）：供应商就是它的模型，路由组是全部成员的并集。 */
export function targetModels(payload: GatewayPayload | null, value: string): string[] {
  const target = parseTargetValue(value);
  if (!payload || !target) return [];
  const relayIds = target.kind === "provider"
    ? [target.id]
    : payload.groups.find((group) => group.id === target.id)?.members ?? [];
  const models: string[] = [];
  for (const id of relayIds) {
    for (const model of payload.pool.find((entry) => entry.id === id)?.models ?? []) {
      if (!models.includes(model)) models.push(model);
    }
  }
  return models;
}

export function protocolLabel(protocol: GatewayPoolEntry["protocol"]): string {
  switch (protocol) {
    case "chatCompletions":
      return "Chat";
    case "anthropicMessages":
      return "Anthropic";
    default:
      return "Responses";
  }
}

/** Key 健康状态里的失败分类 → 中文说明。 */
export function failureLabel(value: string | null | undefined): string {
  switch (value) {
    case "rate_limited":
      return "被限流";
    case "quota_exhausted":
      return "额度耗尽";
    case "auth_invalid":
      return "Key 无效";
    case "server_error":
      return "上游出错";
    case "failover":
      return "已切换";
    case "client_error":
      return "请求被拒";
    default:
      return "";
  }
}

export function formatRemaining(ms: number | null | undefined): string {
  if (!ms || ms <= 0) return "";
  const seconds = Math.ceil(ms / 1000);
  if (seconds < 60) return `${seconds}s`;
  const minutes = Math.ceil(seconds / 60);
  if (minutes < 60) return `${minutes}m`;
  return `${Math.floor(minutes / 60)}h${minutes % 60 ? `${minutes % 60}m` : ""}`;
}

/** Claude 供应商列表里网关托管的那一条。 */
export const GATEWAY_CLAUDE_PROVIDER_ID = "codex-plus-gateway";
