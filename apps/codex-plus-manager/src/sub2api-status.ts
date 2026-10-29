// 供应商卡片上的 sub2api 倍率 / 余额：识别结果缓存与数字格式化。
// 识别本身在后端 `probe_sub2api`（同时请求 /v1/sub2api/billing 与 /v1/usage）。

export type Sub2ApiBillingInfo = {
  endpoint: string;
  groupRateMultiplier: number;
  userRateMultiplier: number | null;
  resolvedRateMultiplier: number;
  peakRateEnabled: boolean;
  peakRateMultiplier: number | null;
  appliedPeakMultiplier: number | null;
  effectiveRateMultiplier: number;
  observedAt: string;
};

export type Sub2ApiRateLimitWindow = { window: string; limit: number; used: number; remaining: number };

export type Sub2ApiUsageInfo = {
  endpoint: string;
  /** keyQuota：Key 自带额度/限速；subscription：订阅分组；wallet：钱包余额。 */
  kind: "keyQuota" | "subscription" | "wallet" | string;
  isValid: boolean;
  planName: string | null;
  remaining: number | null;
  quotaLimit: number | null;
  quotaUsed: number | null;
  unit: string;
  rateLimits: Sub2ApiRateLimitWindow[];
};

export type Sub2ApiProbe = {
  isSub2api: boolean;
  billing: Sub2ApiBillingInfo | null;
  usage: Sub2ApiUsageInfo | null;
  detail: string;
};

export type Sub2ApiProbeProfile = {
  id: string;
  baseUrl: string;
  upstreamBaseUrl: string;
  apiKey: string;
  apiKeys?: { id: string; apiKey: string }[];
  activeApiKeyId?: string;
  relayMode: string;
  officialMixApiKey: boolean;
  noAuth: boolean;
};

/** 是 sub2api 时余额会变，缓存 5 分钟；不是 sub2api 的记 1 天，免得每次打开都去敲门。 */
export const SUB2API_POSITIVE_TTL_MS = 5 * 60_000;
export const SUB2API_NEGATIVE_TTL_MS = 24 * 60 * 60_000;
export const SUB2API_NEGATIVE_STORAGE_KEY = "codex-plus-sub2api-negative-probes";

function activeApiKey(profile: Sub2ApiProbeProfile): string {
  const keys = profile.apiKeys ?? [];
  const selected = keys.find((entry) => entry.id === profile.activeApiKeyId) ?? keys[0];
  return (selected?.apiKey ?? profile.apiKey).trim();
}

function upstreamUrl(profile: Sub2ApiProbeProfile): string {
  return (profile.upstreamBaseUrl.trim() || profile.baseUrl.trim()).replace(/\/+$/, "");
}

/** 官方登录、聚合、免鉴权或缺地址 / Key 的供应商不去识别。 */
export function sub2apiProbeEligible(profile: Sub2ApiProbeProfile): boolean {
  if (profile.relayMode === "aggregate" || profile.noAuth) return false;
  if (profile.relayMode === "official" && !profile.officialMixApiKey) return false;
  return Boolean(upstreamUrl(profile) && activeApiKey(profile));
}

// FNV-1a：缓存键里只放 Key 的指纹，不落明文。
function fingerprint(text: string): string {
  let hash = 0x811c9dc5;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 0x01000193);
  }
  return (hash >>> 0).toString(36);
}

/** 地址或当前 Key 一变，缓存键随之变化，自动重新识别。 */
export function sub2apiProbeCacheKey(profile: Sub2ApiProbeProfile): string {
  return `${profile.id}|${fingerprint(`${upstreamUrl(profile)}\n${activeApiKey(profile)}`)}`;
}

/** 网关供应商池：前端看不到 Key，按地址、Key 数和代理变化重新识别。
 *  订阅登录账号和内置的官方厂商（DeepSeek、Kimi 等）不识别。 */
export function sub2apiGatewayCacheKey(entry: {
  id: string;
  baseUrl: string;
  keyCount: number;
  official?: boolean;
  proxy?: string;
  account?: string | null;
}): string {
  if (entry.account || entry.official || entry.keyCount <= 0 || !entry.baseUrl.trim()) return "";
  const url = entry.baseUrl.trim().replace(/\/+$/, "");
  return `gateway:${entry.id}|${fingerprint(`${url}\n${entry.keyCount}\n${entry.proxy ?? ""}`)}`;
}

type StorageLike = Pick<Storage, "getItem" | "setItem">;

function readNegatives(storage: StorageLike | null): Record<string, number> {
  try {
    const parsed = JSON.parse(storage?.getItem(SUB2API_NEGATIVE_STORAGE_KEY) ?? "{}");
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

export function sub2apiNegativeCached(storage: StorageLike | null, key: string, now = Date.now()): boolean {
  const checkedAt = readNegatives(storage)[key];
  return typeof checkedAt === "number" && now - checkedAt < SUB2API_NEGATIVE_TTL_MS;
}

/** 记下或清掉「不是 sub2api」，顺手丢掉过期条目，避免越攒越多。 */
export function rememberSub2ApiNegative(storage: StorageLike | null, key: string, negative: boolean, now = Date.now()) {
  const next: Record<string, number> = {};
  for (const [entryKey, checkedAt] of Object.entries(readNegatives(storage))) {
    if (entryKey !== key && typeof checkedAt === "number" && now - checkedAt < SUB2API_NEGATIVE_TTL_MS) {
      next[entryKey] = checkedAt;
    }
  }
  if (negative) next[key] = now;
  try {
    storage?.setItem(SUB2API_NEGATIVE_STORAGE_KEY, JSON.stringify(next));
  } catch {
    // 存不了只影响下次是否重新识别。
  }
}

export function formatSub2ApiMultiplier(value: number): string {
  return `${Number(value.toFixed(4))}x`;
}

export function formatSub2ApiAmount(value: number, unit: string): string {
  const amount = value.toFixed(2);
  const normalized = unit.trim().toUpperCase();
  if (!normalized || normalized === "USD") return `$${amount}`;
  if (normalized === "CNY" || normalized === "RMB") return `¥${amount}`;
  return `${amount} ${unit.trim()}`;
}

/** 高峰时段实际叠加的倍率；不在高峰或倍率为 1 时不单独提示。 */
export function sub2apiAppliedPeak(billing: Sub2ApiBillingInfo | null): number | null {
  const peak = billing?.appliedPeakMultiplier;
  return typeof peak === "number" && Number.isFinite(peak) && peak !== 1 ? peak : null;
}
