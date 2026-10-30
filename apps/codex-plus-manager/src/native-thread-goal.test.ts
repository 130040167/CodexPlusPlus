import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { describe, it } from "node:test";
import vm from "node:vm";

const read = (path: string) => readFileSync(new URL(`../../../${path}`, import.meta.url), "utf8");
const runtimeSource = read("assets/inject/native-thread-goal.js");
const rendererSource = read("assets/inject/renderer-inject/32-thread-goal.js");
const extensionSource = read("assets/inject/renderer-inject/91-extension-api.js");
const routesSource = read("crates/codex-plus-core/src/routes.rs");
const ownerError = "Please continue this conversation on the window where it was started.";
const threadId = "fixture-thread";
const hostId = "local";
const flush = () => new Promise<void>((resolve) => setImmediate(resolve));

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

function fixture() {
  const goal = { objective: "native objective", status: "active", updatedAt: 123 };
  const remoteGoal = { objective: "owner objective", status: "active", updatedAt: 456 };
  const state = {
    role: "owner" as string | null,
    hostId,
    present: true,
    enabled: true,
    manager: null as any,
    nativeCalls: [] as any[],
    forwards: [] as any[],
    reports: [] as string[],
    events: [] as string[],
    loads: 0,
    roleReads: 0,
    uuidCount: 0,
    onRoleRead: null as null | (() => void),
    onResume: null as null | (() => void | Promise<void>),
    settingsPending: null as null | Promise<void>,
    serverPending: null as null | Promise<void>,
    implementation: null as null | ((this: any, id: string, params: any) => any),
    transcript: Object.freeze([{ id: "existing-native-item" }]),
  };
  class NativeManager {
    disposed = false;
    getHostId() { return state.hostId; }
    getConversation(id: string) { return state.present && id === threadId ? { id } : null; }
    getStreamRole() {
      state.roleReads += 1;
      state.onRoleRead?.();
      return state.role ? { role: state.role, ownerClientId: "fixture-owner" } : null;
    }
    async setThreadGoal(id: string, params: any) {
      state.nativeCalls.push({ receiver: this, id, params });
      state.events.push("native");
      if (state.implementation) return state.implementation.call(this, id, params);
      state.events.push("resume");
      await state.onResume?.();
      if (state.role !== "owner") throw new Error(ownerError);
      state.events.push("assert-owner");
      state.events.push(params.threadSettings == null ? "wait-settings" : "update-settings");
      await state.settingsPending;
      state.events.push("goal-request");
      await state.serverPending;
      state.events.push(params.appendTranscriptItem === false ? "native-goal-state" : "native-transcript");
      return goal;
    }
    setConversationStreamRole() { throw new Error("The adapter must not change stream ownership"); }
    updateConversationState() { throw new Error("The adapter must not write transcript state"); }
    sendRequest() { throw new Error("The adapter must not issue raw app-server requests"); }
  }
  const manager = new NativeManager();
  state.manager = manager;
  const scopeIdentity = {};
  const managerSignal = { kind: "signal-family", scope: scopeIdentity };
  const managersSignal = { kind: "signal", scope: scopeIdentity };
  const scope = {
    scope: scopeIdentity,
    get(signal: any) {
      if (signal === managerSignal) return state.manager;
      if (signal === managersSignal) return state.manager ? [state.manager] : [];
      return null;
    },
  };
  // Preserve the native getter's structural contract while using arbitrary export names.
  function nativeGetter(e: any, t: string) {
    if (t == null) return null;
    const manager = e.get(managerSignal);
    return manager?.getConversation(t) ? manager
      : e.get(managersSignal).find((candidate: any) => candidate.getConversation(t) != null) ?? null;
  }
  const module: any = { renamedSignal: managerSignal, renamedGetter: nativeGetter, unrelated: {} };
  const fiber: any = { memoizedState: { memoizedState: { current: scope }, next: null } };
  const root: any = { __reactContainer$fixture: { stateNode: { current: fiber } } };
  const body: any = {};
  const documentElement: any = {};
  const context = vm.createContext({
    window: { location: new URL("app://-/index.html?initialRoute=%2Fspotlight") },
    document: {
      getElementById: (id: string) => id === "root" ? root : null,
      body,
      documentElement,
      querySelectorAll: () => [],
    },
    performance: { getEntriesByType: () => [] },
    URL,
    crypto: { randomUUID: () => `fixture-request-${++state.uuidCount}` },
  });
  vm.runInContext(runtimeSource, context);
  const api = context.window.__codexPlusNativeThreadGoal;
  const options = {
    hostId,
    loadModule: async () => { state.loads += 1; return module; },
    forward: async (request: any) => { state.forwards.push(request); return { status: "ok", goal: remoteGoal }; },
    enabled: () => state.enabled,
    report: (code: string) => state.reports.push(code),
  };
  const install = (overrides: any = {}) => api.install(threadId, { ...options, ...overrides });
  const request = (requestId = "request-1", params: any = { objective: "preserve objective" }) =>
    ({ threadId, hostId, requestId, params });
  return { api, context, state, manager, module, scope, fiber, root, body, documentElement,
    goal, remoteGoal, options, install, request };
}

describe("native thread goal forwarding", () => {
  it("keeps owner calls on the original method with the same receiver and parameters", async () => {
    const f = fixture();
    const params = Object.freeze({
      objective: " keep spaces\nand lines ",
      appendTranscriptItem: false,
      threadSettings: Object.freeze({ model: "fixture-model", effort: "high", serviceTier: "fast",
        sandboxPolicy: Object.freeze({ type: "workspaceWrite", writableRoots: ["C:/fixture"] }) }),
    });
    assert.equal(await f.install(), true);
    assert.equal(await f.manager.setThreadGoal(threadId, params), f.goal);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.nativeCalls[0].receiver, f.manager);
    assert.equal(f.state.nativeCalls[0].id, threadId);
    assert.equal(f.state.nativeCalls[0].params, params);
    assert.equal(f.state.forwards.length, 0);
    assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "update-settings",
      "goal-request", "native-goal-state"]);
  });

  it("forwards followers once without issuing native writes or changing local state", async () => {
    const f = fixture();
    f.state.role = "follower";
    const transcript = f.state.transcript;
    const params = Object.freeze({ objective: "forward this", appendTranscriptItem: true,
      threadSettings: Object.freeze({ model: "selected-model", approvalsReviewer: "guardian" }) });
    await f.install();
    assert.equal(await f.manager.setThreadGoal(threadId, params), f.remoteGoal);
    assert.equal(f.state.nativeCalls.length, 0);
    assert.equal(f.state.forwards.length, 1);
    assert.equal(f.state.forwards[0].params, params);
    assert.equal(f.state.forwards[0].threadId, threadId);
    assert.equal(f.state.forwards[0].hostId, hostId);
    assert.equal(f.state.forwards[0].requestId, "fixture-request-1");
    assert.equal(f.state.role, "follower");
    assert.equal(f.state.transcript, transcript);
    assert.deepEqual(f.state.events, []);
    assert.deepEqual(f.state.reports, []);
  });

  it("only compensates the known ownership assertion when resume changes the role", async () => {
    const f = fixture();
    f.state.onResume = () => { f.state.role = "follower"; };
    await f.install();
    assert.equal(await f.manager.setThreadGoal(threadId, { objective: "race" }), f.remoteGoal);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.forwards.length, 1);
    assert.deepEqual(f.state.events, ["native", "resume"]);
  });

  it("does not forward other errors or replay a write whose outcome is unknown", async () => {
    for (const [message, finalRole] of [[ownerError, "owner"], ["Timeout after write", "follower"],
      [`${ownerError} retry`, "follower"]]) {
      const f = fixture();
      const error = new Error(message);
      f.state.implementation = async () => {
        f.state.role = finalRole;
        throw error;
      };
      await f.install();
      await assert.rejects(f.manager.setThreadGoal(threadId, { objective: "failed" }), value => value === error);
      assert.equal(f.state.nativeCalls.length, 1, message);
      assert.equal(f.state.forwards.length, 0, message);
    }
  });

  it("preserves native behavior when enhancements are disabled and follows later toggles", async () => {
    const f = fixture();
    f.state.role = "follower";
    f.state.enabled = false;
    await f.install();
    await assert.rejects(f.manager.setThreadGoal(threadId, { objective: "disabled" }), { message: ownerError });
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.forwards.length, 0);
    f.state.enabled = true;
    assert.equal(await f.manager.setThreadGoal(threadId, { objective: "enabled" }), f.remoteGoal);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.forwards.length, 1);
  });

  it("reports structured owner failures without retrying or calling the native method", async () => {
    for (const response of [{ status: "failed", code: "owner-unavailable", message: "owner not ready" },
      { status: "ok" }, null]) {
      const f = fixture();
      f.state.role = "follower";
      let calls = 0;
      await f.install({ forward: async () => { calls += 1; return response; } });
      await assert.rejects(f.manager.setThreadGoal(threadId, { objective: "failed forwarding" }));
      assert.equal(calls, 1);
      assert.equal(f.state.nativeCalls.length, 0);
      assert.equal(f.state.reports.length, 1);
      assert.equal(f.state.reports[0], response?.code || "goal-forward-failed");
    }
  });

  it("propagates rejected forwarding without retrying an unknown result", async () => {
    const f = fixture();
    const error = new Error("CDP connection closed after dispatch");
    let calls = 0;
    f.state.role = "follower";
    await f.install({ forward: async () => { calls += 1; throw error; } });
    await assert.rejects(f.manager.setThreadGoal(threadId, { objective: "unknown" }), value => value === error);
    assert.equal(calls, 1);
    assert.equal(f.state.nativeCalls.length, 0);
  });

  it("reuses the installed runtime and prototype wrapper while refreshing adapters", async () => {
    const f = fixture();
    f.state.role = "follower";
    await f.install();
    const wrapper = f.manager.setThreadGoal;
    vm.runInContext(runtimeSource, f.context);
    assert.equal(f.context.window.__codexPlusNativeThreadGoal, f.api);
    let forwards = 0;
    await f.install({ forward: async () => { forwards += 1; return { status: "ok", goal: f.goal }; } });
    assert.equal(f.manager.setThreadGoal, wrapper);
    assert.equal(await f.manager.setThreadGoal(threadId, { objective: "reinjected" }), f.goal);
    assert.equal(forwards, 1);
    assert.equal(f.state.forwards.length, 0);
    assert.equal(f.state.nativeCalls.length, 0);
    assert.equal(f.state.loads, 1);
  });
});

describe("native thread goal discovery and execution", () => {
  it("skips exports whose instrumented function serialization throws", async () => {
    const f = fixture();
    const blocked = () => {};
    f.module.uninspectableExport = blocked;
    f.context.uninspectableExport = blocked;
    vm.runInContext(`{
      const original = Function.prototype.toString;
      Function.prototype.toString = function () {
        if (this === uninspectableExport) throw new TypeError("Instrumented function is unavailable");
        return original.call(this);
      };
    }`, f.context);
    assert.equal(await f.install(), true);
    assert.equal((await f.api.probe({ threadId, hostId })).status, "owner");
    assert.equal((await f.api.execute(f.request())).goal, f.goal);
    assert.equal(f.state.nativeCalls.length, 1);
  });

  it("probes roles without writing native state, forwarding or acquiring ownership", async () => {
    const f = fixture();
    await f.install();
    const transcript = f.state.transcript;
    for (const role of ["owner", "follower", null]) {
      f.state.role = role;
      assert.equal((await f.api.probe({ threadId, hostId })).status, role || "unavailable");
      assert.equal(f.state.role, role);
    }
    assert.equal(f.state.transcript, transcript);
    assert.equal(f.state.nativeCalls.length, 0);
    assert.equal(f.state.forwards.length, 0);
    assert.deepEqual(f.state.events, []);
  });

  it("refuses missing managers, threads, mismatched hosts and disposed instances", async () => {
    for (const change of [
      (f: any) => { f.state.manager = null; },
      (f: any) => { f.state.present = false; },
      (f: any) => { f.state.hostId = "ssh:other-host"; },
      (f: any) => { f.manager.disposed = true; },
      (f: any) => { f.scope.scope = {}; },
    ]) {
      const f = fixture();
      const original = f.manager.setThreadGoal;
      change(f);
      assert.equal(await f.install(), false);
      assert.equal(f.manager.setThreadGoal, original);
      assert.equal((await f.api.probe({ threadId, hostId })).status, "unavailable");
      await assert.rejects(f.api.execute(f.request()), { message: "goal-owner-changed" });
      assert.equal(f.state.nativeCalls.length, 0);
      assert.equal(f.state.forwards.length, 0);
    }
  });

  it("finds the AppScope through child and sibling fibers and later hooks", async () => {
    const f = fixture();
    const scopeHook = f.fiber.memoizedState;
    f.fiber.memoizedState = null;
    f.fiber.child = { memoizedState: null, sibling: {
      memoizedState: { memoizedState: { current: {} }, next: scopeHook },
    } };
    f.fiber.child.child = f.fiber;
    assert.equal(await f.install(), true);
    assert.equal((await f.api.probe({ threadId, hostId })).status, "owner");
    assert.equal(f.state.nativeCalls.length, 0);
  });

  it("does not patch accessor or read-only native methods", async () => {
    for (const accessor of [false, true]) {
      const f = fixture();
      const proto = Object.getPrototypeOf(f.manager);
      const original = f.manager.setThreadGoal;
      Object.defineProperty(proto, "setThreadGoal", accessor
        ? { get: () => original, configurable: true }
        : { value: original, writable: false, configurable: true });
      assert.equal(await f.install(), false);
      assert.equal(f.manager.setThreadGoal, original);
      assert.equal(f.state.forwards.length, 0);
    }
  });

  it("runs owner settings and transcript behavior once with all original parameters", async () => {
    for (const appendTranscriptItem of [false, true, undefined]) {
      const f = fixture();
      const settings = deferred<void>();
      const params = Object.freeze({ objective: " keep original goal\n",
        ...(appendTranscriptItem == null ? {} : { appendTranscriptItem }),
        threadSettings: Object.freeze({ model: "selected-model", effort: "high",
          serviceTier: "fast", approvalsReviewer: "guardian",
          sandboxPolicy: Object.freeze({ type: "workspaceWrite", writableRoots: ["C:/fixture"] }) }) });
      f.state.settingsPending = settings.promise;
      await f.install();
      const pending = f.api.execute(f.request("owner-request", params));
      await flush();
      assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "update-settings"]);
      assert.equal(f.state.nativeCalls.length, 1);
      assert.equal(f.state.nativeCalls[0].params, params);
      assert.equal(f.state.nativeCalls[0].receiver, f.manager);
      settings.resolve();
      const result = await pending;
      assert.equal(result.status, "ok");
      assert.equal(result.goal, f.goal);
      assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "update-settings",
        "goal-request", appendTranscriptItem === false ? "native-goal-state" : "native-transcript"]);
      assert.equal(f.state.forwards.length, 0);
    }
  });

  it("waits for the native pending settings path when no threadSettings are supplied", async () => {
    const f = fixture();
    const settings = deferred<void>();
    f.state.settingsPending = settings.promise;
    await f.install();
    const pending = f.api.execute(f.request());
    await flush();
    assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "wait-settings"]);
    settings.resolve();
    assert.equal((await pending).status, "ok");
    assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "wait-settings",
      "goal-request", "native-transcript"]);
  });

  it("propagates a settings failure before any native goal request and never retries it", async () => {
    const f = fixture();
    const settings = deferred<void>();
    const error = new Error("settings update rejected");
    f.state.settingsPending = settings.promise;
    await f.install();
    const request = f.request("settings-failure", { objective: "goal", threadSettings: { model: "selected" } });
    const pending = f.api.execute(request);
    const rejected = assert.rejects(pending, value => value === error);
    await flush();
    settings.reject(error);
    await rejected;
    await assert.rejects(f.api.execute(request), value => value === error);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.deepEqual(f.state.events, ["native", "resume", "assert-owner", "update-settings"]);
    assert.equal(f.state.forwards.length, 0);
  });

  it("rechecks role, host, thread and disposed state immediately before execution", async () => {
    for (const change of [
      (f: any) => { f.state.role = "follower"; },
      (f: any) => { f.state.hostId = "ssh:changed"; },
      (f: any) => { f.state.present = false; },
      (f: any) => { f.manager.disposed = true; },
    ]) {
      const f = fixture();
      await f.install();
      f.state.onRoleRead = () => {
        if (f.state.roleReads === 1) change(f);
      };
      await assert.rejects(f.api.execute(f.request()), { message: "goal-owner-changed" });
      assert.equal(f.state.nativeCalls.length, 0);
      assert.equal(f.state.forwards.length, 0);
      assert.deepEqual(f.state.events, []);
    }
  });

  it("uses the original owner method so resume races cannot recursively forward", async () => {
    const f = fixture();
    f.state.onResume = () => { f.state.role = "follower"; };
    await f.install();
    await assert.rejects(f.api.execute(f.request()), { message: ownerError });
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.forwards.length, 0);
    assert.deepEqual(f.state.events, ["native", "resume"]);
  });

  it("deduplicates concurrent and completed requests and rejects conflicting ID reuse", async () => {
    const f = fixture();
    const server = deferred<void>();
    f.state.serverPending = server.promise;
    await f.install();
    const request = f.request("same-id");
    const first = f.api.execute(request);
    const concurrent = f.api.execute(request);
    await flush();
    assert.equal(f.state.nativeCalls.length, 1);
    await assert.rejects(f.api.execute(f.request("same-id", { objective: "different objective" })),
      { message: "goal-request-id-conflict" });
    server.resolve();
    const [firstResult, concurrentResult] = await Promise.all([first, concurrent]);
    assert.equal(firstResult.goal, f.goal);
    assert.equal(concurrentResult.goal, f.goal);
    assert.equal((await f.api.execute(request)).goal, f.goal);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.events.filter(event => event === "native-transcript").length, 1);
    assert.equal(f.state.forwards.length, 0);
  });

  it("retains a failed request result instead of retrying a write with an unknown outcome", async () => {
    const f = fixture();
    const error = new Error("connection closed after native request");
    f.state.implementation = async () => { f.state.events.push("goal-request"); throw error; };
    await f.install();
    const request = f.request("unknown-outcome");
    await assert.rejects(f.api.execute(request), value => value === error);
    await assert.rejects(f.api.execute(request), value => value === error);
    assert.equal(f.state.nativeCalls.length, 1);
    assert.equal(f.state.events.filter(event => event === "goal-request").length, 1);
    assert.equal(f.state.forwards.length, 0);
  });

  it("caps pending requests without evicting an in-flight request or duplicating its write", async () => {
    const f = fixture();
    const server = deferred<void>();
    f.state.serverPending = server.promise;
    await f.install();
    const pending = Array.from({ length: 128 }, (_, index) => f.api.execute(f.request(`pending-${index}`)));
    await flush();
    assert.equal(f.state.nativeCalls.length, 128);
    await assert.rejects(f.api.execute(f.request("overflow")), { message: "goal-request-limit" });
    const duplicate = f.api.execute(f.request("pending-0"));
    assert.equal(f.state.nativeCalls.length, 128);
    server.resolve();
    const results = await Promise.all([...pending, duplicate]);
    assert.ok(results.every(result => result.status === "ok" && result.goal === f.goal));
    assert.equal(f.state.nativeCalls.length, 128);
  });
});

describe("native thread goal renderer integration", () => {
  function rendererFixture() {
    const f = fixture();
    const state = {
      now: 10_000,
      ref: { session_id: threadId, host_id: hostId },
      imports: [] as string[],
      posts: [] as any[],
      diagnostics: [] as any[],
    };
    f.context.Date = { now: () => state.now };
    f.context.currentSessionRef = () => state.ref;
    f.context.codexPlusBackendSettings = { enhancementsEnabled: true };
    f.context.loadOptionalCodexAppModule = async (prefix: string) => {
      state.imports.push(prefix);
      return f.module;
    };
    f.context.postJson = async (path: string, request: any) => {
      state.posts.push({ path, request });
      return { status: "ok", goal: f.remoteGoal };
    };
    f.context.sendCodexPlusDiagnostic = (event: string, details: any) =>
      state.diagnostics.push({ event, details });
    vm.runInContext(`${rendererSource}\nthis.installFromRenderer = installCodexThreadGoalForwarding;`, f.context);
    return { ...f, rendererState: state };
  }

  it("loads the native module and binds only the internal owner route with live settings", async () => {
    const f = rendererFixture();
    f.state.role = "follower";
    f.context.installFromRenderer();
    await flush();
    const params = { objective: "renderer goal", threadSettings: { model: "selected" } };
    assert.equal(await f.manager.setThreadGoal(threadId, params), f.remoteGoal);
    assert.deepEqual(f.rendererState.imports, ["app-shared-"]);
    assert.equal(f.rendererState.posts.length, 1);
    assert.equal(f.rendererState.posts[0].path, "/thread-goal/set-in-owner");
    assert.equal(f.rendererState.posts[0].request.params, params);
    f.context.codexPlusBackendSettings.enhancementsEnabled = false;
    await assert.rejects(f.manager.setThreadGoal(threadId, params), { message: ownerError });
    assert.equal(f.rendererState.posts.length, 1);
    assert.equal(f.state.nativeCalls.length, 1);
  });

  it("does not install without a current thread and debounces overlapping installs", async () => {
    const f = rendererFixture();
    const pending = deferred<boolean>();
    let calls = 0;
    f.api.install = () => { calls += 1; return pending.promise; };
    f.rendererState.ref = { session_id: "", host_id: hostId };
    f.context.installFromRenderer();
    assert.equal(calls, 0);
    f.rendererState.ref = { session_id: threadId, host_id: hostId };
    f.context.installFromRenderer();
    f.context.installFromRenderer();
    f.rendererState.now += 3000;
    f.context.installFromRenderer();
    assert.equal(calls, 1);
    pending.resolve(true);
    await flush();
    f.context.installFromRenderer();
    assert.equal(calls, 2);
    await flush();
    f.context.installFromRenderer();
    assert.equal(calls, 2);
  });

  it("keeps the owner mutation route outside the extension API whitelist", () => {
    const whitelist = extensionSource.match(/const codexPlusExtensionRoutes = new Set\(\[[\s\S]*?\]\);/);
    assert.ok(whitelist, "The real extension route whitelist must remain inspectable");
    const routes = vm.runInNewContext(`${whitelist[0]}\nArray.from(codexPlusExtensionRoutes)`) as string[];
    assert.equal(routes.includes("/thread-goal/set-in-owner"), false);
    assert.match(rendererSource, /postJson\("\/thread-goal\/set-in-owner", request\)/);
    assert.match(routesSource, /"\/thread-goal\/set-in-owner"\s*=>/);
    assert.doesNotMatch(runtimeSource, /assertThreadFollowerOwner\s*=|setConversationStreamRole\s*\(/);
  });
});
