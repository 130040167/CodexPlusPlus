import assert from "node:assert/strict";
import { test } from "node:test";
import {
  formatSub2ApiAmount,
  formatSub2ApiMultiplier,
  rememberSub2ApiNegative,
  sub2apiAppliedPeak,
  sub2apiBalanceBadge,
  sub2apiGatewayCacheKey,
  sub2apiNegativeCached,
  sub2apiProbeCacheKey,
  sub2apiProbeEligible,
  SUB2API_NEGATIVE_TTL_MS,
  type Sub2ApiBillingInfo,
  type Sub2ApiProbeProfile,
  type Sub2ApiUsageInfo,
} from "./sub2api-status.ts";

const profile = (patch: Partial<Sub2ApiProbeProfile> = {}): Sub2ApiProbeProfile => ({
  id: "relay-1", baseUrl: "https://relay.example.com/v1", upstreamBaseUrl: "", apiKey: "sk-a",
  relayMode: "mixedApi", officialMixApiKey: false, noAuth: false, ...patch,
});

class MemoryStorage {
  data = new Map<string, string>();
  getItem(key: string) { return this.data.get(key) ?? null; }
  setItem(key: string, value: string) { this.data.set(key, value); }
}

test("only API-key relays with a URL and key are probed", () => {
  assert.equal(sub2apiProbeEligible(profile()), true);
  assert.equal(sub2apiProbeEligible(profile({ relayMode: "pureApi" })), true);
  assert.equal(sub2apiProbeEligible(profile({ relayMode: "official" })), false);
  assert.equal(sub2apiProbeEligible(profile({ relayMode: "official", officialMixApiKey: true })), true);
  assert.equal(sub2apiProbeEligible(profile({ relayMode: "aggregate" })), false);
  assert.equal(sub2apiProbeEligible(profile({ noAuth: true })), false);
  assert.equal(sub2apiProbeEligible(profile({ apiKey: " " })), false);
  assert.equal(sub2apiProbeEligible(profile({ baseUrl: "" })), false);
  assert.equal(sub2apiProbeEligible(profile({ baseUrl: "", upstreamBaseUrl: "https://up.example.com" })), true);
});

test("cache key follows the active key and upstream URL without leaking the key", () => {
  const base = sub2apiProbeCacheKey(profile());
  assert.ok(!base.includes("sk-a"));
  assert.equal(sub2apiProbeCacheKey(profile({ baseUrl: "https://relay.example.com/v1/" })), base);
  assert.notEqual(sub2apiProbeCacheKey(profile({ apiKey: "sk-b" })), base);
  assert.notEqual(sub2apiProbeCacheKey(profile({ upstreamBaseUrl: "https://other.example.com" })), base);
  const multiKey = profile({ apiKeys: [{ id: "a", apiKey: "sk-a" }, { id: "b", apiKey: "sk-b" }], activeApiKeyId: "b" });
  assert.equal(sub2apiProbeCacheKey(multiKey), sub2apiProbeCacheKey(profile({ apiKey: "sk-b" })));
});

test("negative probes expire and can be cleared", () => {
  const storage = new MemoryStorage();
  rememberSub2ApiNegative(storage, "k1", true, 1000);
  rememberSub2ApiNegative(storage, "k2", true, 1000);
  assert.equal(sub2apiNegativeCached(storage, "k1", 2000), true);
  assert.equal(sub2apiNegativeCached(storage, "k1", 1000 + SUB2API_NEGATIVE_TTL_MS), false);
  rememberSub2ApiNegative(storage, "k1", false, 2000);
  assert.equal(sub2apiNegativeCached(storage, "k1", 2000), false);
  assert.equal(sub2apiNegativeCached(storage, "k2", 2000), true);
  // 过期条目在下次写入时被丢掉
  rememberSub2ApiNegative(storage, "k3", true, 1000 + SUB2API_NEGATIVE_TTL_MS);
  assert.deepEqual(Object.keys(JSON.parse(storage.getItem("codex-plus-sub2api-negative-probes")!)), ["k3"]);
  assert.equal(sub2apiNegativeCached(null, "k3"), false);
});

test("formats multipliers, amounts and peak hints", () => {
  assert.equal(formatSub2ApiMultiplier(0.9), "0.9x");
  assert.equal(formatSub2ApiMultiplier(1.234567), "1.2346x");
  assert.equal(formatSub2ApiAmount(12.345, "USD"), "$12.35");
  assert.equal(formatSub2ApiAmount(97505834.07, "USD"), "$97,505,834.07");
  assert.equal(formatSub2ApiAmount(3, "CNY"), "¥3.00");
  assert.equal(formatSub2ApiAmount(3, "credits"), "3.00 credits");
  const billing = { appliedPeakMultiplier: 1.5 } as Sub2ApiBillingInfo;
  assert.equal(sub2apiAppliedPeak(billing), 1.5);
  assert.equal(sub2apiAppliedPeak({ appliedPeakMultiplier: 1 } as Sub2ApiBillingInfo), null);
  assert.equal(sub2apiAppliedPeak({ appliedPeakMultiplier: null } as Sub2ApiBillingInfo), null);
  assert.equal(sub2apiAppliedPeak(null), null);
});

test("gateway pool entries are probed by id unless they are accounts or official vendors", () => {
  const entry = { id: "pool-1", baseUrl: "https://relay.example.com", keyCount: 1, official: false };
  const key = sub2apiGatewayCacheKey(entry);
  assert.ok(key.startsWith("gateway:pool-1|"));
  assert.equal(sub2apiGatewayCacheKey({ ...entry, baseUrl: "https://relay.example.com/" }), key);
  assert.notEqual(sub2apiGatewayCacheKey({ ...entry, keyCount: 2 }), key);
  assert.notEqual(sub2apiGatewayCacheKey({ ...entry, proxy: "http://127.0.0.1:7890" }), key);
  assert.equal(sub2apiGatewayCacheKey({ ...entry, official: true }), "");
  assert.equal(sub2apiGatewayCacheKey({ ...entry, account: "chatgpt" }), "");
  assert.equal(sub2apiGatewayCacheKey({ ...entry, keyCount: 0 }), "");
  assert.equal(sub2apiGatewayCacheKey({ ...entry, baseUrl: " " }), "");
});

test("pool badge shows remaining amount, quota ratio and exhausted tone", () => {
  const usage = (patch: Partial<Sub2ApiUsageInfo>): Sub2ApiUsageInfo => ({
    endpoint: "", kind: "wallet", isValid: true, planName: null, remaining: 12, quotaLimit: null, quotaUsed: null,
    unit: "USD", rateLimits: [], ...patch,
  });
  assert.deepEqual(sub2apiBalanceBadge(usage({})), { amount: "$12.00", ratio: null, tone: "ok" });
  assert.deepEqual(sub2apiBalanceBadge(usage({ kind: "keyQuota", remaining: 10, quotaLimit: 50 })), { amount: "$10.00", ratio: 0.2, tone: "mid" });
  assert.equal(sub2apiBalanceBadge(usage({ remaining: 0 }))?.tone, "low");
  assert.equal(sub2apiBalanceBadge(usage({ rateLimits: [{ window: "5h", limit: 10, used: 10, remaining: 0 }] }))?.tone, "low");
  assert.equal(sub2apiBalanceBadge(usage({ remaining: null })), null);
});
