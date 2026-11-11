import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import vm from "node:vm";

const renderer = new URL("../../../assets/inject/renderer-inject/", import.meta.url);
const service = readFileSync(new URL("30-service-tier.js", renderer), "utf8");
const serviceFragment = service.slice(0, service.indexOf("  function codexRemoteSessionStartedThreadId("));
const catalog = readFileSync(new URL("70-model-catalog.js", renderer), "utf8");
const requestWrapper = catalog.slice(
  catalog.indexOf("  function patchAppServerModelRequestClient("),
  catalog.indexOf("  // issue #2177"),
);
const patchVersion = readFileSync(new URL("00-prelude.js", renderer), "utf8")
  .match(/const codexAppServerModelRequestPatchVersion = "([^"]+)";/)![1];
const rpc = readFileSync(new URL("91-extension-rpc.js", renderer), "utf8");

function runtime(mode: "aggregate" | "pureApi", source: string, status: "idle" | "notLoaded" | "active",
  providerSyncEnabled = true) {
  const calls: { method: string; params: any; provider?: string | null }[] = [];
  const preparations: any[] = [];
  const state = {
    indexedProvider: source,
    connectedProvider: status !== "notLoaded" ? source : null as string | null,
    status,
    cached: { modelProvider: source, resumeState: status === "notLoaded" ? "needs_resume" : "resumed",
      threadRuntimeStatus: { type: status } },
  };
  const profile = { id: "relay", relayMode: mode, configContents: 'model_provider = "custom"' };
  const settings = {
    relayProfilesEnabled: true, activeRelayId: "relay", providerSyncEnabled,
    relayProfiles: [profile],
    managedSessionConnection: {
      profileId: "relay", provider: "codex_plus_managed", baseUrl: "http://127.0.0.1:62021/v1",
    },
  };
  const client = {
    hostId: "local",
    async sendRequest(method: string, params: any) {
      const call = { method, params, provider: state.connectedProvider };
      calls.push(call);
      if (method === "thread/read") {
        return { thread: { id: "thread", cwd: "C:/fixture",
          model: "saved-model", reasoningEffort: "high",
          status: { type: state.status }, modelProvider: state.indexedProvider } };
      }
      if (method === "config/read") return { config: { model_provider: "custom" } };
      if (method === "thread/turns/list") return { data: [{}] };
      if (method === "thread/unsubscribe") {
        state.status = "notLoaded";
        state.connectedProvider = null;
        state.cached.resumeState = "needs_resume";
        return {};
      }
      if (method === "thread/resume") {
        if (state.status === "notLoaded") {
          state.connectedProvider = params.modelProvider ?? state.indexedProvider;
          state.status = "idle";
        }
        return { thread: { id: "thread", status: { type: state.status } },
          modelProvider: state.connectedProvider, model: params.model,
          reasoningEffort: params.config?.model_reasoning_effort ?? null };
      }
      if (method === "turn/start") return { provider: state.connectedProvider };
      throw new Error(`Unexpected request: ${method}`);
    },
    async getConversation() { return state.cached; },
    async unsubscribeInactiveConversation() {
      state.status = "notLoaded";
      state.connectedProvider = null;
      state.cached.resumeState = "needs_resume";
    },
  };
  const context = vm.createContext({
    client, window: { __codexSessionDeleteBridge: () => {} },
    codexAppServerModelRequestPatchVersion: patchVersion,
    codexPlusBackendSettings: settings, codexModelCatalog: {}, codexAppServerPreparationTimeoutMs: 100,
    registerNativeHostClient() {}, sendCodexPlusDiagnostic() {}, codexPlusPluginNativeInterceptClient: () => null,
    appServerModelRequestMethod: (method: string) => method,
    loadBackendSettingsState: async () => true, loadCodexModelCatalog: async () => ({}),
    readCodexAppServerPreparation: async (route: string, payload: any) => route === "/gateway/connection-generation"
      ? { status: "ok", active: false, generation: "" }
      : { status: "ok", threadId: payload.thread_id, model: "saved-model", reasoningEffort: "high" },
    postJson: async (route: string, payload: any) => {
      assert.equal(route, "/thread-provider/prepare");
      preparations.push(payload);
      state.indexedProvider = payload.target_provider;
      return { status: "ok" };
    },
    prepareCodexThreadContextStart: async (_method: string, params: any) => params,
    noteCodexThreadRuntimeContext() {}, refreshCodexThreadModelBeforeTurn: async () => null,
    codexThreadModelRequestState: () => ({}), codexPlusModelUnlockEnabled: () => false,
    AbortController, setTimeout, clearTimeout,
  });
  vm.runInContext(serviceFragment + requestWrapper, context);
  assert.equal(vm.runInContext("patchAppServerModelRequestClient(client)", context), true);
  const plans = new Function("window", "codexPlusBackendSettings", "prepareCodexThreadProviderResume",
    "codexThreadProviderPreparations", "codexGatewayConnectionGeneration", "codexPlusAppServerClient",
    "codexManagedSessionConnection", "codexRemoteSessionActiveProfile",
    "codexPlusExtensionListeners", "codexPlusExtensionInterceptors", "codexPlusRunWaterfall",
    "codexPlusMarkExtensionFailure", "showToast", `${rpc}\nreturn { resolve: codexPlusRpcPlanResolve, resume: codexPlusRpcPlanResume };`)(
    {}, settings, context.prepareCodexThreadProviderResume, vm.runInContext("codexThreadProviderPreparations", context),
    context.codexGatewayConnectionGeneration, async () => client, context.codexManagedSessionConnection,
    context.codexRemoteSessionActiveProfile, new Map(), new Map(),
    (_name: string, value: any) => value, () => {}, () => {});
  return { calls, client, preparations, plan: plans.resolve, resumePlan: plans.resume, settings, state };
}

describe("aggregate transitions through the installed request wrapper", () => {
  for (const source of ["custom", "openai"]) {
    it(`does not prepare the ordinary index when entering aggregate from ${source}`, async () => {
      const env = runtime("aggregate", source, "notLoaded");
      const result = await env.client.sendRequest("thread/resume", { threadId: "thread" });

      assert.equal(result.modelProvider, "codex_plus_managed");
      assert.equal(env.preparations.length, 0);
    });
  }

  it("does not send on a loaded managed connection after switching to pure API", async () => {
    const env = runtime("pureApi", "codex_plus_managed", "idle");
    let failure: unknown;
    try {
      await env.client.sendRequest("turn/start", { threadId: "thread", input: [] });
    } catch (error) {
      failure = error;
    }

    assert.equal(env.preparations.length, 0);
    const turns = env.calls.filter((call) => call.method === "turn/start");
    if (turns.length) {
      assert.equal(turns.length, 1);
      assert.equal(turns[0].provider, "custom");
    } else {
      assert.match(String(failure), /供应商|连接|provider|connection/i);
    }
  });

  for (const enabled of [false, true]) {
    for (const status of ["idle", "notLoaded"] as const) {
      for (const mode of ["aggregate", "pureApi"] as const) {
        it(`reconnects ${status} through the real turn candidate for ${mode}, sync=${enabled}`, async () => {
          const source = mode === "aggregate" ? "openai" : "codex_plus_managed";
          const target = mode === "aggregate" ? "codex_plus_managed" : "custom";
          for (const field of ["resume", "steer", "absent"]) {
            const env = runtime(mode, source, status, enabled);
            const candidate: any = {
              executionHostId: "local", conversationId: "thread",
              resume: { conversationId: "thread" },
              start: { request: { threadId: "thread", input: [[]] } },
              steer: { input: [[]], restoreMessage: { submissionOptions: {} } },
            };
            if (field === "resume") candidate.resume.modelProvider = source;
            if (field === "steer") candidate.steer.restoreMessage.submissionOptions.modelProvider = source;
            const plan = env.plan(["resolve", 1, candidate]);
            assert.ok(plan?.before);
            await plan.before("thread", true, candidate, new AbortController().signal);
            assert.equal(env.state.connectedProvider, target, `${mode}/${enabled}/${status}/${field}`);
            assert.equal(env.preparations.length, 0);
            const resume = env.calls.find((call) => call.method === "thread/resume");
            assert.equal(resume?.params.model, "saved-model");
            assert.equal(resume?.params.config?.model_reasoning_effort, "high");
            if (mode === "aggregate") {
              assert.equal(resume?.params.config?.["model_providers.codex_plus_managed"]?.base_url,
                "http://127.0.0.1:62021/v1");
            }
          }
        });
      }
    }
  }

  it("keeps an active aggregate transition untouched", async () => {
    const env = runtime("aggregate", "openai", "active", true);
    const candidate = { executionHostId: "local", conversationId: "thread",
      resume: { conversationId: "thread" }, start: { request: { threadId: "thread", input: [[]] } } };
    await env.plan(["resolve", 1, candidate]).before("thread", true, candidate, new AbortController().signal);
    assert.equal(env.state.connectedProvider, "openai");
    assert.equal(env.calls.some((call) => call.method === "thread/unsubscribe" || call.method === "thread/resume"), false);
    assert.equal(env.preparations.length, 0);
  });

  it("restores the saved model when a pure API resume carries the old managed alias", async () => {
    const env = runtime("pureApi", "codex_plus_managed", "notLoaded", false);
    const result = await env.client.sendRequest("thread/resume",
      { threadId: "thread", model: null, modelProvider: "codex_plus_managed" });
    assert.equal(result.modelProvider, "custom");
    assert.equal(result.model, "saved-model");
    assert.equal(result.reasoningEffort, "high");
    assert.equal(env.preparations.length, 0);
  });

  for (const enabled of [false, true]) {
    for (const mode of ["aggregate", "pureApi"] as const) {
      it(`reconnects a loaded ${mode} resume through RPC with sync=${enabled}`, async () => {
        const source = mode === "aggregate" ? "openai" : "codex_plus_managed";
        const target = mode === "aggregate" ? "codex_plus_managed" : "custom";
        for (const includeProvider of [false, true]) {
          const env = runtime(mode, source, "idle", enabled);
          const params = { threadId: "thread", ...(includeProvider ? { modelProvider: source } : {}) };
          const frame = ["push", ["pipeline", -19, ["resumeThread"], [params]]];
          const plan = env.resumePlan(frame, "local", null);
          assert.ok(plan?.before);
          await plan.before(params, true, frame, new AbortController().signal);
          assert.equal(env.state.connectedProvider, target);
          assert.equal(env.preparations.length, 0);
        }
      });
    }
  }

  it("unloads an explicit old managed provider before a direct pure API resume", async () => {
    const env = runtime("pureApi", "codex_plus_managed", "idle", false);
    const result = await env.client.sendRequest("thread/resume",
      { threadId: "thread", model: "chosen", modelProvider: "codex_plus_managed" });
    assert.equal(result.modelProvider, "custom");
    assert.equal(result.model, "chosen");
    assert.equal(env.preparations.length, 0);
  });

  it("rejects an unavailable aggregate connection without changing the ordinary index", async () => {
    const env = runtime("aggregate", "openai", "idle", true);
    env.settings.managedSessionConnection.baseUrl = "";
    const candidate = { executionHostId: "local", conversationId: "thread",
      resume: { conversationId: "thread" }, start: { request: { threadId: "thread", input: [[]] } } };
    await assert.rejects(env.plan(["resolve", 1, candidate])
      .before("thread", true, candidate, new AbortController().signal), /聚合供应商连接不可用/);
    assert.equal(env.state.connectedProvider, "openai");
    assert.equal(env.preparations.length, 0);
  });
});
