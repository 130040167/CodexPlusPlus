// 供应商卡片上的 sub2api 倍率 / 余额行。直连供应商和网关供应商池共用：
// 两边只是识别请求不同（直连传整个 profile，网关按 id 由后端取 Key）。
import { useCallback, useEffect, useRef, useState } from "react";
import { RefreshCw } from "lucide-react";
import { t, tf } from "./i18n";
import {
  formatSub2ApiAmount,
  formatSub2ApiMultiplier,
  rememberSub2ApiNegative,
  sub2apiAppliedPeak,
  sub2apiNegativeCached,
  SUB2API_POSITIVE_TTL_MS,
  type Sub2ApiProbe,
} from "./sub2api-status";

type ProbeResponse = Partial<Sub2ApiProbe> & { status?: string };

// 是 sub2api 的只在内存里缓存（余额会变），不是的记在 localStorage。
const probeCache = new Map<string, { probe: Sub2ApiProbe; at: number }>();
const probeInflight = new Map<string, Promise<Sub2ApiProbe | null>>();

function storage(): Storage | null {
  try { return window.localStorage; } catch { return null; }
}

function requestProbe(key: string, request: () => Promise<ProbeResponse>): Promise<Sub2ApiProbe | null> {
  const pending = probeInflight.get(key);
  if (pending) return pending;
  const next = request()
    .then((result) => {
      // 命令本身失败（如网关里已删掉这个供应商）不算识别结论，下次再试。
      if (result.status && result.status !== "ok") return null;
      const probe: Sub2ApiProbe = {
        isSub2api: result.isSub2api === true,
        billing: result.billing ?? null,
        usage: result.usage ?? null,
        detail: result.detail ?? "",
      };
      if (probe.isSub2api) probeCache.set(key, { probe, at: Date.now() });
      else probeCache.delete(key);
      rememberSub2ApiNegative(storage(), key, !probe.isSub2api);
      return probe;
    })
    // 识别失败不打扰用户，卡片照常显示，只是没有 sub2api 这一行。
    .catch(() => null)
    .finally(() => probeInflight.delete(key));
  probeInflight.set(key, next);
  return next;
}

/** `key` 为空表示这个供应商不识别；key 变化（换地址 / Key）会重新识别。 */
export function useSub2ApiProbe(key: string, request: () => Promise<ProbeResponse>) {
  const [state, setState] = useState<{ key: string; probe: Sub2ApiProbe | null; loading: boolean }>(() => ({
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
      if (cached && Date.now() - cached.at < SUB2API_POSITIVE_TTL_MS) {
        setState({ key, probe: cached.probe, loading: false });
        return;
      }
      if (!cached && sub2apiNegativeCached(storage(), key)) {
        setState({ key, probe: null, loading: false });
        return;
      }
    }
    setState({ key, probe: cached?.probe ?? null, loading: true });
    void requestProbe(key, () => requestRef.current()).then((probe) => {
      setState((current) => current.key === key
        ? { key, probe: probe ? (probe.isSub2api ? probe : null) : current.probe, loading: false }
        : current);
    });
  }, [key]);

  useEffect(() => { load(false); }, [load]);

  const probe = state.key === key ? state.probe : null;
  return { probe, loading: state.key === key && state.loading, refresh: () => load(true) };
}

export function Sub2ApiCardLine({ probe, loading, onRefresh }: { probe: Sub2ApiProbe; loading: boolean; onRefresh: () => void }) {
  const { billing, usage } = probe;
  const peak = sub2apiAppliedPeak(billing);
  const parts: { text: string; tone?: string }[] = [];
  if (billing) {
    parts.push({ text: tf("倍率 {0}", [formatSub2ApiMultiplier(billing.effectiveRateMultiplier)]) });
    if (peak !== null) parts.push({ text: tf("含高峰 ×{0}", [String(Number(peak.toFixed(4)))]), tone: "warning" });
  }
  if (usage) {
    if (!usage.isValid) parts.push({ text: t("Key 已失效"), tone: "danger" });
    if (usage.remaining !== null) {
      const amount = formatSub2ApiAmount(usage.remaining, usage.unit);
      const tone = usage.remaining <= 0 ? "danger" : undefined;
      if (usage.kind === "keyQuota") {
        parts.push({
          text: usage.quotaLimit !== null
            ? tf("Key 额度剩余 {0} / {1}", [amount, formatSub2ApiAmount(usage.quotaLimit, usage.unit)])
            : tf("Key 额度剩余 {0}", [amount]),
          tone,
        });
      } else if (usage.kind === "subscription") {
        parts.push({ text: usage.planName ? tf("{0} 剩余 {1}", [usage.planName, amount]) : tf("订阅剩余 {0}", [amount]), tone });
      } else {
        parts.push({ text: tf("余额 {0}", [amount]), tone });
      }
    } else if (usage.kind === "subscription" && usage.planName) {
      parts.push({ text: usage.planName });
    }
    for (const rateLimit of usage.rateLimits) {
      parts.push({
        text: tf("{0} 剩 {1}", [rateLimit.window, formatSub2ApiAmount(rateLimit.remaining, usage.unit)]),
        tone: rateLimit.remaining <= 0 ? "danger" : undefined,
      });
    }
  }
  const title = [
    billing ? tf("倍率接口：{0}", [billing.endpoint]) : t("这个 sub2api 版本没有倍率接口"),
    usage ? tf("余额接口：{0}", [usage.endpoint]) : t("余额接口不可用"),
  ].join("\n");
  return (
    <span className="relay-sub2api-line" title={title}>
      <span className="relay-tag" data-tone="sub2api">sub2api</span>
      {parts.map((part, index) => (
        <span className="relay-sub2api-part" data-tone={part.tone} key={index}>{part.text}</span>
      ))}
      <button
        aria-label={t("刷新 sub2api 倍率和余额")}
        className="relay-sub2api-refresh"
        disabled={loading}
        onClick={(event) => {
          event.stopPropagation();
          onRefresh();
        }}
        onKeyDown={(event) => event.stopPropagation()}
        title={t("刷新 sub2api 倍率和余额")}
        type="button"
      >
        <RefreshCw className={`h-3 w-3 ${loading ? "animate-spin" : ""}`} />
      </button>
    </span>
  );
}
