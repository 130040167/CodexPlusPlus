// 供应商卡片上的中转站（sub2api / new-api）倍率 / 余额。直连供应商和网关供应商池共用：
// 两边只是识别请求不同（直连传整个 profile，网关按 id 由后端取 Key）。
import { useCallback, useEffect, useRef, useState, type CSSProperties } from "react";
import { RefreshCw } from "lucide-react";
import { t, tf } from "./i18n";
import {
  formatRelayAmount,
  formatRelayMultiplier,
  rememberRelayBillingNegative,
  relayAppliedPeak,
  relayPlatformLabel,
  relayBalanceBadge,
  relayBillingNegativeCached,
  RELAY_BILLING_POSITIVE_TTL_MS,
  type RelayBillingProbe,
} from "./relay-billing";

type ProbeResponse = Partial<RelayBillingProbe> & { status?: string };

// 识别出中转站的只在内存里缓存（余额会变），识别不出的记在 localStorage。
const probeCache = new Map<string, { probe: RelayBillingProbe; at: number }>();
const probeInflight = new Map<string, Promise<RelayBillingProbe | null>>();

function storage(): Storage | null {
  try { return window.localStorage; } catch { return null; }
}

function requestProbe(key: string, request: () => Promise<ProbeResponse>): Promise<RelayBillingProbe | null> {
  const pending = probeInflight.get(key);
  if (pending) return pending;
  const next = request()
    .then((result) => {
      // 命令本身失败（如网关里已删掉这个供应商）不算识别结论，下次再试。
      if (result.status && result.status !== "ok") return null;
      const platform = result.platform === "sub2api" || result.platform === "newapi" ? result.platform : null;
      const probe: RelayBillingProbe = {
        platform,
        rate: result.rate ?? null,
        usage: result.usage ?? null,
        detail: result.detail ?? "",
      };
      if (platform) probeCache.set(key, { probe, at: Date.now() });
      else probeCache.delete(key);
      rememberRelayBillingNegative(storage(), key, !platform);
      return probe;
    })
    // 识别失败不打扰用户，卡片照常显示，只是没有倍率 / 余额。
    .catch(() => null)
    .finally(() => probeInflight.delete(key));
  probeInflight.set(key, next);
  return next;
}

/** `key` 为空表示这个供应商不识别；key 变化（换地址 / Key）会重新识别。 */
export function useRelayBillingProbe(key: string, request: () => Promise<ProbeResponse>) {
  const [state, setState] = useState<{ key: string; probe: RelayBillingProbe | null; loading: boolean }>(() => ({
    key,
    probe: key ? probeCache.get(key)?.probe ?? null : null,
    loading: false,
  }));
  const requestRef = useRef(request);
  requestRef.current = request;

  const load = useCallback((force: boolean) => {
    if (!key) return;
    const cached = probeCache.get(key);
    if (!force) {
      if (cached && Date.now() - cached.at < RELAY_BILLING_POSITIVE_TTL_MS) {
        setState({ key, probe: cached.probe, loading: false });
        return;
      }
      if (!cached && relayBillingNegativeCached(storage(), key)) {
        setState({ key, probe: null, loading: false });
        return;
      }
    }
    setState({ key, probe: cached?.probe ?? null, loading: true });
    void requestProbe(key, () => requestRef.current()).then((probe) => {
      setState((current) => current.key === key
        ? { key, probe: probe ? (probe.platform ? probe : null) : current.probe, loading: false }
        : current);
    });
  }, [key]);

  useEffect(() => { load(false); }, [load]);

  const probe = state.key === key ? state.probe : null;
  return { probe, loading: state.key === key && state.loading, refresh: () => load(true) };
}

function relayBillingParts(probe: RelayBillingProbe): { text: string; tone?: string }[] {
  const { rate, usage } = probe;
  const peak = relayAppliedPeak(rate);
  const parts: { text: string; tone?: string }[] = [];
  if (rate) {
    parts.push({ text: tf("倍率 {0}", [formatRelayMultiplier(rate.multiplier)]) });
    if (peak !== null) parts.push({ text: tf("含高峰 ×{0}", [String(Number(peak.toFixed(4)))]), tone: "warning" });
  }
  if (usage) {
    if (!usage.isValid) parts.push({ text: t("Key 已失效"), tone: "danger" });
    if (usage.remaining !== null) {
      const amount = formatRelayAmount(usage.remaining, usage.unit);
      const tone = usage.remaining <= 0 ? "danger" : undefined;
      if (usage.kind === "keyQuota") {
        parts.push({
          text: usage.quotaLimit !== null
            ? tf("Key 额度剩余 {0} / {1}", [amount, formatRelayAmount(usage.quotaLimit, usage.unit)])
            : tf("Key 额度剩余 {0}", [amount]),
          tone,
        });
      } else if (usage.kind === "subscription") {
        parts.push({ text: usage.planName ? tf("{0} 剩余 {1}", [usage.planName, amount]) : tf("订阅剩余 {0}", [amount]), tone });
      } else {
        parts.push({ text: tf("余额 {0}", [amount]), tone });
      }
    } else if (usage.kind === "unlimited") {
      parts.push({ text: t("Key 不限额度") });
    } else if (usage.kind === "subscription" && usage.planName) {
      parts.push({ text: usage.planName });
    }
    for (const rateLimit of usage.rateLimits) {
      parts.push({
        text: tf("{0} 剩 {1}", [rateLimit.window, formatRelayAmount(rateLimit.remaining, usage.unit)]),
        tone: rateLimit.remaining <= 0 ? "danger" : undefined,
      });
    }
  }
  return parts;
}

function relayRateSource(probe: RelayBillingProbe): string {
  const { rate } = probe;
  if (probe.platform === "newapi") {
    if (!rate) return t("这个 Key 还没有消费记录，拿不到分组倍率");
    return rate.group
      ? tf("分组倍率取自最近一次消费记录（分组 {0}）", [rate.group])
      : t("分组倍率取自最近一次消费记录");
  }
  return rate ? tf("倍率接口：{0}", [rate.endpoint]) : t("这个 sub2api 版本没有倍率接口");
}

function relayBillingTitle(probe: RelayBillingProbe): string {
  return [
    relayRateSource(probe),
    probe.usage ? tf("余额接口：{0}", [probe.usage.endpoint]) : t("余额接口不可用"),
  ].join("\n");
}

function RelayBillingRefreshButton({ loading, onRefresh }: { loading: boolean; onRefresh: () => void }) {
  return (
    <button
      aria-label={t("刷新倍率和余额")}
      className="relay-billing-refresh"
      disabled={loading}
      onClick={(event) => {
        event.stopPropagation();
        onRefresh();
      }}
      onKeyDown={(event) => event.stopPropagation()}
      title={t("刷新倍率和余额")}
      type="button"
    >
      <RefreshCw className={`h-3 w-3 ${loading ? "animate-spin" : ""}`} />
    </button>
  );
}

export function RelayBillingLine({ probe, loading, onRefresh }: { probe: RelayBillingProbe; loading: boolean; onRefresh: () => void }) {
  if (!probe.platform) return null;
  const parts = relayBillingParts(probe);
  return (
    <span className="relay-billing-line" title={relayBillingTitle(probe)}>
      <span className="relay-tag" data-tone="billing">{relayPlatformLabel(probe.platform)}</span>
      {parts.map((part, index) => (
        <span className="relay-billing-part" data-tone={part.tone} key={index}>{part.text}</span>
      ))}
      <RelayBillingRefreshButton loading={loading} onRefresh={onRefresh} />
    </span>
  );
}

/** 网关供应商池右侧：和订阅账号的「套餐 + 额度条」同一位置、同一样式。 */
export function RelayBillingPoolLabel({ probe }: { probe: RelayBillingProbe }) {
  if (!probe.platform) return null;
  const peak = relayAppliedPeak(probe.rate);
  const platform = relayPlatformLabel(probe.platform);
  return (
    <span className="gw-billing-label" title={`${relayBillingParts(probe).map((part) => part.text).join(" · ")}\n${relayRateSource(probe)}`}>
      {probe.rate ? tf("{0} · 倍率 {1}", [platform, formatRelayMultiplier(probe.rate.multiplier)]) : platform}
      {peak !== null ? <em>{tf("含高峰 ×{0}", [String(Number(peak.toFixed(4)))])}</em> : null}
    </span>
  );
}

export function RelayBillingPoolBadge({ probe, loading, onRefresh }: { probe: RelayBillingProbe; loading: boolean; onRefresh: () => void }) {
  const badge = probe.usage ? relayBalanceBadge(probe.usage) : null;
  const details = relayBillingParts(probe).map((part) => part.text).join("\n");
  return (
    <span className="gw-billing-status">
      {badge ? (
        <span className="gw-quota-badge" data-tone={badge.tone} title={`${details}\n${relayBillingTitle(probe)}`}>
          {badge.ratio !== null ? <i style={{ "--gw-quota": `${Math.round(badge.ratio * 100)}%` } as CSSProperties} /> : null}
          {probe.usage?.kind === "wallet" ? tf("余额 {0}", [badge.amount]) : tf("剩 {0}", [badge.amount])}
        </span>
      ) : probe.usage?.kind === "unlimited" ? (
        <span className="gw-quota-badge" data-tone={probe.usage.isValid ? "ok" : "low"} title={`${details}\n${relayBillingTitle(probe)}`}>
          {t("Key 不限额度")}
        </span>
      ) : null}
      <RelayBillingRefreshButton loading={loading} onRefresh={onRefresh} />
    </span>
  );
}
