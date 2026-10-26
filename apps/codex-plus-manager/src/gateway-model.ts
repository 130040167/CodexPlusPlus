/** 网关模式的前端数据模型：与 `codex_plus_core::gateway` 的序列化结构一一对应，外加表单转换。 */
import { PRESETS } from "./presets.ts";

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
  baseUrl: string;
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

/** 路由动态里的一次尝试。 */
export type RouteStepView = {
  relayId: string;
  relayName: string;
  keyId: string;
  model: string;
  outcome: "answered" | "failed" | "transport";
  failure?: string | null;
  status?: number | null;
  elapsedMs: number;
};

export type RouteRecordView = {
  requestedModel: string;
  model: string;
  groupId?: string | null;
  planned: Array<{ relayId: string; relayName: string; model: string }>;
  steps: RouteStepView[];
  result: "answered" | "failed" | "cooling";
  coolingMs?: number | null;
  finishedAtMs: number;
  elapsedMs: number;
};

export type AgentActivityView = {
  agent: string;
  requests: number;
  rerouted: number;
  errors: number;
  inFlight: string[];
  last?: RouteRecordView | null;
};

export type GatewayPayload = {
  gateway: GatewaySettings;
  port: number;
  helperRunning: boolean;
  pool: GatewayPoolEntry[];
  /** 还没加入池的 Codex 供应商。 */
  codexProviders: GatewayPoolEntry[];
  groups: GatewayGroupEntry[];
  agents: GatewayAgentView[];
  health: { keys: KeyHealthView[]; providers: ProviderHealthView[] };
  /** 旧版后端没有这个字段。 */
  activity?: { agents: AgentActivityView[] };
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

export const GATEWAY_AGENT_NAMES: Record<string, string> = { claude: "Claude Code", grok: "Grok", codex: "Codex" };

export function agentName(agent: string): string {
  return GATEWAY_AGENT_NAMES[agent] ?? agent;
}

/** 路由组在模型选择器、路由页里的短名：按组名生成（组 id 是一串随机字符，不适合给人看）。 */
export function groupSlug(name: string): string {
  const slug = name.trim().toLowerCase().replace(/[\s_/]+/g, "-").replace(/^-+|-+$/g, "");
  return `group/${slug || "unnamed"}`;
}

export type PickerItem = {
  /** `provider:<id>` / `group:<id>` */
  target: string;
  /** 路由组为空：用组里成员各自的模型。 */
  model: string;
  label: string;
  sub: string;
  avatar: string;
  custom?: boolean;
};

export type PickerSection = { key: string; title: string; avatar: string; items: PickerItem[] };

function matches(query: string, ...values: string[]): boolean {
  if (!query) return true;
  return values.some((value) => value.toLowerCase().includes(query));
}

/**
 * 模型选择器的分组：路由组在前，然后每个供应商一组列出它的模型。
 * 查询词不是任何已知模型时，另给一组「自定义模型」：发到各个供应商。
 * `avatar` 由调用方算（避免这里依赖界面工具函数）。
 */
export function pickerSections(
  payload: GatewayPayload | null,
  rawQuery: string,
  avatar: (name: string) => string,
): PickerSection[] {
  if (!payload) return [];
  const query = rawQuery.trim().toLowerCase();
  const sections: PickerSection[] = [];
  const groups = payload.groups
    .filter((group) => matches(query, group.name, groupSlug(group.name)))
    .map((group) => ({
      target: `group:${group.id}`,
      model: "",
      label: group.name,
      sub: groupSlug(group.name),
      avatar: avatar(group.name),
    }));
  if (groups.length) sections.push({ key: "group", title: "路由组", avatar: "", items: groups });
  for (const entry of payload.pool) {
    const providerHit = matches(query, entry.name);
    const items = entry.models
      .filter((model) => providerHit || matches(query, model))
      .map((model) => ({
        target: `provider:${entry.id}`,
        model,
        label: model,
        sub: entry.name,
        avatar: avatar(entry.name),
      }));
    if (items.length) sections.push({ key: `provider:${entry.id}`, title: entry.name, avatar: avatar(entry.name), items });
  }
  const typed = rawQuery.trim();
  const known = payload.pool.some((entry) => entry.models.includes(typed));
  if (typed && !known && !/\s/.test(typed)) {
    sections.push({
      key: "custom",
      title: "自定义模型",
      avatar: "",
      items: payload.pool.slice(0, 6).map((entry) => ({
        target: `provider:${entry.id}`,
        model: typed,
        label: typed,
        sub: entry.name,
        avatar: avatar(entry.name),
        custom: true,
      })),
    });
  }
  return sections;
}

/** 当前绑定在选择器按钮上的显示：模型名 + 供应商，或路由组名。 */
export function bindingDisplay(
  payload: GatewayPayload | null,
  target: string,
  model: string,
): { title: string; sub: string; kind: GatewayTargetKind | null } {
  const parsed = parseTargetValue(target);
  if (!payload || !parsed) return { title: "", sub: "", kind: null };
  if (parsed.kind === "group") {
    const group = payload.groups.find((item) => item.id === parsed.id);
    return { title: group?.name ?? parsed.id, sub: groupSlug(group?.name ?? parsed.id), kind: "group" };
  }
  const entry = payload.pool.find((item) => item.id === parsed.id);
  return { title: model.trim() || entry?.models[0] || parsed.id, sub: entry?.name ?? parsed.id, kind: "provider" };
}

export function strategyLabel(strategy: string): string {
  switch (strategy) {
    case "conversationRoundRobin":
      return "按对话轮转";
    case "requestRoundRobin":
      return "按请求轮转";
    case "weightedRoundRobin":
      return "权重轮转";
    default:
      return "依次";
  }
}

export function strategyDescription(strategy: string): string {
  switch (strategy) {
    case "conversationRoundRobin":
      return "按对话轮转：同一个对话固定一个成员，新对话轮到下一个；成员答不了时换下一个，每个成员再按自己的 Key 轮换。";
    case "requestRoundRobin":
      return "按请求轮转：每个请求轮到下一个成员；成员答不了时换下一个，每个成员再按自己的 Key 轮换。";
    case "weightedRoundRobin":
      return "权重轮转：按成员权重分配请求；成员答不了时换下一个，每个成员再按自己的 Key 轮换。";
    default:
      return "依次：按成员顺序，前一个答不了才换下一个，每个成员再按自己的 Key 轮换。";
  }
}

export type MemberStatus = { state: "busy" | "answered" | "cooling" | "idle"; text: string; args: Array<string | number> };

/** 供应商在路由图里的状态：正在回答 / 熔断或 Key 全部冷却 / 最近回答了谁 / Key 数。文案是中文原文，界面再翻译。 */
export function memberStatus(payload: GatewayPayload | null, relayId: string): MemberStatus {
  const activity = payload?.activity?.agents ?? [];
  if (activity.some((agent) => agent.inFlight.includes(relayId))) {
    return { state: "busy", text: "正在回答…", args: [] };
  }
  const breaker = payload?.health.providers.find((item) => item.relayId === relayId && item.breakerOpen);
  if (breaker) return { state: "cooling", text: "连续出错 · {0} 后恢复", args: [formatRemaining(breaker.remainingMs)] };
  const entry = payload?.pool.find((item) => item.id === relayId);
  const keyCount = entry?.keyCount ?? 1;
  const unhealthy = (payload?.health.keys ?? []).filter((key) => key.relayId === relayId && key.state !== "ok");
  if (unhealthy.length >= keyCount && unhealthy.length > 0) {
    const cooling = unhealthy.filter((key) => key.state === "cooling");
    if (!cooling.length) return { state: "cooling", text: "Key 都已停用", args: [] };
    const soonest = Math.min(...cooling.map((key) => key.remainingMs ?? 0));
    const label = cooling.some((key) => key.lastFailure === "quota_exhausted") ? "额度耗尽 · {0} 后恢复" : "被限流 · {0} 后恢复";
    return { state: "cooling", text: label, args: [formatRemaining(soonest)] };
  }
  const answered = activity
    .filter((agent) => agent.last?.result === "answered" && agent.last.steps.at(-1)?.relayId === relayId)
    .sort((left, right) => (right.last?.finishedAtMs ?? 0) - (left.last?.finishedAtMs ?? 0))[0];
  if (answered) return { state: "answered", text: "回答了 {0}", args: [agentName(answered.agent)] };
  if (unhealthy.length) return { state: "idle", text: "{0} 个 Key · {1} 个冷却中", args: [keyCount, unhealthy.length] };
  return keyCount > 1 ? { state: "idle", text: "{0} 个 Key", args: [keyCount] } : { state: "idle", text: "一个 Key", args: [] };
}

export function formatElapsed(ms: number): string {
  if (ms < 10_000) return `${(ms / 1000).toFixed(1)} s`;
  return `${Math.round(ms / 1000)} s`;
}

export type NarrativeLine = { tone: "muted" | "step" | "ok" | "fail"; text: string; args: Array<string | number> };

/** 「上一个请求是怎么路由的」：按记录逐条生成。文案是中文模板，界面用 tf 翻译。 */
export function routeNarrative(agent: string, record: RouteRecordView, payload: GatewayPayload | null): NarrativeLine[] {
  const lines: NarrativeLine[] = [];
  const who = agentName(agent);
  const requested = record.requestedModel || record.model;
  const plan = record.planned.map((member) => `${member.relayName}/${member.model}`).join(", ");
  if (record.groupId) {
    const group = payload?.groups.find((item) => item.id === record.groupId);
    lines.push({ tone: "muted", text: "{0} 请求 {1}，交给路由组「{2}」：{3}", args: [who, requested, group?.name ?? record.groupId, plan] });
    if (record.planned[0]) lines.push({ tone: "step", text: "{0} 排在第一个。", args: [record.planned[0].relayName] });
  } else {
    lines.push({ tone: "muted", text: "{0} 请求 {1}，转发到 {2}", args: [who, requested, plan] });
  }
  record.steps.forEach((step, index) => {
    const last = index === record.steps.length - 1;
    if (step.outcome === "answered") {
      lines.push({ tone: "ok", text: "{0}（{1}）回答，用时 {2}。", args: [step.relayName, step.model, formatElapsed(record.elapsedMs)] });
      return;
    }
    const reason = step.outcome === "transport" ? "连不上" : failureLabel(step.failure) || "出错";
    const status = step.status ? ` · HTTP ${step.status}` : "";
    lines.push({
      tone: last ? "fail" : "step",
      text: last ? "{0} 的 Key {1} {2}{3}，错误交回给 {4}。" : "{0} 的 Key {1} {2}{3}，换下一个。",
      args: last ? [step.relayName, step.keyId, reason, status, who] : [step.relayName, step.keyId, reason, status],
    });
  });
  if (record.result === "cooling") {
    lines.push({ tone: "fail", text: "所有 Key 都在冷却，约 {0} 后恢复。", args: [formatRemaining(record.coolingMs ?? 0) || "1s"] });
  }
  return lines;
}

/** 绑定到同一目标的 Agent 合成一张路由卡片。 */
export function routeCards(payload: GatewayPayload | null): Array<{ target: string; agents: GatewayAgentId[] }> {
  if (!payload) return [];
  const cards: Array<{ target: string; agents: GatewayAgentId[] }> = [];
  for (const view of payload.agents) {
    const binding = payload.gateway.bindings[view.agent];
    if (!view.enabled || !binding) continue;
    const target = targetValue(binding.target);
    const card = cards.find((item) => item.target === target);
    if (card) card.agents.push(view.agent);
    else cards.push({ target, agents: [view.agent] });
  }
  return cards;
}

/** 路由卡片右侧的成员：供应商就是它自己，路由组按成员顺序；模型取最近一次计划里的，没有就用成员默认模型。 */
export function routeMembers(payload: GatewayPayload | null, target: string, records: RouteRecordView[]): Array<{ relayId: string; name: string; model: string }> {
  const parsed = parseTargetValue(target);
  if (!payload || !parsed) return [];
  const ids = parsed.kind === "provider" ? [parsed.id] : payload.groups.find((group) => group.id === parsed.id)?.members ?? [];
  return ids.map((id) => {
    const entry = payload.pool.find((item) => item.id === id);
    const planned = records.flatMap((record) => record.planned).find((member) => member.relayId === id);
    return { relayId: id, name: entry?.name ?? id, model: planned?.model ?? entry?.models[0] ?? "" };
  });
}

/* —————————————— 内置厂商预设 —————————————— */

export type GatewayProtocol = GatewayPoolEntry["protocol"];
export type PresetSection = "vendor" | "relay" | "local";

export type GatewayPreset = {
  key: string;
  name: string;
  baseUrl: string;
  protocol: GatewayProtocol;
  model: string;
  modelList: string[];
  section: PresetSection;
  /** 本机服务不校验 Key，预填一个占位值。 */
  defaultKey?: string;
  apiKeyUrl?: string;
};

/** 应用里通用预设之外、网关额外提供的几个：Anthropic 原生协议、xAI 和本机服务。 */
const GATEWAY_EXTRA_PRESETS: GatewayPreset[] = [
  {
    key: "anthropic",
    name: "Anthropic",
    baseUrl: "https://api.anthropic.com",
    protocol: "anthropicMessages",
    model: "claude-sonnet-5-5",
    modelList: ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"],
    section: "vendor",
    apiKeyUrl: "https://console.anthropic.com/settings/keys",
  },
  {
    key: "xai",
    name: "xAI",
    baseUrl: "https://api.x.ai/v1",
    protocol: "chatCompletions",
    model: "",
    modelList: [],
    section: "vendor",
    apiKeyUrl: "https://console.x.ai",
  },
  {
    key: "ollama",
    name: "Ollama",
    baseUrl: "http://127.0.0.1:11434/v1",
    protocol: "chatCompletions",
    model: "",
    modelList: [],
    section: "local",
    defaultKey: "ollama",
  },
  {
    key: "lm-studio",
    name: "LM Studio",
    baseUrl: "http://127.0.0.1:1234/v1",
    protocol: "chatCompletions",
    model: "",
    modelList: [],
    section: "local",
    defaultKey: "lm-studio",
  },
];

/** 「添加供应商」里的全部预设：厂商（官方 / 国内官方）、中转、本机。 */
export function gatewayPresets(): GatewayPreset[] {
  const shared = PRESETS.map((preset): GatewayPreset => ({
    key: preset.id,
    name: preset.name,
    baseUrl: preset.baseUrl,
    protocol: preset.protocol,
    model: preset.model,
    modelList: preset.modelList ?? (preset.model ? [preset.model] : []),
    section: preset.category === "official" || preset.category === "cn_official" ? "vendor" : "relay",
    apiKeyUrl: preset.apiKeyUrl,
  }));
  const vendors = [...GATEWAY_EXTRA_PRESETS.filter((preset) => preset.section === "vendor"), ...shared.filter((preset) => preset.section === "vendor")];
  return [...vendors, ...shared.filter((preset) => preset.section === "relay"), ...GATEWAY_EXTRA_PRESETS.filter((preset) => preset.section === "local")];
}

function normalizeUrl(url: string): string {
  return url.trim().replace(/\/+$/, "").toLowerCase();
}

/** 预设卡片上的副标题：地址的主机名（本机服务显示端口）。 */
export function presetHost(baseUrl: string): string {
  try {
    const url = new URL(baseUrl);
    return url.hostname === "127.0.0.1" || url.hostname === "localhost" ? `localhost:${url.port}` : url.hostname;
  } catch {
    return baseUrl;
  }
}

/** 池里已有同一地址的供应商：卡片显示为已添加（仍可再加一个，比如第二个账号）。 */
export function presetAdded(payload: GatewayPayload | null, preset: GatewayPreset): boolean {
  const target = normalizeUrl(preset.baseUrl);
  return Boolean(payload?.pool.some((entry) => normalizeUrl(entry.baseUrl) === target));
}

/** 预设 / 自定义供应商的表单。 */
export type ProviderDraft = { name: string; baseUrl: string; apiKey: string; protocol: GatewayProtocol; model: string; modelList: string };

export function providerDraftFromPreset(preset: GatewayPreset | null): ProviderDraft {
  if (!preset) return { name: "", baseUrl: "", apiKey: "", protocol: "chatCompletions", model: "", modelList: "" };
  return {
    name: preset.name,
    baseUrl: preset.baseUrl,
    apiKey: preset.defaultKey ?? "",
    protocol: preset.protocol,
    model: preset.model,
    modelList: preset.modelList.join("\n"),
  };
}

/** 表单校验：返回中文错误原文，没问题返回 null。 */
export function validateProviderDraft(draft: ProviderDraft): string | null {
  if (!draft.name.trim()) return "请填写名称";
  if (!/^https?:\/\/\S+$/i.test(draft.baseUrl.trim())) return "地址要以 http:// 或 https:// 开头";
  if (!draft.apiKey.trim()) return "请填写 API Key";
  if (!draft.model.trim() && splitModels(draft.modelList).length === 0) return "至少填一个模型";
  return null;
}
