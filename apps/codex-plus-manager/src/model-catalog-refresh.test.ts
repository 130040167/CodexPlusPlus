import assert from "node:assert/strict";
import { it } from "node:test";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const source = await readFile(new URL("../../../assets/inject/renderer-inject/70-model-catalog.js", import.meta.url), "utf8");
const preparationSource = await readFile(new URL("../../../assets/inject/renderer-inject/30-service-tier.js", import.meta.url), "utf8");
function runtime(sharedWindow: any = {}) {
  let now = 10000;
  let nextTimerId = 0;
  let settings: any = { relayProfilesEnabled: true, activeRelayId: "codex-plus-gateway",
    relayProfiles: [{ id: "codex-plus-gateway", relayMode: "pureApi", model: "a/sol", modelList: "a/sol" }] };
  const pending: Array<(value: any) => void> = [], rejected: Array<(error: Error) => void> = [], invalidated: any[] = [];
  const diagnostics: Array<{ name: string; details: any }> = [];
  const timers = new Map<number, { at: number; callback: () => void }>();
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
    postJson: () => new Promise((resolve, reject) => { pending.push(resolve); rejected.push(reject); }),
    setTimeout: (callback: () => void, delay: number) => {
      const id = ++nextTimerId;
      timers.set(id, { at: now + delay, callback });
      return id;
    },
    clearTimeout: (id: number) => timers.delete(id),
    findCodexQueryClient: () => ({
      getQueryCache: () => ({ getAll: () => queries }),
      invalidateQueries: (filter: any) => { invalidated.push(filter.queryKey); },
    }),
    renderCodexPlusMenu: () => {}, scheduleCodexModelWhitelistRefresh: () => {},
    codexPlusModelPickerCatalogChanged: () => {},
    sendCodexPlusDiagnostic: (name: string, details: any) => diagnostics.push({ name, details }),
  });
  const declarations = source.slice(source.indexOf("  let codexModelCatalog ="), source.indexOf("  if (window.__CODEX_PLUS_TEST_SERVICE_TIER__)"));
  const functions = source.slice(source.indexOf("  function codexPlusModelUnlockEnabled()"), source.indexOf("  function appServerModelRequestMethod("));
  const preparation = preparationSource.slice(preparationSource.indexOf("  const codexAppServerPreparationTimeoutMs ="),
    preparationSource.indexOf("  async function loadBackendSettingsState("));
  assert.ok(preparation.includes("async function readCodexAppServerPreparation("));
  const api = vm.runInContext(`(() => {${preparation}${declarations}${functions}
    return { loadCodexModelCatalog, refreshCodexModelCatalogForSettings, patchModelArray, patchModelContainer,
      installModelJsonResponsePatch, patchStatsigModelWhitelist,
      catalog: () => codexModelCatalog }; })()`, context);
  return { api, pending, rejected, invalidated, diagnostics, queries, window: sharedWindow, tick: (duration = 11000) => {
      const until = now + duration;
      for (;;) {
        const next = [...timers.entries()].sort((left, right) => left[1].at - right[1].at)[0];
        if (!next || next[1].at > until) break;
        now = next[1].at;
        timers.delete(next[0]);
        next[1].callback();
      }
      now = until;
    },
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
const settle = async () => { for (let n = 0; n < 20; n++) await Promise.resolve(); };

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
  const models: any[] = [{ model: "a/sol" }];
  r.api.patchModelArray(models, true, "local");
  assert.deepEqual([...models[0].supportedReasoningEfforts].map((entry: any) => entry.reasoningEffort), ["high", "max", "ultra"]);
  assert.deepEqual(r.api.catalog().modelMetadata["a/sol"].supportedReasoningEfforts, [{ reasoningEffort: "high" }]);
});
it("a hidden default group retains its model identity without being unhidden by catalog patches", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["group/1", "a/sol"], {
    "group/1": { displayName: "Routing group", hidden: true },
  }));
  await first;
  const value = { hostId: "local", models: [{ model: "group/1", hidden: false }],
    hiddenModels: ["group/1", "a/sol"], defaultModel: "group/1" };
  r.api.patchModelContainer(value);
  assert.equal(value.models.find(model => model.model === "group/1")?.hidden, true);
  assert.equal(value.models.find(model => model.model === "a/sol")?.hidden, false);
  assert.deepEqual([...value.hiddenModels], ["group/1"]);
  assert.equal(value.defaultModel, "group/1", "hiding a group must not replace the configured default");
  const models: any[] = [];
  r.api.patchModelArray(models, true, "local");
  assert.equal(models.find(model => model.model === "group/1").hidden, true, "new descriptors also respect visibility");
  const refreshed = r.api.loadCodexModelCatalog(true);
  r.pending.shift()!(catalog(["group/1", "a/sol"], { "group/1": { displayName: "Routing group" } }));
  await refreshed;
  r.api.patchModelArray(models, true, "local");
  assert.equal(models.find(model => model.model === "group/1").hidden, false, "showing the group must restore its entry");
});
it("temporary failures keep the last successful catalog and retry with bounded backoff", async () => {
  const r = runtime(), first = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/sol", "b/sol"]));
  r.rejected.shift();
  await first;
  const failed = r.api.loadCodexModelCatalog(true);
  r.pending.shift();
  r.rejected.shift()!(new Error("bridge disconnected"));
  assert.equal((await failed).status, "failed");
  assert.equal(r.api.catalog().status, "ok");
  assert.deepEqual([...r.api.catalog().models], ["a/sol", "b/sol"]);
  assert.equal((await r.api.loadCodexModelCatalog()).status, "failed");
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
it("a timed-out initial read rejects its late result and permits retry after cooldown", async () => {
  const r = runtime();
  let finished = false;
  const first = r.api.loadCodexModelCatalog().then((value: any) => { finished = true; return value; });
  const same = r.api.loadCodexModelCatalog();
  const resolveLate = r.pending.shift()!;
  assert.equal(r.pending.length, 0);
  r.tick(1999);
  await settle();
  assert.equal(finished, false);
  r.tick(1);
  const failure = await first;
  assert.equal(failure.status, "failed");
  assert.match(failure.message, /preparation read timed out: \/codex-model-catalog/);
  assert.equal((await same).status, "failed");
  assert.equal((await r.api.loadCodexModelCatalog()).status, "failed");
  assert.equal(r.pending.length, 0);
  resolveLate(catalog(["a/late"]));
  await settle();
  assert.equal(r.api.catalog().status, "failed");
  assert.equal(r.invalidated.length, 0);
  assert.ok(r.diagnostics.some((entry) => entry.name === "app_server_preparation_read_timeout"));
  r.tick(1000);
  const retry = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/retry"]));
  assert.equal((await retry).status, "ok");
  assert.deepEqual([...r.api.catalog().models], ["a/retry"]);
});
it("a timed-out refresh preserves the visible snapshot without interrupting a new generation", async () => {
  const r = runtime();
  const initial = r.api.loadCodexModelCatalog();
  r.pending.shift()!(catalog(["a/previous"]));
  await initial;
  const old = r.api.loadCodexModelCatalog(true);
  const resolveLate = r.pending.shift()!;
  r.tick(2000);
  assert.equal((await old).status, "failed");
  assert.deepEqual([...r.api.catalog().models], ["a/previous"]);
  assert.equal((await r.api.loadCodexModelCatalog()).status, "failed");
  const fresh = r.api.loadCodexModelCatalog(true);
  const resolveFresh = r.pending.shift()!;
  resolveLate(catalog(["a/late"]));
  await settle();
  assert.deepEqual([...r.api.catalog().models], ["a/previous"]);
  const joined = r.api.loadCodexModelCatalog();
  assert.equal(r.pending.length, 0, "the late read must not clear the new in-flight request");
  resolveFresh(catalog(["a/fresh"]));
  assert.equal((await fresh).status, "ok");
  assert.equal((await joined).status, "ok");
  assert.deepEqual([...r.api.catalog().models], ["a/fresh"]);
  assert.equal((await r.api.loadCodexModelCatalog()).status, "ok");
  assert.equal(r.invalidated.length, 2);
});
it("an older generation timing out cannot clear or fail a newer in-flight load", async () => {
  const r = runtime();
  const old = r.api.loadCodexModelCatalog();
  const resolveOld = r.pending.shift()!;
  r.tick(500);
  const fresh = r.api.loadCodexModelCatalog(true);
  const resolveFresh = r.pending.shift()!;
  r.tick(1500);
  await old;
  assert.equal(r.api.catalog().status, "loading");
  assert.equal(r.diagnostics.some((entry) => entry.name === "model_catalog_refresh_failed"), false);
  const joined = r.api.loadCodexModelCatalog();
  assert.equal(r.pending.length, 0);
  resolveFresh(catalog(["a/fresh"]));
  assert.equal((await fresh).status, "ok");
  assert.equal((await joined).status, "ok");
  resolveOld(catalog(["a/late"]));
  await settle();
  assert.deepEqual([...r.api.catalog().models], ["a/fresh"]);
  assert.equal(r.invalidated.length, 1);
});
it("a late fallback settings response cannot replace a newer catalog", async () => {
  const r = runtime();
  const old = r.api.loadCodexModelCatalog();
  r.pending.shift()!({ status: "not_configured", models: [], sources: [] });
  await settle();
  const resolveFallback = r.pending.shift()!;
  r.tick(2000);
  await old;
  const fresh = r.api.loadCodexModelCatalog(true);
  const resolveFresh = r.pending.shift()!;
  resolveFallback({ activeRelayId: "old", relayProfiles: [{ id: "old", modelList: "a/late" }] });
  await settle();
  resolveFresh(catalog(["a/fresh"]));
  await fresh;
  assert.deepEqual([...r.api.catalog().models], ["a/fresh"]);
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
