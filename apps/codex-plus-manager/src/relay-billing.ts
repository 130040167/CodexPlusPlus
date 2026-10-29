// 供应商卡片上的倍率 / 余额：识别结果缓存与数字格式化。
// 识别本身在后端 `probe_relay_billing`：先试 sub2api（/v1/sub2api/billing、/v1/usage），
// 不是再试 new-api（/api/usage/token/、/api/log/token、/api/status）。

export type RelayBillingPlatform = "sub2api" | "newapi";

export type RelayRateInfo = {
  endpoint: string;
  multiplier: number;
  /** sub2api：高峰时段叠加的倍率。 */
  appliedPeakMultiplier: number | null;
  /** new-api：倍率取自哪个分组的消费记录。 */
  group: string | null;
};

export type RelayRateLimitWindow = { window: string; limit: number; used: number; remaining: number };

export type RelayUsageInfo = {
  endpoint: string;
  /** keyQuota：Key 自带额度/限速；subscription：订阅分组；wallet：账户余额；unlimited：Key 不限额度且查不到账户余额。 */
  kind: "keyQuota" | "subscription" | "wallet" | "unlimited" | string;
  isValid: boolean;
  planName: string | null;
  remaining: number | null;
  quotaLimit: number | null;
  quotaUsed: number | null;
  unit: string;
  rateLimits: RelayRateLimitWindow[];
};

export type RelayBillingProbe = {
  /** 识别出的中转站；null 表示不是已知的中转站。 */
  platform: RelayBillingPlatform | null;
  rate: RelayRateInfo | null;
  usage: RelayUsageInfo | null;
  detail: string;
};

export function relayPlatformLabel(platform: RelayBillingPlatform): string {
  return platform === "newapi" ? "new-api" : "sub2api";
}

export type RelayBillingProfile = {
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

/** 识别出中转站时余额会变，缓存 5 分钟；识别不出的记 1 天，免得每次打开都去敲门。 */
export const RELAY_BILLING_POSITIVE_TTL_MS = 5 * 60_000;
export const RELAY_BILLING_NEGATIVE_TTL_MS = 24 * 60 * 60_000;
export const RELAY_BILLING_NEGATIVE_STORAGE_KEY = "codex-plus-relay-billing-negative-probes";

function activeApiKey(profile: RelayBillingProfile): string {
  const keys = profile.apiKeys ?? [];
  const selected = keys.find((entry) => entry.id === profile.activeApiKeyId) ?? keys[0];
  return (selected?.apiKey ?? profile.apiKey).trim();
}

function upstreamUrl(profile: RelayBillingProfile): string {
  return (profile.upstreamBaseUrl.trim() || profile.baseUrl.trim()).replace(/\/+$/, "");
}

/** 官方登录、聚合、免鉴权或缺地址 / Key 的供应商不去识别。 */
export function relayBillingProbeEligible(profile: RelayBillingProfile): boolean {
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
export function relayBillingCacheKey(profile: RelayBillingProfile): string {
  return `${profile.id}|${fingerprint(`${upstreamUrl(profile)}\n${activeApiKey(profile)}`)}`;
}

/** 网关供应商池：前端看不到 Key，按地址、Key 数和代理变化重新识别。
 *  订阅登录账号和内置的官方厂商（DeepSeek、Kimi 等）不识别。 */
export function relayBillingGatewayCacheKey(entry: {
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
    const parsed = JSON.parse(storage?.getItem(RELAY_BILLING_NEGATIVE_STORAGE_KEY) ?? "{}");
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch {
    return {};
  }
}

export function relayBillingNegativeCached(storage: StorageLike | null, key: string, now = Date.now()): boolean {
  const checkedAt = readNegatives(storage)[key];
  return typeof checkedAt === "number" && now - checkedAt < RELAY_BILLING_NEGATIVE_TTL_MS;
}

/** 记下或清掉「识别不出」，顺手丢掉过期条目，避免越攒越多。 */
export function rememberRelayBillingNegative(storage: StorageLike | null, key: string, negative: boolean, now = Date.now()) {
  const next: Record<string, number> = {};
  for (const [entryKey, checkedAt] of Object.entries(readNegatives(storage))) {
    if (entryKey !== key && typeof checkedAt === "number" && now - checkedAt < RELAY_BILLING_NEGATIVE_TTL_MS) {
      next[entryKey] = checkedAt;
    }
  }
  if (negative) next[key] = now;
  try {
    storage?.setItem(RELAY_BILLING_NEGATIVE_STORAGE_KEY, JSON.stringify(next));
  } catch {
    // 存不了只影响下次是否重新识别。
  }
}

export function formatRelayMultiplier(value: number): string {
  return `${Number(value.toFixed(4))}x`;
}

export function formatRelayAmount(value: number, unit: string): string {
  const amount = value.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
  const normalized = unit.trim().toUpperCase();
  if (!normalized || normalized === "USD") return `$${amount}`;
  if (normalized === "CNY" || normalized === "RMB") return `¥${amount}`;
  return `${amount} ${unit.trim()}`;
}

/** 高峰时段实际叠加的倍率；不在高峰或倍率为 1 时不单独提示。 */
export function relayAppliedPeak(rate: RelayRateInfo | null): number | null {
  const peak = rate?.appliedPeakMultiplier;
  return typeof peak === "number" && Number.isFinite(peak) && peak !== 1 ? peak : null;
}

/** 供应商池右侧徽章：剩余金额，以及有上限时的剩余比例（画额度条）。 */
export function relayBalanceBadge(usage: RelayUsageInfo): { amount: string; ratio: number | null; tone: "ok" | "mid" | "low" } | null {
  if (usage.remaining === null) return null;
  const ratio = usage.kind === "keyQuota" && usage.quotaLimit && usage.quotaLimit > 0
    ? Math.min(1, Math.max(0, usage.remaining / usage.quotaLimit))
    : null;
  const exhausted = usage.remaining <= 0 || !usage.isValid || usage.rateLimits.some((window) => window.remaining <= 0);
  const tone = exhausted ? "low" : ratio !== null && ratio <= 0.1 ? "low" : ratio !== null && ratio <= 0.3 ? "mid" : "ok";
  return { amount: formatRelayAmount(usage.remaining, usage.unit), ratio, tone };
}
