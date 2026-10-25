import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import vm from "node:vm";

const tier = readFileSync(new URL("../../../assets/inject/renderer-inject/30-service-tier.js", import.meta.url), "utf8");
const catalog = readFileSync(new URL("../../../assets/inject/renderer-inject/70-model-catalog.js", import.meta.url), "utf8");
const guard = tier.slice(tier.indexOf("  function codexManagedNativeTitleFallback("),
  tier.indexOf("  function applyCodexManagedSessionConnection("));
const connectionGuard = tier.slice(tier.indexOf("  function codexManagedSessionConnection("),
  tier.indexOf("  function codexManagedNativeTitleFallback("));
const installer = catalog.slice(catalog.indexOf("  let codexNativeTitleModule ="),
  catalog.indexOf("  function ensureCodexModelWhitelistInstalls()"));

function runtime() {
  const state = { mode: "aggregate", connected: true, calls: [] as any[], imports: [] as string[] };
  const native = Object.freeze({
    generateTitle(...args: any[]) { state.calls.push({ receiver: this, args }); return Promise.resolve({ title: "generated" }); },
    generateDescription(...args: any[]) { return { receiver: this, args }; },
  });
  const root = { threadMetadataGeneration: native };
  const module: any = { unrelated: {}, root };
  const generation = {};
  const context = vm.createContext({
    window: { __codexPlusRemoteSessionRecoveryGeneration: generation },
    codexRemoteSessionRecoveryGeneration: generation,
    codexPlusBackendSettings: { managedSessionConnection: {
      profileId: "active", provider: "codex_plus_managed", baseUrl: "http://127.0.0.1:62021/v1",
    } },
    codexRemoteSessionActiveProfile: () => state.connected ? { id: "active", relayMode: state.mode } : null,
    loadOptionalCodexAppModule: async (prefix: string) => { state.imports.push(prefix); return module; },
  });
  vm.runInContext(`${connectionGuard}\n${guard}\n${installer}\nthis.patch = patchCodexNativeTitleService;
    this.install = installCodexNativeTitleServiceGuard;`, context);
  return { state, native, root, module, context };
}

describe("native title provider guard", () => {
  it("uses production profile gating without requiring an aggregate projection for direct suppliers", () => {
    const profile = { id: "direct", relayMode: "pureApi", officialMixApiKey: false };
    const settings: any = { relayProfilesEnabled: true, activeRelayId: "direct", relayProfiles: [profile],
      managedSessionConnection: null };
    const context = vm.createContext({ window: {}, codexPlusBackendSettings: settings });
    const activeProfile = tier.slice(tier.indexOf("  function codexRemoteSessionActiveProfile("),
      tier.indexOf("  function codexRemoteSessionProviderPatchEnabled("));
    vm.runInContext(`${activeProfile}\n${connectionGuard}\n${guard}\nthis.fallback = codexManagedNativeTitleFallback;`, context);
    const local = { hostId: "local" };
    for (const mode of ["pureApi", "mixedApi"]) {
      profile.relayMode = mode;
      assert.equal(context.fallback(local), true);
      assert.equal(context.fallback({ hostId: "ssh:test" }), false);
      settings.relayProfilesEnabled = false;
      assert.equal(context.fallback(local), false);
      settings.relayProfilesEnabled = true;
    }
    profile.relayMode = "official";
    assert.equal(context.fallback(local), false);
    profile.officialMixApiKey = true;
    assert.equal(context.fallback(local), true);
    profile.relayMode = "aggregate";
    assert.equal(context.fallback(local), false);
    settings.managedSessionConnection = { profileId: "direct", provider: "codex_plus_managed",
      baseUrl: "http://127.0.0.1:62021/v1" };
    assert.equal(context.fallback(local), true);
    settings.managedSessionConnection.profileId = "stale";
    assert.equal(context.fallback(local), false);
    settings.managedSessionConnection.profileId = "direct";
    settings.managedSessionConnection.baseUrl = "https://remote.invalid/v1";
    assert.equal(context.fallback(local), false);
    settings.activeRelayId = "missing";
    assert.equal(context.fallback(local), false);
  });

  it("returns null before native RPC only for protected local titles and follows hot switches", async () => {
    const { state, native, root, context } = runtime();
    assert.equal(context.patch(root), true);
    const service = root.threadMetadataGeneration;
    const params = { hostId: "local", prompt: "private user text" };
    for (const mode of ["aggregate", "pureApi", "mixedApi"]) {
      state.mode = mode;
      assert.equal(await service.generateTitle(params), null);
    }
    assert.equal(state.calls.length, 0);
    for (const mode of ["official", "unknown"]) {
      state.mode = mode;
      assert.deepEqual(await service.generateTitle(params), { title: "generated" });
    }
    for (const mode of ["aggregate", "pureApi", "mixedApi", "official"]) {
      state.mode = mode;
      state.connected = false;
      await service.generateTitle(params);
    }
    state.mode = "aggregate";
    state.connected = true;
    assert.equal(await service.generateTitle(params), null);
    for (const params of [{ hostId: "ssh-host" }, { hostId: "cloud" }, {}, undefined]) {
      await service.generateTitle(params);
    }
    assert.equal(state.calls.length, 10);
    assert.ok(state.calls.every((call) => call.receiver === native));
    assert.equal(state.calls[0].args[0], params);
  });

  it("does not nest wrappers or change other service methods, callbacks or frozen RPC stubs", () => {
    const { native, root, context } = runtime();
    const originalTitle = native.generateTitle;
    context.patch(root);
    const wrapper = root.threadMetadataGeneration;
    context.patch(root);
    assert.equal(root.threadMetadataGeneration, wrapper);
    assert.equal(native.generateTitle, originalTitle);
    const callback = () => "essential first turn callback";
    const result = wrapper.generateDescription({ onFirstTurnReady: callback });
    assert.equal(result.receiver, native);
    assert.equal(result.args[0].onFirstTurnReady, callback);
    assert.equal(wrapper.generateDescription, wrapper.generateDescription);
  });

  it("refuses read-only roots, accessors and unrelated objects without running getters", () => {
    const { native, context } = runtime();
    assert.equal(context.patch(Object.freeze({ threadMetadataGeneration: native })), false);
    assert.equal(context.patch({ get threadMetadataGeneration() { throw new Error("must not read"); } }), false);
    assert.equal(context.patch({ generateTitle() {} }), false);
    assert.equal(context.patch({ threadMetadataGeneration: {} }), false);
    assert.equal(context.window.__codexPlusNativeTitleServiceGuardInstalled, undefined);
  });

  it("rebinds reinjected settings without retaining a stale guard or nesting wrappers", async () => {
    const { context, root, state } = runtime();
    context.patch(root);
    const wrapper = root.threadMetadataGeneration;
    const generation = {};
    context.window.__codexPlusRemoteSessionRecoveryGeneration = generation;
    const fresh = vm.createContext({
      window: context.window,
      codexRemoteSessionRecoveryGeneration: generation,
      codexManagedSessionConnection: () => null,
      codexRemoteSessionActiveProfile: () => ({ relayMode: "official" }),
    });
    vm.runInContext(`${guard}\nthis.patch = patchCodexNativeTitleService;`, fresh);
    assert.equal(fresh.patch(root), true);
    const marker = context.window.__codexPlusNativeTitleServiceGuardInstalled;
    assert.equal(marker.version, 1);
    assert.equal(marker.generation, generation);
    assert.equal(root.threadMetadataGeneration, wrapper);
    await wrapper.generateTitle({ hostId: "local" });
    assert.equal(state.calls.length, 1);
    assert.equal(context.patch(root), false);
    assert.equal(context.window.__codexPlusNativeTitleServiceGuardInstalled, marker);
    assert.deepEqual(await wrapper.generateTitle({ hostId: "local" }), { title: "generated" });
    assert.equal(root.threadMetadataGeneration, wrapper);
    assert.equal(state.calls.length, 2);
  });

  it("ignores stale installer completions and cached installer calls after reinjection", async () => {
    for (const cached of [false, true]) {
      const { context, root, state } = runtime();
      let finish!: (module: any) => void;
      context.loadOptionalCodexAppModule = () => new Promise((resolve) => { finish = resolve; });
      context.install();
      const pending = vm.runInContext("codexNativeTitleModulePromise", context);
      if (cached) {
        finish({ root });
        await pending;
      }
      const generation = {};
      context.window.__codexPlusRemoteSessionRecoveryGeneration = generation;
      const fresh = vm.createContext({
        window: context.window,
        codexRemoteSessionRecoveryGeneration: generation,
        codexManagedSessionConnection: () => null,
        codexRemoteSessionActiveProfile: () => ({ relayMode: "official" }),
      });
      vm.runInContext(`${guard}\nthis.patch = patchCodexNativeTitleService;`, fresh);
      assert.equal(fresh.patch(root), true);
      const wrapper = root.threadMetadataGeneration;
      if (!cached) {
        finish({ root });
        await pending;
        assert.equal(vm.runInContext("codexNativeTitleModule", context), null);
      }
      context.install();
      assert.equal(context.patch(root), false);
      assert.deepEqual(await wrapper.generateTitle({ hostId: "local" }), { title: "generated" });
      assert.equal(root.threadMetadataGeneration, wrapper);
      assert.equal(state.calls.length, 1);
    }
  });

  it("uses only the optional app-shared module, retries late live exports and patches replacement roots", async () => {
    const { context, module, root, native, state } = runtime();
    module.root = undefined;
    Object.defineProperty(module, "unknownAccessor", { get() { throw new Error("must not read"); } });
    context.install();
    context.install();
    await vm.runInContext("codexNativeTitleModulePromise", context);
    assert.deepEqual(state.imports, ["app-shared-"]);
    assert.equal(root.threadMetadataGeneration, native);
    module.root = root;
    context.install();
    assert.equal(await root.threadMetadataGeneration.generateTitle({ hostId: "local" }), null);
    const replacement = { threadMetadataGeneration: native };
    module.root = replacement;
    context.install();
    assert.equal(await replacement.threadMetadataGeneration.generateTitle({ hostId: "local" }), null);
    assert.equal(state.calls.length, 0);
    assert.equal(state.imports.length, 1);
  });
});
