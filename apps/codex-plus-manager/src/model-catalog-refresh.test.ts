import assert from "node:assert/strict";
import { it } from "node:test";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const source = await readFile(new URL("../../../assets/inject/renderer-inject/70-model-catalog.js", import.meta.url), "utf8");
function runtime(sharedWindow: any = {}) {
  let now = 10000;
  let settings: any = { relayProfilesEnabled: true, activeRelayId: "codex-plus-gateway",
    relayProfiles: [{ id: "codex-plus-gateway", relayMode: "pureApi", model: "a/sol", modelList: "a/sol" }] };
  const pending: Array<(value: any) => void> = [], rejected: Array<(error: Error) => void> = [], invalidated: any[] = [];
  const queries = [
    { queryKey: ["models", "local"] }, { queryKey: ["models", "ssh:remote"] },
    { queryKey: ["threads", "local"] }, { queryKey: ["model-usage", "local"] },
  ];
  const Response = sharedWindow.testResponse ||= class {
    payload: any;
    constructor(payload: any) { this.payload = payload; }
    async json() { return this.payload; }
  };
  const context = vm.createContext({
    window: sharedWindow, Response, Map, WeakMap, Set, Promise, Date: { now: () => now },
    uniqueValues: (values: any[]) => [...new Set(values.filter(value => typeof value === "string" && value.trim()))],
    codexPlusSettings: () => ({ modelWhitelistUnlock: true }),
    codexPlusBackendSettingsLoaded: true, codexPlusBackendSettings: settings,
    codexRemoteSessionActiveProfile: () => settings.relayProfiles.find((entry: any) => entry.id === settings.activeRelayId),
    codexServiceTierModelFromValue: (value: any) => value,
    readCodexAppServerPreparation: () => new Promise((resolve, reject) => { pending.push(resolve); rejected.push(reject); }),
    findCodexQueryClient: () => ({
      getQueryCache: () => ({ getAll: () => queries }),
      invalidateQueries: (filter: any) => { invalidated.push(filter.queryKey); },
    }),
    renderCodexPlusMenu: () => {}, scheduleCodexModelWhitelistRefresh: () => {},
    codexPlusModelPickerCatalogChanged: () => {}, sendCodexPlusDiagnostic: () => {},
  });
  const declarations = source.slice(source.indexOf("  let codexModelCatalog ="), source.indexOf("  if (window.__CODEX_PLUS_TEST_SERVICE_TIER__)"));
  const functions = source.slice(source.indexOf("  function codexPlusModelUnlockEnabled()"), source.indexOf("  function appServerModelRequestMethod("));
  const api = vm.runInContext(`(() => {${declarations}${functions}
    return { loadCodexModelCatalog, refreshCodexModelCatalogForSettings, patchModelArray,
      installModelJsonResponsePatch, patchStatsigModelWhitelist,
      catalog: () => codexModelCatalog }; })()`, context);
  return { api, pending, rejected, invalidated, queries, window: sharedWindow, tick: () => { now += 11000; },
    update: (next: any) => {
      const previous = settings;
      settings = next;
      context.codexPlusBackendSettings = settings;
      api.refreshCodexModelCatalogForSettings(previous, next);
    }, settings: () => settings };
}
const catalog = (models: string[], metadata = {}) => ({ status: "ok", model_provider: "codex-plus-gateway",
  model: models[0], default_model: models[0], models, modelMetadata: metadata,
  sources: [{ id: "relay-profile:codex-plus-gateway", type: "relay_profile_model_list" }] });
const settle = async () => { for (let n = 0; n < 8; n++) await Promise.resolve(); };

it("gateway changes refresh immediately within the cache window and remove stale native models", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol", "b/sol"]));
  await first;
  const models = [{ model: "native-unavailable" }];
  r.api.patchModelArray(models, true, "local");
  assert.deepEqual(models.map(entry => entry.model), ["a/sol", "b/sol"]);
  r.update({ ...r.settings(), relayProfiles: [{ ...r.settings().relayProfiles[0],
    modelList: "a/sol\ngroup/coding", modelMetadata: '{"group/coding":{"display_name":"Coding"}}' }] });
  assert.equal(r.pending.length, 1, "heartbeat change must bypass the ten-second cache");
  r.pending.shift()!(catalog(["a/sol", "group/coding"]));
  await settle();
  r.api.patchModelArray(models, true, "local");
  assert.deepEqual(models.map(entry => entry.model), ["a/sol", "group/coding"]);
  assert.equal(r.invalidated.length, 2);
  assert.deepEqual([...r.invalidated[0]], ["models", "local"]);
});
it("late catalog reads from a previous configuration cannot overwrite the new catalog", async () => {
  const r = runtime(), old = r.api.loadCodexModelCatalog();
  const resolveOld = r.pending.shift()!;
  r.update({ ...r.settings(), relayProfiles: [{ ...r.settings().relayProfiles[0], modelList: "a/astra" }] });
  const resolveNew = r.pending.shift()!;
  resolveNew(catalog(["a/astra"]));
  await settle();
  resolveOld(catalog(["a/sol"]));
  await old;
  assert.deepEqual([...r.api.catalog().models], ["a/astra"]);
});
it("unchanged settings and unrelated preferences do not refetch model queries", async () => {
  const r = runtime();
  r.update({ ...r.settings(), codexAppTypingEffects: true });
  assert.equal(r.pending.length, 0);
  assert.equal(r.invalidated.length, 0);
});
it("same-id gateway metadata changes update names, reasoning levels and favorites", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol"], { "a/sol": { displayName: "Sol old" } }));
  await first;
  r.update({ ...r.settings(), relayProfiles: [{ ...r.settings().relayProfiles[0], modelMetadata: "changed" }] });
  r.pending.shift()!(catalog(["a/sol"], { "a/sol": { displayName: "Sol new",
    codex_plus_favorite: true, supportedReasoningEfforts: [{ reasoningEffort: "high" }] } }));
  await settle();
  assert.equal(r.api.catalog().modelMetadata["a/sol"].displayName, "Sol new");
  assert.equal(r.api.catalog().modelMetadata["a/sol"].codex_plus_favorite, true);
});
it("temporary failures keep the last successful catalog and retry with bounded backoff", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol", "b/sol"]));
  r.rejected.shift();
  await first;
  const failed = r.api.loadCodexModelCatalog(true);
  r.pending.shift();
  r.rejected.shift()!(new Error("bridge disconnected"));
  await failed;
  assert.equal(r.api.catalog().status, "ok");
  assert.deepEqual([...r.api.catalog().models], ["a/sol", "b/sol"]);
  await r.api.loadCodexModelCatalog();
  assert.equal(r.pending.length, 0, "scan ticks must not flood a disconnected backend");
  r.tick();
  const retry = r.api.loadCodexModelCatalog();
  r.pending.shift()!({ status: "failed", models: [], message: "backend restarting" });
  await retry;
  assert.deepEqual([...r.api.catalog().models], ["a/sol", "b/sol"]);
  assert.equal(r.invalidated.length, 1, "failures must not invalidate native model data");
  r.tick();
  r.update({ ...r.settings() });
  assert.equal(r.pending.length, 1, "settings heartbeat retries even when the configuration has not changed");
  r.pending.shift()!(catalog(["a/new"]));
  await settle();
  assert.deepEqual([...r.api.catalog().models], ["a/new"]);
});
it("hot reinjection inherits the successful catalog and ignores reads from the old runtime", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol"]));
  await first;
  const late = r.api.loadCodexModelCatalog(true), resolveLate = r.pending.shift()!;
  const next = runtime(r.window);
  assert.deepEqual([...next.api.catalog().models], ["a/sol"]);
  const fresh = next.api.loadCodexModelCatalog();
  next.pending.shift()!(catalog(["a/astra"]));
  await fresh;
  resolveLate(catalog(["a/old"]));
  await late;
  const reloaded = runtime(r.window);
  assert.deepEqual([...reloaded.api.catalog().models], ["a/astra"]);
});
it("switching providers discards the old snapshot even when the next catalog fails", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol"]));
  await first;
  r.update({ ...r.settings(), activeRelayId: "another" });
  r.pending.shift()!({ status: "failed", models: [] });
  await settle();
  assert.equal(r.api.catalog().status, "failed");
  assert.deepEqual([...r.api.catalog().models], []);
});
it("overlapping forced reads only publish the newest response", async () => {
  const r = runtime(), old = r.api.loadCodexModelCatalog(true), latest = r.api.loadCodexModelCatalog(true);
  const resolveOld = r.pending.shift()!, resolveLatest = r.pending.shift()!;
  resolveLatest(catalog(["a/new"]));
  await latest;
  resolveOld(catalog(["a/old"]));
  await old;
  assert.deepEqual([...r.api.catalog().models], ["a/new"]);
});
it("installed JSON and Statsig callbacks use the current runtime after hot reinjection", async () => {
  const config = { value: { hostId: "local", available_models: ["native"] } };
  const window: any = { __STATSIG__: { firstInstance: { getDynamicConfig: () => config } } };
  const first = runtime(window), loaded = first.api.loadCodexModelCatalog();
  first.pending.shift()!(catalog(["a/old"]));
  await loaded;
  first.api.installModelJsonResponsePatch();
  first.api.patchStatsigModelWhitelist();
  const responseJson = window.testResponse.prototype.json;
  const getConfig = window.__STATSIG__.firstInstance.getDynamicConfig;
  const next = runtime(window), refreshed = next.api.loadCodexModelCatalog();
  next.pending.shift()!(catalog(["a/new", "b/sol"]));
  await refreshed;
  next.api.installModelJsonResponsePatch();
  next.api.patchStatsigModelWhitelist();
  assert.equal(window.testResponse.prototype.json, responseJson, "do not stack wrappers on reinjection");
  assert.equal(window.__STATSIG__.firstInstance.getDynamicConfig, getConfig);
  const response = await new window.testResponse({ hostId: "local", models: [{ model: "a/old" }] }).json();
  assert.deepEqual([...response.models].map(entry => entry.model), ["a/new", "b/sol"]);
  assert.deepEqual([...getConfig("107580212").value.available_models], ["a/new", "b/sol"]);
  first.api.installModelJsonResponsePatch();
  first.api.patchStatsigModelWhitelist();
  assert.deepEqual([...getConfig("107580212").value.available_models], ["a/new", "b/sol"]);
});
