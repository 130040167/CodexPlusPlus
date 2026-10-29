import assert from "node:assert/strict";
import { test } from "node:test";
import {
  formatRelayAmount,
  formatRelayMultiplier,
  rememberRelayBillingNegative,
  relayAppliedPeak,
  relayBalanceBadge,
  relayPlatformLabel,
  relayBillingGatewayCacheKey,
  relayBillingNegativeCached,
  relayBillingCacheKey,
  relayBillingProbeEligible,
  RELAY_BILLING_NEGATIVE_TTL_MS,
  type RelayRateInfo,
  type RelayBillingProfile,
  type RelayUsageInfo,
} from "./relay-billing.ts";

const profile = (patch: Partial<RelayBillingProfile> = {}): RelayBillingProfile => ({
  id: "relay-1", baseUrl: "https://relay.example.com/v1", upstreamBaseUrl: "", apiKey: "sk-a",
  relayMode: "mixedApi", officialMixApiKey: false, noAuth: false, ...patch,
});

class MemoryStorage {
  data = new Map<string, string>();
  getItem(key: string) { return this.data.get(key) ?? null; }
  setItem(key: string, value: string) { this.data.set(key, value); }
}

test("only API-key relays with a URL and key are probed", () => {
  assert.equal(relayBillingProbeEligible(profile()), true);
  assert.equal(relayBillingProbeEligible(profile({ relayMode: "pureApi" })), true);
  assert.equal(relayBillingProbeEligible(profile({ relayMode: "official" })), false);
  assert.equal(relayBillingProbeEligible(profile({ relayMode: "official", officialMixApiKey: true })), true);
  assert.equal(relayBillingProbeEligible(profile({ relayMode: "aggregate" })), false);
  assert.equal(relayBillingProbeEligible(profile({ noAuth: true })), false);
  assert.equal(relayBillingProbeEligible(profile({ apiKey: " " })), false);
  assert.equal(relayBillingProbeEligible(profile({ baseUrl: "" })), false);
  assert.equal(relayBillingProbeEligible(profile({ baseUrl: "", upstreamBaseUrl: "https://up.example.com" })), true);
});

test("cache key follows the active key and upstream URL without leaking the key", () => {
  const base = relayBillingCacheKey(profile());
  assert.ok(!base.includes("sk-a"));
  assert.equal(relayBillingCacheKey(profile({ baseUrl: "https://relay.example.com/v1/" })), base);
  assert.notEqual(relayBillingCacheKey(profile({ apiKey: "sk-b" })), base);
  assert.notEqual(relayBillingCacheKey(profile({ upstreamBaseUrl: "https://other.example.com" })), base);
  const multiKey = profile({ apiKeys: [{ id: "a", apiKey: "sk-a" }, { id: "b", apiKey: "sk-b" }], activeApiKeyId: "b" });
  assert.equal(relayBillingCacheKey(multiKey), relayBillingCacheKey(profile({ apiKey: "sk-b" })));
});

test("negative probes expire and can be cleared", () => {
  const storage = new MemoryStorage();
  rememberRelayBillingNegative(storage, "k1", true, 1000);
  rememberRelayBillingNegative(storage, "k2", true, 1000);
  assert.equal(relayBillingNegativeCached(storage, "k1", 2000), true);
  assert.equal(relayBillingNegativeCached(storage, "k1", 1000 + RELAY_BILLING_NEGATIVE_TTL_MS), false);
  rememberRelayBillingNegative(storage, "k1", false, 2000);
  assert.equal(relayBillingNegativeCached(storage, "k1", 2000), false);
  assert.equal(relayBillingNegativeCached(storage, "k2", 2000), true);
  // 过期条目在下次写入时被丢掉
  rememberRelayBillingNegative(storage, "k3", true, 1000 + RELAY_BILLING_NEGATIVE_TTL_MS);
  assert.deepEqual(Object.keys(JSON.parse(storage.getItem("codex-plus-relay-billing-negative-probes")!)), ["k3"]);
  assert.equal(relayBillingNegativeCached(null, "k3"), false);
});

test("formats multipliers, amounts and peak hints", () => {
  assert.equal(formatRelayMultiplier(0.9), "0.9x");
  assert.equal(formatRelayMultiplier(1.234567), "1.2346x");
  assert.equal(formatRelayAmount(12.345, "USD"), "$12.35");
  assert.equal(formatRelayAmount(97505834.07, "USD"), "$97,505,834.07");
  assert.equal(formatRelayAmount(3, "CNY"), "¥3.00");
  assert.equal(formatRelayAmount(3, "credits"), "3.00 credits");
  const billing = { appliedPeakMultiplier: 1.5 } as RelayRateInfo;
  assert.equal(relayAppliedPeak(billing), 1.5);
  assert.equal(relayAppliedPeak({ appliedPeakMultiplier: 1 } as RelayRateInfo), null);
  assert.equal(relayAppliedPeak({ appliedPeakMultiplier: null } as RelayRateInfo), null);
  assert.equal(relayAppliedPeak(null), null);
});

test("gateway pool entries are probed by id unless they are accounts or official vendors", () => {
  const entry = { id: "pool-1", baseUrl: "https://relay.example.com", keyCount: 1, official: false };
  const key = relayBillingGatewayCacheKey(entry);
  assert.ok(key.startsWith("gateway:pool-1|"));
  assert.equal(relayBillingGatewayCacheKey({ ...entry, baseUrl: "https://relay.example.com/" }), key);
  assert.notEqual(relayBillingGatewayCacheKey({ ...entry, keyCount: 2 }), key);
  assert.notEqual(relayBillingGatewayCacheKey({ ...entry, proxy: "http://127.0.0.1:7890" }), key);
  assert.equal(relayBillingGatewayCacheKey({ ...entry, official: true }), "");
  assert.equal(relayBillingGatewayCacheKey({ ...entry, account: "chatgpt" }), "");
  assert.equal(relayBillingGatewayCacheKey({ ...entry, keyCount: 0 }), "");
  assert.equal(relayBillingGatewayCacheKey({ ...entry, baseUrl: " " }), "");
});

test("pool badge shows remaining amount, quota ratio and exhausted tone", () => {
  const usage = (patch: Partial<RelayUsageInfo>): RelayUsageInfo => ({
    endpoint: "", kind: "wallet", isValid: true, planName: null, remaining: 12, quotaLimit: null, quotaUsed: null,
    unit: "USD", rateLimits: [], ...patch,
  });
  assert.deepEqual(relayBalanceBadge(usage({})), { amount: "$12.00", ratio: null, tone: "ok" });
  assert.deepEqual(relayBalanceBadge(usage({ kind: "keyQuota", remaining: 10, quotaLimit: 50 })), { amount: "$10.00", ratio: 0.2, tone: "mid" });
  assert.equal(relayBalanceBadge(usage({ remaining: 0 }))?.tone, "low");
  assert.equal(relayBalanceBadge(usage({ rateLimits: [{ window: "5h", limit: 10, used: 10, remaining: 0 }] }))?.tone, "low");
  assert.equal(relayBalanceBadge(usage({ remaining: null })), null);
});

test("platform labels and unlimited keys", () => {
  assert.equal(relayPlatformLabel("sub2api"), "sub2api");
  assert.equal(relayPlatformLabel("newapi"), "new-api");
  const unlimited: RelayUsageInfo = {
    endpoint: "", kind: "unlimited", isValid: true, planName: null, remaining: null, quotaLimit: null, quotaUsed: null,
    unit: "USD", rateLimits: [],
  };
  assert.equal(relayBalanceBadge(unlimited), null);
});
