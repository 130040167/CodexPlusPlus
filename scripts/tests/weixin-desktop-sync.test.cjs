"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const { test } = require("node:test");
const path = require("node:path");
const source = fs.readFileSync(path.join(__dirname, "../../assets/inject/renderer-inject/85-weixin-desktop-sync.js"), "utf8");
const id = "00000000-0000-4000-8000-000000000001";
const revision = "00000000-0000-4000-8000-000000000002";

function setup(notifyThread, extra = {}) {
  const catalog = { notifyThread, readEntries() {} };
  const state = extra.state || { projectsByHome: {}, assignments: {} };
  const keys = {
    APP_SERVER_PROJECT_ID_BY_LEGACY_PROJECT_ID_BY_HOST: "projectsByHome",
    THREAD_PROJECT_ASSIGNMENTS: "assignments",
  };
  const services = {
    localThreadCatalog: catalog,
    projects: { getLocalProjects: async () => ({}), upsertLocal: async () => true },
    executionPaths: { readCodexHome: async () => "C:/codex-home" },
    threadProjectAssignments: { setProjectless: async () => {}, assignIfUnassigned: async () => {} },
    ...extra,
  };
  class NativeClient {
    static getInstance() { return new NativeClient(); }
    async post(route, body) {
      assert.equal(route, "vscode://codex/get-global-state");
      return { body: { value: state[JSON.parse(body).key] ?? null } };
    }
    sendRequest() {}
    onFetchResponse() {}
    cancelStream() {}
  }
  const context = vm.createContext({
    window: {}, codexPlusBackendSettingsLoaded: true,
    codexPlusBackendSettings: { weixinDesktopSync: [{ threadId: id, revision }] },
    loadOptionalCodexAppModule: async () => ({
      arbitraryExport: services, arbitraryConstants: keys, arbitraryClient: NativeClient,
      misleadingStateUpdater: () => { throw Error("get-global-state set-global-state params"); },
      permissiveRpc: new Proxy(() => {}, {
        get: (_, key) => key === "prototype" ? NativeClient.prototype : () => {},
      }),
    }),
    getCodexGlobalState: () => assert.fail("Do not use the ambiguous state-function matcher"),
  });
  vm.runInContext(source, context);
  return { context, state, run: () => vm.runInContext("syncWeixinDesktopThreads()", context) };
}

test("notifies only local IDs, deduplicates revisions and survives reinjection", async () => {
  const calls = [];
  const { context, run } = setup(async (...args) => calls.push(args));
  await run();
  await run();
  vm.runInContext(`(() => { ${source} })()`, context);
  await run();
  assert.equal(calls.length, 1);
  assert.equal(calls[0][0].hostId, "local");
  assert.equal(calls[0][0].threadId, id);
  assert.equal(calls[0][1], "update");
  context.codexPlusBackendSettings.weixinDesktopSync[0].revision = id;
  await run();
  assert.equal(calls.length, 2);
});

test("failures are retried without acknowledging or parallel notification", async () => {
  let calls = 0, release;
  const { context, run } = setup(() => {
    calls += 1;
    return new Promise((_, reject) => { release = () => reject(Error("not active")); });
  });
  const first = run();
  await new Promise((resolve) => setImmediate(resolve));
  await run();
  assert.equal(calls, 1);
  release();
  await first;
  await run();
  assert.equal(calls, 1);
  assert.equal(context.window.__codexPlusWeixinDesktopSync.revisions.size, 0);
  assert(context.window.__codexPlusWeixinDesktopSync.retryAt > Date.now());
});

test("rejects malformed or unbounded snapshots", async () => {
  let calls = 0;
  const { context, run } = setup(async () => { calls += 1; });
  context.codexPlusBackendSettings.weixinDesktopSync = [{ threadId: "remote:any", revision }];
  await run();
  context.codexPlusBackendSettings.weixinDesktopSync = Array.from({ length: 129 }, () => ({ threadId: id, revision }));
  await run();
  assert.equal(calls, 0);
});

const reserved = "codex-plus-weixin-reserved-v1";
const project = {
  id: revision, name: "Renamed Weixin", rootPaths: ["C:\\work\\weixin"], registrationKey: reserved,
};

test("never registers an unrecognized project", async () => {
  const { context, run } = setup(async () => assert.fail("Unknown identity must remain pending"));
  context.codexPlusBackendSettings.weixinDesktopSync[0].project = project;
  await run();
  assert.equal(context.window.__codexPlusWeixinDesktopSync.revisions.size, 0);
});
test("uses the existing desktop project ID instead of duplicating the server project", async () => {
  for (const registrationKey of [undefined, reserved]) {
    let assigned;
    const { context, run } = setup(async () => {}, {
      state: { projectsByHome: { "local:C:\\codex-home": { desktop: project.id } }, assignments: {} },
      projects: {
        getLocalProjects: async () => ({ desktop: { id: "desktop", rootPaths: ["c:/WORK/weixin/"] } }),
        upsertLocal: async () => assert.fail("An existing selected project must not be registered again"),
      },
      threadProjectAssignments: {
        setProjectless: async () => {},
        assignIfUnassigned: async ([request]) => { assigned = request; },
      },
    });
    context.codexPlusBackendSettings.weixinDesktopSync[0].project = { ...project, registrationKey };
    await run();
    assert.equal(assigned.assignment.projectId, "desktop");
    assert.equal(context.window.__codexPlusWeixinDesktopSync.revisions.size, 1);
  }
});

test("declined registration and ambiguous existing roots leave the revision pending", async () => {
  for (const ambiguous of [false, true]) {
    const { context, run } = setup(async () => assert.fail("Must not acknowledge incomplete project sync"), {
      projects: {
        getLocalProjects: async () => ambiguous ? {
          first: { id: "first", rootPaths: project.rootPaths },
          second: { id: "second", rootPaths: project.rootPaths },
        } : {},
        upsertLocal: async () => false,
      },
      threadProjectAssignments: {
        setProjectless: async () => assert.fail("Must not guess membership"),
        assignIfUnassigned: async () => assert.fail("Must not guess membership"),
      },
    });
    context.codexPlusBackendSettings.weixinDesktopSync[0].project =
      { ...project, registrationKey: ambiguous ? undefined : reserved };
    await run();
    assert.equal(context.window.__codexPlusWeixinDesktopSync.revisions.size, 0);
    assert(context.window.__codexPlusWeixinDesktopSync.retryAt > Date.now());
  }
});

test("never infers identity from shared roots, other homes or overwrites a saved assignment", async () => {
  for (const mode of ["shared-root", "other-home", "already-assigned"]) {
    let notified = false;
    const mappings = mode === "shared-root" ? { desktop: id } : { desktop: project.id };
    const { context, run } = setup(async () => { notified = true; }, {
      state: {
        projectsByHome: { [mode === "other-home" ? "local:C:\\other-home" : "local:C:\\codex-home"]: mappings },
        assignments: mode === "already-assigned" ? { [id]: { projectKind: "remote", projectId: "user-choice" } } : {},
      },
      projects: {
        getLocalProjects: async () => ({ desktop: { id: "desktop", rootPaths: project.rootPaths } }),
        upsertLocal: async () => assert.fail("Must not duplicate selected projects"),
      },
      threadProjectAssignments: {
        setProjectless: async () => assert.fail("Must preserve unknown or existing assignment"),
        assignIfUnassigned: async () => assert.fail("Must preserve unknown or existing assignment"),
      },
    });
    context.codexPlusBackendSettings.weixinDesktopSync[0].project = { ...project, registrationKey: undefined };
    await run();
    assert.equal(notified, mode === "already-assigned");
  }
});

test("an unreadable native state response cannot be treated as an unassigned thread", async () => {
  const { context, run } = setup(async () => assert.fail("Must leave unreadable state pending"));
  const module = await context.loadOptionalCodexAppModule();
  class UnreadableClient {
    static getInstance() { return new UnreadableClient(); }
    async post() { return { body: {} }; }
    sendRequest() {}
    onFetchResponse() {}
    cancelStream() {}
  }
  context.loadOptionalCodexAppModule = async () => ({ ...module, arbitraryClient: UnreadableClient });
  context.codexPlusBackendSettings.weixinDesktopSync[0].project = project;
  await run();
  assert.equal(context.window.__codexPlusWeixinDesktopSync.revisions.size, 0);
  assert(context.window.__codexPlusWeixinDesktopSync.retryAt > Date.now());
});
