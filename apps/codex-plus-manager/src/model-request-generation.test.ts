import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { it } from "node:test";
import vm from "node:vm";

const catalog = readFileSync(new URL("../../../assets/inject/renderer-inject/70-model-catalog.js", import.meta.url), "utf8");
const service = readFileSync(new URL("../../../assets/inject/renderer-inject/30-service-tier.js", import.meta.url), "utf8");
const prelude = readFileSync(new URL("../../../assets/inject/renderer-inject/00-prelude.js", import.meta.url), "utf8");
const version = prelude.match(/const codexAppServerModelRequestPatchVersion = "([^"]+)"/)![1];
function productionFunction(name: string) {
  const match = new RegExp(`  (?:async )?function ${name}\\(`).exec(catalog)!;
  assert.ok(match, name);
  return catalog.slice(match.index, catalog.indexOf("\n  }\n", match.index) + 5);
}
const production = [
  service.slice(service.indexOf("  function codexManagedSessionConnection()"),
    service.indexOf("  function codexRemoteSessionStartedThreadId(")),
  catalog.slice(catalog.indexOf("  function codexRememberedThreadModels()"),
    catalog.indexOf("  function codexPerModelContextEnabled()")),
  ...["codexPureApiAllowedModelSet", "codexAggregateAllowedModelSet", "codexAggregateSelectedModelTarget",
    "normalizeCodexConfiguredModelRequest", "codexPerModelContextEnabled", "codexThreadModelRequestState",
    "refreshCodexThreadModelBeforeTurn", "codexAppServerNativeModelMethod", "patchAppServerModelRequestClient",
    "installCodexAppServerClientPrototypePatch", "sendAppServerRequestWithSideChat"].map(productionFunction),
].join("\n");

function fixture() {
  const calls: any[] = [];
  const transforms: string[] = [];
  const storage = new Map<string, string>();
  class Client {
    hostId = "local";
    async sendRequest(method: string, params: any): Promise<any> {
      if (method === "send-cli-request-for-host") return this.nativeEnvelope(params);
      assert.equal(this.hostId === "local" || this.hostId === "ssh:test", true);
      calls.push({ method, params });
      return { thread: { id: params.threadId || "thread" }, modelProvider: params.modelProvider };
    }
    async nativeEnvelope(params: any) {
      return nativeSend.call(this, params.method, params.params);
    }
    async prewarmThreadStart(params: any) { return nativeSend.call(this, "thread/start", params); }
  }
  const nativeSend = Client.prototype.sendRequest;
  const nativePrewarm = Client.prototype.prewarmThreadStart;
  const client: any = new Client();
  const window: any = { __codexPlusAppServerClientClass: Client };
  function inject(mode: "aggregate" | "pureApi" | "official", port: number, gate?: Promise<boolean>) {
    const generation = {};
    window.__codexPlusRemoteSessionRecoveryGeneration = generation;
    const profile = { id: mode === "aggregate" ? "aggregate" : "hanhe", relayMode: mode };
    const connection = { profileId: profile.id, provider: "codex_plus_managed",
      baseUrl: `http://127.0.0.1:${port}/v1`, epoch: port };
    const models = mode === "aggregate" ? ["other-only", "cpx-hanhe/shared"] : ["shared"];
    const context = vm.createContext({
      window, codexRemoteSessionRecoveryGeneration: generation, codexAppServerModelRequestPatchVersion: version,
      document: { querySelectorAll: () => [] },
      localStorage: { getItem: (key: string) => storage.get(key), setItem: (key: string, value: string) => storage.set(key, value) },
      codexPlusBackendSettingsLoaded: true, codexPlusBackendSettings: { managedSessionConnection: mode === "official" ? null : connection },
      codexModelCatalog: { status: "ok", models, codex_plus_default_group_id: "other",
        sources: [{ id: mode === "aggregate" ? "aggregate:aggregate" : "relay-profile:hanhe",
          type: mode === "aggregate" ? "aggregate_model_catalog" : "relay_profile" }] },
      codexRemoteSessionActiveProfile: () => profile,
      codexPlusModelNames: () => models,
      codexPlusModelMetadata: (model: string) => ({ codex_plus_group_id: model.startsWith("cpx-hanhe/") ? "hanhe" : "other" }),
      codexPlusConfiguredDefaultModel: () => models[0],
      modelReasoningEfforts: () => ["low", "medium", "high"].map(reasoningEffort => ({ reasoningEffort })),
      codexPlusModelUnlockEnabled: () => mode !== "official", uniqueValues: (values: any) => [...new Set(values)],
      codexPluginMarketplacePatchEnabled: () => false, codexPlusSettings: () => ({}),
      codexRemoteSessionProviderPatchEnabled: () => mode !== "official", codexRemoteSessionProviderOverrideEnabled: () => false,
      codexRemoteSessionProviderRequestMethod: () => true,
      applyCodexRemoteSessionProviderOverride: (_method: string, params: any) => params,
      applyCodexServiceTierRequestOnly: (_method: string, params: any) => params,
      codexRemoteSessionTargetProvider: () => "custom",
      appServerModelRequestMethod: (method: string, params: any) => method === "send-cli-request-for-host" ? params.method : method,
      modelResponseHostId: (params: any) => params.hostId || "local",
      registerNativeHostClient() {}, codexPlusPluginNativeInterceptClient: () => null,
      prepareCodexPureApiModelCatalog: async () => gate ? await gate : true,
      loadBackendSettingsState: async () => true, codexAppServerPreparationTimeoutMs: 1500,
      validThreadScrollSessionKey: (value: any) => value,
      sendCodexPlusDiagnostic() {}, nativeHostClient: () => client,
      recordTransform: () => transforms.push(`${mode}:${port}`), setTimeout, clearTimeout,
    });
    const api = vm.runInContext(`(() => { ${production};
      const retained = sendCodexRetainedModelRequest;
      sendCodexRetainedModelRequest = (...args) => { recordTransform(); return retained(...args); };
      return {
      patch: patchAppServerModelRequestClient, prototype: installCodexAppServerClientPrototypePatch
    }; })()`, context);
    return { ...api, generation, connection };
  }
  return { client, Client, window, calls, transforms, inject, nativeSend, nativePrewarm };
}

for (const entry of ["instance", "prototype", "instance+prototype"] as const) {
  it(`${entry} rebinds aggregate/direct/aggregate generations without stacking transforms`, async () => {
    const f = fixture();
    const install = (api: any) => {
      if (entry !== "prototype") assert.equal(api.patch(f.client), true);
      if (entry !== "instance") assert.equal(api.prototype(), true);
    };
    const old = f.inject("aggregate", 62021); install(old);
    await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
    assert.equal(f.calls.at(-1).params.model, "cpx-hanhe/shared");
    const direct = f.inject("pureApi", 62022); install(direct);
    await f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared" });
    assert.equal(f.calls.at(-1).params.model, "shared");
    const staleSend = f.client.sendRequest;
    const stalePrewarm = f.client.prewarmThreadStart;
    const current = f.inject("aggregate", 62023);
    // 原型安装也必须重绑已登记且覆盖原型的实例。
    if (entry === "instance+prototype") assert.equal(current.prototype(), true);
    else install(current);
    const installedSend = f.client.sendRequest, installedPrewarm = f.client.prewarmThreadStart;
    for (let i = 0; i < 5; i++) install(current);
    assert.equal(f.client.sendRequest, installedSend);
    assert.equal(f.client.prewarmThreadStart, installedPrewarm);
    assert.equal(old.patch(f.client), false);
    assert.equal(direct.prototype(), false);
    assert.equal(f.client.sendRequest, installedSend, "late old installer cannot reclaim the wrapper");
    await assert.rejects(staleSend.call(f.client, "thread/start", { model: "cpx-hanhe/shared" }), /连接配置已变化/);
    await assert.rejects(stalePrewarm.call(f.client, { model: "cpx-hanhe/shared" }), /连接配置已变化/);
    f.calls.length = 0; f.transforms.length = 0;
    await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
    await f.client.sendRequest("turn/start", { threadId: "thread", model: "cpx-hanhe/shared" });
    assert.deepEqual(f.calls.map(call => call.method), ["thread/start", "turn/start"]);
    for (const call of f.calls) assert.equal(call.params.model, "cpx-hanhe/shared");
    assert.equal(f.calls[0].params.config["model_providers.codex_plus_managed"].base_url, current.connection.baseUrl);
    assert.deepEqual(f.transforms, ["aggregate:62023"], "exactly one current transform for the actual turn");
    assert.equal(f.client.__codexPlusModelOriginalSendRequest === undefined, false);
    assert.equal((f.Client.prototype as any).__codexPlusModelOriginalSendRequest || f.nativeSend, f.nativeSend);
  });
}

for (const [entry, operation] of (["instance", "prototype"] as const).flatMap(entry =>
  (["request", "prewarm"] as const).map(operation => [entry, operation] as const))) {
  it(`${entry} stops an in-flight old ${operation} preparation after reinjection`, async () => {
    const f = fixture();
    let release!: (ready: boolean) => void;
    const old = f.inject("pureApi", 62021, new Promise(resolve => { release = resolve; }));
    entry === "instance" ? old.patch(f.client) : old.prototype();
    const pending = operation === "request"
      ? f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared" })
      : f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
    const current = f.inject("aggregate", 62022);
    entry === "instance" ? current.patch(f.client) : current.prototype();
    release(true);
    await assert.rejects(pending, /连接配置已变化/);
    assert.equal(f.calls.length, 0);
    await f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared" });
    assert.equal(f.calls[0].params.model, "cpx-hanhe/shared");
  });
}

it("direct compatibility accepts only the selected member and leaves remote requests intact", async () => {
  const f = fixture();
  const api = f.inject("pureApi", 62021); api.prototype();
  await f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared" });
  assert.equal(f.calls.at(-1).params.model, "shared");
  const count = f.calls.length;
  await assert.rejects(f.client.sendRequest("thread/start", { model: "cpx-other/shared" }), /不属于当前供应商/);
  assert.equal(f.calls.length, count, "another member's alias cannot authorize switching suppliers");
  f.client.hostId = "ssh:test";
  await f.client.sendRequest("thread/start", { model: "cpx-other/shared" });
  assert.equal(f.calls.at(-1).params.model, "cpx-other/shared");
  assert.equal(f.calls.at(-1).params.modelProvider, undefined);
});

it("leaving managed mode rebinds previous wrappers even with all patch features disabled", async () => {
  const f = fixture();
  const old = f.inject("pureApi", 62021); old.patch(f.client); old.prototype();
  const official = f.inject("official", 62022);
  assert.equal(official.prototype(), true);
  const params = { model: "official-model" };
  await f.client.prewarmThreadStart(params);
  await f.client.sendRequest("turn/start", params);
  assert.equal(f.calls.length, 2);
  for (const call of f.calls) assert.equal(call.params, params);
});

it("host envelopes use the rebound production wrapper without rewriting remote member aliases", async () => {
  const f = fixture();
  const direct = f.inject("pureApi", 62021); direct.prototype();
  const current = f.inject("aggregate", 62022); current.prototype();
  await f.client.sendRequest("send-cli-request-for-host", { hostId: "local", method: "thread/start",
    params: { model: "cpx-hanhe/shared" } });
  assert.equal(f.calls.at(-1).params.model, "cpx-hanhe/shared");
  assert.equal(f.calls.at(-1).params.config["model_providers.codex_plus_managed"].base_url, current.connection.baseUrl);
  const remote = { model: "cpx-other/shared" };
  await f.client.sendRequest("send-cli-request-for-host", { hostId: "ssh:test", method: "thread/start", params: remote });
  assert.equal(f.calls.at(-1).params, remote);
});

it("upgrades same-version legacy instance shadows found through the native weak registry", async () => {
  const f = fixture();
  const old = async () => { throw new Error("legacy direct transform must never run"); };
  Object.assign(f.client, { sendRequest: old, prewarmThreadStart: old,
    __codexPlusModelRequestPatch: version, __codexPlusModelOriginalSendRequest: f.nativeSend,
    __codexPlusServiceTierOriginalPrewarmThreadStart: f.nativePrewarm });
  f.window.__codexPlusNativeHostClients = new Map([["local", { ref: new WeakRef(f.client) }]]);
  const current = f.inject("aggregate", 62022); current.prototype();
  await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
  await f.client.sendRequest("turn/start", { threadId: "thread", model: "cpx-hanhe/shared" });
  assert.equal(f.calls.length, 2);
  for (const call of f.calls) assert.equal(call.params.model, "cpx-hanhe/shared");
});

it("production resume retention and collaboration settings preserve the current aggregate member and effort", async () => {
  const f = fixture();
  const old = f.inject("pureApi", 62021); old.prototype();
  await f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared", reasoningEffort: "high" });
  const current = f.inject("aggregate", 62022); current.prototype();
  f.calls.length = 0;
  await f.client.sendRequest("thread/resume", { threadId: "thread", model: "cpx-hanhe/shared",
    reasoningEffort: "high", config: { model_reasoning_effort: "high" } });
  const resume = f.calls.at(-1).params;
  assert.equal(resume.model, "cpx-hanhe/shared");
  assert.equal(resume.reasoningEffort, "high");
  assert.equal(resume.config.model_reasoning_effort, "high");
  await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
  const turn = { threadId: "thread", model: "cpx-hanhe/shared", effort: "low",
    collaborationMode: { mode: "default", settings: { model: "cpx-hanhe/shared", reasoning_effort: "low" } } };
  await f.client.sendRequest("turn/start", turn);
  assert.equal(f.calls.at(-1).params, turn);
  assert.equal(f.calls.at(-1).params.model, "cpx-hanhe/shared");
  assert.equal(f.calls.at(-1).params.collaborationMode.settings.model, "cpx-hanhe/shared");
  assert.equal(f.calls.at(-1).params.effort, "low");
  assert.equal(f.calls.at(-1).params.collaborationMode.settings.reasoning_effort, "low");
  const direct = f.inject("pureApi", 62023); direct.prototype();
  await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
  await f.client.sendRequest("turn/start", turn);
  const normalized = f.calls.at(-1).params;
  assert.equal(normalized.model, "shared");
  assert.equal(normalized.collaborationMode.settings.model, "shared");
  assert.equal(normalized.effort, "low");
  assert.equal(normalized.collaborationMode.settings.reasoning_effort, "low");
});

it("instance bound legacy originals use independent prototype native backups without executing old transforms", async () => {
  const f = fixture();
  let oldCalls = 0;
  const oldSend = function codexPlusModelPatchedSendRequest() { oldCalls++; throw new Error("old send transform"); };
  const oldPrewarm = function codexPlusServiceTierPrewarmThreadStart() { oldCalls++; throw new Error("old prewarm transform"); };
  Object.assign(f.Client.prototype, { sendRequest: oldSend, prewarmThreadStart: oldPrewarm,
    __codexPlusModelOriginalSendRequest: f.nativeSend,
    __codexPlusServiceTierOriginalPrewarmThreadStart: f.nativePrewarm });
  Object.assign(f.client, { sendRequest: oldSend.bind(f.client), prewarmThreadStart: oldPrewarm.bind(f.client),
    __codexPlusModelRequestPatch: version,
    __codexPlusModelOriginalSendRequest: oldSend.bind(f.client),
    __codexPlusServiceTierOriginalPrewarmThreadStart: oldPrewarm.bind(f.client) });
  f.window.__codexPlusNativeHostClients = new Map([["local", { ref: new WeakRef(f.client) }]]);
  const current = f.inject("aggregate", 62022); current.prototype();
  await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
  await f.client.sendRequest("turn/start", { threadId: "thread", model: "cpx-hanhe/shared" });
  assert.equal(oldCalls, 0);
  assert.equal(f.calls.length, 2);
  for (const call of f.calls) assert.equal(call.params.model, "cpx-hanhe/shared");
  assert.equal(f.client.__codexPlusModelOriginalSendRequest, f.nativeSend);
  assert.equal(f.client.__codexPlusServiceTierOriginalPrewarmThreadStart, f.nativePrewarm);
});

it("opaque bound originals without a recoverable native backup block sending instead of nesting old transforms", async () => {
  const f = fixture();
  let oldCalls = 0;
  const old = function codexPlusModelPatchedSendRequest() { oldCalls++; throw new Error("old transform"); };
  Object.assign(f.Client.prototype, { sendRequest: old,
    __codexPlusModelOriginalSendRequest: old.bind(f.client), __codexPlusModelRequestPatch: version });
  const current = f.inject("aggregate", 62022); current.prototype();
  await assert.rejects(f.client.sendRequest("thread/start", { model: "cpx-hanhe/shared" }), /无法确认原生请求入口/);
  assert.equal(oldCalls, 0);
  assert.equal(f.calls.length, 0);
});

it("unwraps branded prototype originals to the native methods across manager generations", async () => {
  const f = fixture();
  const old = f.inject("pureApi", 62021); old.prototype();
  Object.assign(f.Client.prototype, { __codexPlusModelOriginalSendRequest: f.client.sendRequest,
    __codexPlusServiceTierOriginalPrewarmThreadStart: f.client.prewarmThreadStart });
  const current = f.inject("aggregate", 62022); current.prototype();
  await f.client.prewarmThreadStart({ model: "cpx-hanhe/shared" });
  await f.client.sendRequest("turn/start", { threadId: "thread", model: "cpx-hanhe/shared" });
  assert.equal(f.calls.length, 2);
  for (const call of f.calls) assert.equal(call.params.model, "cpx-hanhe/shared");
  assert.equal((f.Client.prototype as any).__codexPlusModelOriginalSendRequest, f.nativeSend);
});

it("late same-version discovery cannot patch or republish an old generation", async () => {
  const block = catalog.slice(catalog.indexOf("  const appServerModelRequestPatchMaxMisses = "),
    catalog.indexOf("  let codexNativeTitleModule = "));
  const window: any = {};
  const patched: string[] = [], diagnostics: string[] = [];
  function inject(label: string) {
    const generation = {};
    window.__codexPlusRemoteSessionRecoveryGeneration = generation;
    let resolve!: (value: any) => void;
    const pending = new Promise(value => { resolve = value; });
    const context = vm.createContext({ window, codexRemoteSessionRecoveryGeneration: generation,
      codexAppServerModelRequestPatchVersion: version,
      codexRemoteSessionProviderPatchEnabled: () => true,
      loadAppServerRequestCandidates: () => pending,
      patchAppServerModelRequestClient: () => { patched.push(label); return true; },
      sendCodexPlusDiagnostic: (event: string) => diagnostics.push(`${label}:${event}`),
      clearTimeout, Date });
    const api = vm.runInContext(`(() => { ${block}; return {
      install: installAppServerModelRequestPatch, settle: () => appServerModelRequestPatchPromise
    }; })()`, context);
    return { ...api, generation, resolve: () => resolve({ modules: [{}], candidates: [{}], sources: [] }) };
  }
  const old = inject("old"); old.install();
  const current = inject("new"); current.install(); current.install();
  current.resolve(); await current.settle();
  old.resolve(); await old.settle(); old.install();
  assert.deepEqual(patched, ["new"]);
  assert.deepEqual(diagnostics, ["new:model_app_server_request_patch_installed"]);
  assert.equal(window.__codexPlusAppServerModelRequestPatchGeneration, current.generation);
});
