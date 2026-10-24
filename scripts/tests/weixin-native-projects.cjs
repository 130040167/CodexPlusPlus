"use strict";

// 默认仅元数据验证；显式 --mock-first-message 只允许本地固定 SSE 模拟。
const fs = require("node:fs");
const path = require("node:path");
const http = require("node:http");
const { spawn } = require("node:child_process");
const { once } = require("node:events");
const readline = require("node:readline");

const ROOT = path.resolve(__dirname, "../..");
const CLEANUP_ONLY = process.argv[2] === "--cleanup-followup";
const MOCK = process.argv.includes("--mock-first-message");
const CLI = CLEANUP_ONLY ? null : process.argv[2];
const ASAR = process.argv[3]?.startsWith("--") ? null : process.argv[3];
const RUN = path.join(ROOT, "output/weixin-session-controls", `native-run-${new Date().toISOString().replace(/[:.]/g, "-")}-${process.pid}`);
const TOTAL_MS = 240000;
const RPC_MS = 12000;
const ALLOWED = new Set([
  "initialize", "project/list", "project/create", "project/read", "project/update",
  "thread/start", "thread/read", "thread/list", "thread/loaded/list",
  "thread/metadata/update", "thread/name/set", "thread/inject_items",
  "thread/unsubscribe",
]);
if (MOCK) ALLOWED.add("turn/start");
const evidence = {
  startedAt: new Date().toISOString(), cli: CLI, asar: ASAR, run: RUN,
  policy: { generationRpcsAllowed: MOCK, generation: MOCK ? "LOCAL_FIXED_MOCK_SSE_ONLY" : "NONE",
    realModelGeneration: false, desktopStarted: false, credentialsInherited: false },
  commands: [], servers: [], checks: [], network: [], failures: [],
};
const sessions = new Set();
let abort = false;
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
function save() {
  fs.writeFileSync(path.join(RUN, "evidence.json"), JSON.stringify(evidence, null, 2) + "\n");
}
function check(name, passed, details) {
  evidence.checks.push({ name, passed: Boolean(passed), details });
}
function filesUnder(dir) {
  if (!fs.existsSync(dir)) return [];
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const file = path.join(dir, e.name);
    if (e.isSymbolicLink()) return [];
    return e.isDirectory() ? filesUnder(file) : [{ path: path.relative(RUN, file), size: fs.statSync(file).size }];
  });
}
function runtime(label) {
  const base = path.join(RUN, label);
  const dirs = {};
  for (const name of ["home", "sqlite", "profile", "appdata", "localappdata", "temp", "work", "state", "electron", "webview"]) {
    dirs[name] = path.join(base, name);
    fs.mkdirSync(dirs[name], { recursive: true });
  }
  return { base, ...dirs };
}
function environment(rt, port) {
  const env = {};
  // 白名单而非复制 process.env，避免继承凭据、代理、桥接和正式实例入口。
  for (const key of ["SystemRoot", "WINDIR", "ComSpec", "PATH", "PATHEXT", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"]) {
    if (process.env[key]) env[key] = process.env[key];
  }
  Object.assign(env, {
    CODEX_HOME: rt.home, CODEX_SQLITE_HOME: rt.sqlite, HOME: rt.profile, USERPROFILE: rt.profile,
    APPDATA: rt.appdata, LOCALAPPDATA: rt.localappdata, TEMP: rt.temp, TMP: rt.temp,
    CODEX_PLUS_STATE_DIR: rt.state, CODEX_ELECTRON_USER_DATA_PATH: rt.electron,
    WEBVIEW2_USER_DATA_FOLDER: rt.webview, CODEX_NATIVE_PROBE_KEY: "sk-fake-native-probe-never-real",
    OPENAI_API_KEY: "sk-fake-native-probe-never-real", OPENAI_BASE_URL: `http://127.0.0.1:${port}/v1`,
    NO_PROXY: "127.0.0.1,localhost", HTTP_PROXY: `http://127.0.0.1:${port}`,
    HTTPS_PROXY: `http://127.0.0.1:${port}`, ALL_PROXY: `http://127.0.0.1:${port}`,
    GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(rt.profile, ".gitconfig"),
    GIT_TERMINAL_PROMPT: "0", GCM_INTERACTIVE: "Never",
    GIT_CONFIG_COUNT: "1", GIT_CONFIG_KEY_0: "credential.helper", GIT_CONFIG_VALUE_0: "",
    DO_NOT_TRACK: "1", RUST_LOG: "warn",
  });
  return env;
}
async function command(args, env, cwd, timeout = 12000) {
  const row = { args, cwd, startedAt: new Date().toISOString() };
  evidence.commands.push(row);
  const child = spawn(CLI, args, { env, cwd, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  let stdout = "", stderr = "";
  child.stdout.on("data", (b) => { stdout += b.toString(); });
  child.stderr.on("data", (b) => { stderr += b.toString(); });
  const result = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { child.kill(); reject(new Error("command timeout")); }, timeout);
    child.once("error", (e) => { clearTimeout(timer); reject(e); });
    child.once("close", (code, signal) => { clearTimeout(timer); resolve({ code, signal }); });
  });
  Object.assign(row, result, { stdout, stderr });
  return row;
}
async function processSnapshot() {
  if (process.platform !== "win32") return [];
  const script = "Get-CimInstance Win32_Process | Select-Object Name,ProcessId,ParentProcessId,ExecutablePath,CreationDate | ConvertTo-Json -Compress";
  return new Promise((resolve, reject) => {
    const child = spawn("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", script],
      { windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
    let text = "";
    const timer = setTimeout(() => { child.kill(); reject(new Error("process snapshot timeout")); }, 10000);
    child.stdout.on("data", (b) => { text += b.toString(); });
    child.once("error", (e) => { clearTimeout(timer); reject(e); });
    child.once("close", (code) => {
      clearTimeout(timer);
      if (code !== 0) reject(new Error(`process snapshot exit ${code}`));
      else { try { resolve(text.trim() ? [].concat(JSON.parse(text)) : []); } catch (e) { reject(e); } }
    });
  });
}
class Rpc {
  constructor(rt, env, flags, experimentalApi, label) {
    this.rt = rt;
    this.pending = new Map();
    this.next = 1;
    this.record = { label, runtime: rt, flags, experimentalApi, calls: [], notifications: [], stderr: "" };
    evidence.servers.push(this.record);
    const overrides = [
      'model_provider="native_probe"', 'model="native-probe"',
      'model_providers.native_probe.name="Native isolated probe"',
      `model_providers.native_probe.base_url="${env.OPENAI_BASE_URL}"`,
      'model_providers.native_probe.env_key="CODEX_NATIVE_PROBE_KEY"',
      'model_providers.native_probe.wire_api="responses"',
      "model_providers.native_probe.requires_openai_auth=false",
      'approval_policy="never"', 'sandbox_mode="read-only"',
      "analytics.enabled=false", "feedback.enabled=false", "check_for_update_on_startup=false",
      "features.apps=false", "features.remote_control=false",
      "features.shell_snapshot=false",
      "features.plugins=false", "features.remote_plugin=false", "features.recommended_plugins=false",
      "features.hooks=false", "features.skip_host_skill_discovery=true",
      "features.api_key_model_discovery=false", "features.code_mode=false",
      "features.code_mode_host=false", "model_providers.native_probe.supports_websockets=false",
      "model_providers.native_probe.request_max_retries=0", "model_providers.native_probe.stream_max_retries=0",
      ...flags,
    ];
    const args = ["app-server", "--stdio", ...overrides.flatMap((v) => ["-c", v])];
    this.record.args = args;
    this.record.environment = Object.fromEntries(Object.entries(env).map(([k, v]) =>
      [k, /KEY|TOKEN|SECRET/.test(k) ? "<fake-only>" : v]));
    this.child = spawn(CLI, args, { env, cwd: rt.work, windowsHide: true, stdio: ["pipe", "pipe", "pipe"] });
    this.record.pid = this.child.pid;
    this.exited = new Promise((resolve) => this.child.once("exit", (code, signal) => {
      this.record.exit = { code, signal, at: new Date().toISOString() };
      for (const [id, p] of this.pending) { clearTimeout(p.timer); p.reject(new Error(`server exited during RPC ${id}`)); }
      this.pending.clear();
      resolve(true);
    }));
    this.closed = new Promise((resolve) => this.child.once("close", () => resolve(true)));
    this.child.on("error", (error) => { this.record.spawnError = error.message; });
    this.child.stdin.on("error", (error) => { this.record.stdinError = error.message; });
    this.child.stderr.on("data", (b) => { this.record.stderr = (this.record.stderr + b.toString()).slice(-180000); });
    readline.createInterface({ input: this.child.stdout }).on("line", (line) => {
      let msg;
      try { msg = JSON.parse(line); } catch { this.record.nonJson = (this.record.nonJson || []).concat(line).slice(-20); return; }
      if (Object.hasOwn(msg, "id") && !msg.method) {
        const p = this.pending.get(msg.id);
        if (p) { clearTimeout(p.timer); this.pending.delete(msg.id); p.resolve(msg); }
      } else {
        this.record.notifications.push(msg);
        if (Object.hasOwn(msg, "id")) this.write({ id: msg.id, error: { code: -32601, message: "No tools or generation in native probe" } });
      }
    });
    sessions.add(this);
  }
  write(msg) { this.child.stdin.write(JSON.stringify({ jsonrpc: "2.0", ...msg }) + "\n"); }
  async call(method, params, tag = method) {
    if (abort) throw new Error("Global deadline exceeded");
    if (!ALLOWED.has(method)) throw new Error(`RPC not allowlisted: ${method}`);
    const id = this.next++;
    const row = { id, method, params, tag, at: new Date().toISOString() };
    this.record.calls.push(row);
    const start = Date.now();
    try {
      const reply = await new Promise((resolve, reject) => {
        const timer = setTimeout(() => { this.pending.delete(id); reject(new Error(`RPC timeout: ${method}`)); }, RPC_MS);
        this.pending.set(id, { resolve, reject, timer });
        this.write({ id, method, params });
      });
      Object.assign(row, { elapsedMs: Date.now() - start, reply });
      save();
      return reply;
    } catch (e) { row.failure = e.message; throw e; }
  }
  async initialize() {
    const r = await this.call("initialize", {
      clientInfo: { name: "weixin_native_protocol_probe", title: "Isolated protocol probe", version: "1.0" },
      capabilities: { experimentalApi: this.record.experimentalApi },
    });
    if (r.error) throw new Error(`initialize: ${r.error.message}`);
    this.write({ method: "initialized", params: {} });
  }
  async stop() {
    if (this.stopping) return this.stopping;
    this.stopping = (async () => {
      this.record.processesBeforeStop = await processSnapshot();
      const descendants = new Set([this.child.pid]);
      let added = true;
      while (added) {
        added = false;
        for (const p of this.record.processesBeforeStop) {
          if (descendants.has(p.ParentProcessId) && !descendants.has(p.ProcessId)) {
            descendants.add(p.ProcessId); added = true;
          }
        }
      }
      this.record.ownedProcessTree = this.record.processesBeforeStop.filter((p) => descendants.has(p.ProcessId));
      if (!abort && !this.record.exit) {
        const ids = new Set(this.record.calls.filter((c) => c.method === "thread/start")
          .map((c) => c.reply?.result?.thread?.id).filter(Boolean));
        for (const threadId of ids) {
          try { await this.call("thread/unsubscribe", { threadId }, "cleanup:unsubscribe"); }
          catch (error) { this.record.unsubscribeFailure = error.message; break; }
        }
      }
      this.child.stdin.end();
      let done = await Promise.race([this.exited, delay(12000).then(() => false)]);
      this.record.cleanup = { eofNormalExit: done };
      if (!done) {
        // 只终止本次持有的子进程句柄，不按名称或目录杀进程。
        this.child.kill();
        done = await Promise.race([this.exited, delay(3000).then(() => false)]);
        this.record.cleanup.fallbackKill = true;
      }
      this.record.cleanup.exited = done;
      const pipesClosed = await Promise.race([this.closed, delay(1500).then(() => false)]);
      this.record.cleanup.pipesClosed = pipesClosed;
      if (!pipesClosed && done) {
        this.child.stdout.destroy();
        this.child.stderr.destroy();
      }
      sessions.delete(this);
      if (!done) throw new Error(`cleanup failed for PID ${this.child.pid}`);
    })();
    return this.stopping;
  }
}
function inspectAsar() {
  if (!ASAR || !fs.existsSync(ASAR)) return { available: false };
  const stat = fs.statSync(ASAR);
  const fd = fs.openSync(ASAR, "r");
  const matches = [];
  try {
    const prefix = Buffer.alloc(16);
    fs.readSync(fd, prefix, 0, 16, 0);
    const headerSize = prefix.readUInt32LE(4);
    const header = Buffer.alloc(prefix.readUInt32LE(12));
    fs.readSync(fd, header, 0, header.length, 16);
    const terms = [
      "sourceKinds:wM", "wM=", "ri=[", "ii=[", "Xi:", "$i:",
      "sourceKinds:a=n.$i", "sourceKinds:r.Xi", "function F8t", "function pi(",
      "sourceKinds:Hn", "Hn as ", "features.projects", "projects=true",
      "persistExtendedHistory", "threadStartKind", "experimentalApi:!0",
      "sourceKinds:i=wM", "appServer", "listRecentThreads(",
    ];
    function walk(entries, parent = "") {
      for (const [name, entry] of Object.entries(entries)) {
        const file = `${parent}/${name}`;
        if (entry.files) { walk(entry.files, file); continue; }
        if (!file.endsWith(".js") || entry.unpacked || entry.size > 24000000) continue;
        if (!/app-shared|src-CP7|src-DCNi|main-Bgi|panel-ea/.test(file)) continue;
        const bytes = Buffer.alloc(entry.size);
        fs.readSync(fd, bytes, 0, bytes.length, 8 + headerSize + Number(entry.offset));
        const source = bytes.toString("utf8");
        for (const term of terms) {
          let at = -1, n = 0;
          while ((at = source.indexOf(term, at + 1)) >= 0 && n++ < 8) {
            matches.push({ file, term, offset: at, excerpt: source.slice(Math.max(0, at - 550), at + 850) });
          }
        }
      }
    }
    walk(JSON.parse(header).files);
    return { available: true, size: stat.size, modifiedAt: stat.mtime.toISOString(), matches };
  } finally { fs.closeSync(fd); }
}
async function listVariants(s, tag, projectId) {
  const out = {};
  for (const [name, extra] of Object.entries({
    defaultScan: {}, defaultDb: { useStateDbOnly: true },
    emptySourcesDb: { sourceKinds: [], useStateDbOnly: true },
    appServerScan: { sourceKinds: ["appServer"] },
    appServerDb: { sourceKinds: ["appServer"], useStateDbOnly: true },
    nativeInteractiveDb: { sourceKinds: ["cli", "vscode"], useStateDbOnly: true },
    allProvidersAppServerDb: { sourceKinds: ["appServer"], modelProviders: [], useStateDbOnly: true },
    projectDb: { projectId, modelProviders: [], sourceKinds: ["appServer"], useStateDbOnly: true },
  })) out[name] = await s.call("thread/list", { limit: 100, ...extra }, `${tag}:${name}`);
  return out;
}
async function run(port) {
  const base = runtime("discovery");
  const env = environment(base, port);
  evidence.version = (await command(["--version"], env, base.work)).stdout.trim();
  evidence.featureList = await command(["features", "list"], env, base.work);
  evidence.nativeAssets = inspectAsar();
  const schemaDir = path.join(ROOT, "output/weixin-session-controls/schema");
  evidence.schema = {};
  for (const [name, rel] of Object.entries({
    threadStart: "v2/ThreadStartParams.json", threadResume: "v2/ThreadResumeParams.json",
    threadList: "v2/ThreadListParams.json", metadata: "v2/ThreadMetadataUpdateParams.json",
  })) evidence.schema[name] = JSON.parse(fs.readFileSync(path.join(schemaDir, rel), "utf8")).properties;
  evidence.featureMatrix = [];
  for (const projects of [null, false, true]) for (const experimental of [false, true]) {
    const label = `matrix-projects-${projects}-experimental-${experimental}`;
    const rt = runtime(label);
    const flags = projects === null ? [] : [`features.projects=${projects}`];
    const s = new Rpc(rt, environment(rt, port), flags, experimental, label);
    try {
      await s.initialize();
      const row = { projects, experimental };
      row.list = await s.call("project/list", {});
      row.create = await s.call("project/create", { name: label, idempotencyKey: label, roots: [{ path: rt.work }] });
      const projectId = row.create.result?.project?.id || "00000000-0000-4000-8000-000000000000";
      row.read = await s.call("project/read", { projectId });
      row.update = await s.call("project/update", { projectId, name: label + "-renamed" });
      row.startPlain = await s.call("thread/start", { cwd: rt.work });
      row.startProject = await s.call("thread/start", { cwd: rt.work, projectId });
      row.startDynamic = await s.call("thread/start", { cwd: rt.work, dynamicTools: [{
        type: "function", name: "native_probe_noop", description: "Never invoked in this probe",
        inputSchema: { type: "object", properties: {}, additionalProperties: false },
      }] });
      row.startExtended = await s.call("thread/start", { cwd: rt.work, persistExtendedHistory: true });
      const id = row.startPlain.result?.thread?.id;
      if (id) {
        row.rename = await s.call("thread/name/set", { threadId: id, name: label + "-thread" });
        row.metadata = await s.call("thread/metadata/update", { threadId: id, projectId });
      }
      evidence.featureMatrix.push(row);
    } finally { await s.stop(); }
    save();
  }
  const rt = runtime("persistence");
  const isolatedEnv = environment(rt, port);
  const flags = ["features.projects=true"];
  let s = new Rpc(rt, isolatedEnv, flags, true, "persistence-first-process");
  const full = evidence.persistence = { threads: [] };
  try {
    await s.initialize();
    const make = (key, name) => s.call("project/create", { name, idempotencyKey: key, roots: [{ path: rt.work }], metadata: { probe: "native" } });
    const a = await make("native-project-a", "Native project A");
    const b = await make("native-project-b", "Native project B");
    full.projectA = a.result?.project;
    full.projectB = b.result?.project;
    if (!full.projectA || !full.projectB) throw new Error("Projects unavailable with features.projects=true");
    full.idempotent = await make("native-project-a", "Native project A");
    check("project/create idempotency returns stable ID", full.idempotent.result?.project?.id === full.projectA.id);
    full.rename = await s.call("project/update", { projectId: full.projectA.id, name: "Native project A renamed", metadata: { probe: "renamed" } });
    full.read = await s.call("project/read", { projectId: full.projectA.id });
    check("project rename preserves ID", full.read.result?.project?.id === full.projectA.id && full.read.result?.project?.name === "Native project A renamed");
    for (const [label, extra] of [
      ["omitted", {}], ["extended-false", { persistExtendedHistory: false }],
      ["extended-true", { persistExtendedHistory: true }],
      ["extended-invalid-and-unknown", { persistExtendedHistory: "not-a-boolean", nativeProbeUnknownField: true }],
      ["legacy", { historyMode: "legacy" }], ["paginated", { historyMode: "paginated" }],
      ["ephemeral", { ephemeral: true }],
    ]) {
      const started = await s.call("thread/start", {
        cwd: rt.work, projectId: full.projectA.id, threadSource: "weixin-native-probe",
        dynamicTools: [{ type: "function", name: "native_probe_noop", description: "Not invoked",
          inputSchema: { type: "object", properties: {}, additionalProperties: false } }],
        ...extra,
      }, `persistence:${label}:start`);
      const thread = started.result?.thread;
      const row = { label, started };
      full.threads.push(row);
      if (!thread) continue;
      row.pathExistsAtStart = thread.path ? fs.existsSync(thread.path) : false;
      row.readMetadata = await s.call("thread/read", { threadId: thread.id, includeTurns: false }, `${label}:read-metadata`);
      row.readHistory = await s.call("thread/read", { threadId: thread.id, includeTurns: true }, `${label}:read-history`);
    }
    await delay(1500);
    full.filesBeforeEdits = filesUnder(rt.base);
    full.listsBeforeEdits = await listVariants(s, "before-edits", full.projectA.id);
    full.loadedBeforeEdits = await s.call("thread/loaded/list", {});
    for (const row of full.threads) {
      const t = row.started.result?.thread;
      if (!t) continue;
      row.rename = await s.call("thread/name/set", { threadId: t.id, name: `Native thread ${row.label} renamed` }, `${row.label}:rename`);
      row.assignB = await s.call("thread/metadata/update", { threadId: t.id, projectId: full.projectB.id }, `${row.label}:assign-b`);
      row.readAfterAssignB = await s.call("thread/read", { threadId: t.id, includeTurns: false }, `${row.label}:read-after-assign-b`);
      row.clearProject = await s.call("thread/metadata/update", { threadId: t.id, projectId: "" }, `${row.label}:clear-project`);
      row.readAfterClear = await s.call("thread/read", { threadId: t.id, includeTurns: false }, `${row.label}:read-after-clear`);
      row.assignA = await s.call("thread/metadata/update", { threadId: t.id, projectId: full.projectA.id }, `${row.label}:assign-a`);
      row.readAfterEdits = await s.call("thread/read", { threadId: t.id, includeTurns: false }, `${row.label}:read-after-edits`);
      const r = row.readAfterEdits.result?.thread;
      if (row.label !== "ephemeral") check(`thread stable ID and rename: ${row.label}`,
        r?.id === t.id && r?.name === `Native thread ${row.label} renamed`,
        { id: t.id, error: row.readAfterEdits.error });
    }
    await delay(1000);
    full.filesAfterEdits = filesUnder(rt.base);
    full.listsAfterEdits = await listVariants(s, "after-edits", full.projectA.id);
    // 纯历史追加探针，不启动 turn，也不请求模型；单独记录，避免混入空线程对照组。
    const injected = await s.call("thread/start", { cwd: rt.work, projectId: full.projectA.id }, "inject-control:start");
    full.injectControl = { started: injected };
    if (injected.result?.thread) {
      full.injectControl.inject = await s.call("thread/inject_items", { threadId: injected.result.thread.id, items: [{
        type: "message", role: "user", content: [{ type: "input_text", text: "Local history fixture only; no model request." }],
      }] });
      await delay(500);
      full.injectControl.read = await s.call("thread/read", { threadId: injected.result.thread.id, includeTurns: true });
    }
  } finally { await s.stop(); }
  full.filesAfterStop = filesUnder(rt.base);
  s = new Rpc(rt, isolatedEnv, flags, true, "persistence-second-process");
  try {
    await s.initialize();
    full.projectsAfterRestart = await s.call("project/list", {});
    full.projectReadAfterRestart = await s.call("project/read", { projectId: full.projectA.id });
    check("project stable ID and rename survive restart",
      full.projectReadAfterRestart.result?.project?.id === full.projectA.id &&
      full.projectReadAfterRestart.result?.project?.name === "Native project A renamed");
    full.listsAfterRestart = await listVariants(s, "restart", full.projectA.id);
    for (const row of full.threads) {
      const id = row.started.result?.thread?.id;
      if (id) row.afterRestart = await s.call("thread/read", { threadId: id, includeTurns: false }, `${row.label}:restart-read`);
    }
    if (full.injectControl.started.result?.thread) {
      full.injectControl.afterRestart = await s.call("thread/read", { threadId: full.injectControl.started.result.thread.id, includeTurns: true });
    }
  } finally { await s.stop(); }
  full.filesFinal = filesUnder(rt.base);
}
function readState(rt) {
  const { DatabaseSync } = require("node:sqlite");
  const file = path.join(rt.sqlite, "state_5.sqlite");
  if (!fs.existsSync(file)) return { available: false };
  const db = new DatabaseSync(file, { readOnly: true });
  try {
    return { available: true, rows: db.prepare(
      "SELECT id, source, originator, name, title, preview, has_user_event, first_user_message, project_id FROM threads"
    ).all() };
  } finally { db.close(); }
}
async function mockFirstMessage(port) {
  const rt = runtime("local-mock-first-message");
  const env = environment(rt, port);
  evidence.version = (await command(["--version"], env, rt.work)).stdout.trim();
  const s = new Rpc(rt, env, [], true, "LOCAL_MOCK_SSE_NOT_WEIXIN_E2E");
  const result = evidence.localMock = { kind: "LOCAL_FIXED_MOCK_SSE_ONLY", historyMode: "legacy" };
  const lists = async (tag) => {
    const out = {};
    for (const [name, extra] of Object.entries({
      defaultDb: { useStateDbOnly: true },
      defaultScan: { useStateDbOnly: false },
      appServerDb: { sourceKinds: ["appServer"], useStateDbOnly: true },
      vscodeDb: { sourceKinds: ["vscode"], useStateDbOnly: true },
    })) out[name] = await s.call("thread/list", { limit: 100, modelProviders: [], ...extra }, `${tag}:${name}`);
    return out;
  };
  try {
    await s.initialize();
    result.project = await s.call("project/create", { name: "Local mock project",
      idempotencyKey: "local-mock-project", roots: [{ path: rt.work }] });
    const projectId = result.project.result?.project?.id;
    if (!projectId) throw new Error("mock project/create failed");
    result.start = await s.call("thread/start", { cwd: rt.work, historyMode: "legacy" });
    const thread = result.start.result?.thread;
    if (!thread) throw new Error("mock thread/start failed");
    result.pathExistsBeforeMetadata = fs.existsSync(thread.path);
    // 这是 start 后的第一个 RPC，中间没有 read、rename 或 turn。
    result.immediateMetadata = await s.call("thread/metadata/update", { threadId: thread.id, projectId });
    check("legacy empty thread immediate metadata/update succeeds", !result.immediateMetadata.error, result.immediateMetadata);
    result.readAfterMetadata = await s.call("thread/read", { threadId: thread.id, includeTurns: false });
    result.pathExistsAfterMetadata = fs.existsSync(thread.path);
    result.dbBeforeTurn = readState(rt);
    result.listsBeforeTurn = await lists("mock:before-first-user-message");
    result.turnStart = await s.call("turn/start", { threadId: thread.id, input: [{
      type: "text", text: "Local protocol fixture first user message. Reply with the fixed local mock text only.",
    }] });
    const deadline = Date.now() + 25000;
    while (Date.now() < deadline && !s.record.notifications.some((n) =>
      n.method === "turn/completed" && n.params?.threadId === thread.id)) await delay(100);
    result.completed = s.record.notifications.find((n) => n.method === "turn/completed" && n.params?.threadId === thread.id);
    check("local mock SSE turn completed", result.completed?.params?.turn?.status === "completed", result.completed);
    await delay(400);
    result.pathExistsAfterTurn = fs.existsSync(thread.path);
    result.dbAfterTurn = readState(rt);
    result.listsAfterTurn = await lists("mock:after-first-user-message");
    result.readAfterTurn = await s.call("thread/read", { threadId: thread.id, includeTurns: true });
    check("first user message makes legacy thread visible in default DB list",
      result.listsAfterTurn.defaultDb.result?.data?.some((t) => t.id === thread.id));
    result.finalMetadata = await s.call("thread/metadata/update", { threadId: thread.id, projectId });
    result.rename = await s.call("thread/name/set", { threadId: thread.id, name: "Local mock renamed thread" });
    result.finalRead = await s.call("thread/read", { threadId: thread.id, includeTurns: false });
    check("legacy project assignment succeeds after mock turn", result.finalRead.result?.thread?.projectId === projectId);
  } finally { await s.stop(); }
  result.dbAfterStop = readState(rt);
}
function respondMockSse(res) {
  const text = "LOCAL_MOCK_REPLY_NO_REAL_MODEL";
  const message = { id: "msg_native_local_mock", type: "message", role: "assistant", status: "completed",
    content: [{ type: "output_text", text, annotations: [] }] };
  const response = { id: "resp_native_local_mock", object: "response", created_at: Math.floor(Date.now() / 1000),
    model: "native-probe", status: "completed", output: [message],
    usage: { input_tokens: 10, output_tokens: 6, total_tokens: 16 } };
  res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "close" });
  const events = [
    { type: "response.created", response: { ...response, status: "in_progress", output: [] } },
    { type: "response.output_item.added", output_index: 0, item: { ...message, status: "in_progress", content: [] } },
    { type: "response.content_part.added", item_id: message.id, output_index: 0, content_index: 0,
      part: { type: "output_text", text: "", annotations: [] } },
    { type: "response.output_text.delta", item_id: message.id, output_index: 0, content_index: 0, delta: text },
    { type: "response.output_text.done", item_id: message.id, output_index: 0, content_index: 0, text },
    { type: "response.content_part.done", item_id: message.id, output_index: 0, content_index: 0, part: message.content[0] },
    { type: "response.output_item.done", output_index: 0, item: message },
    { type: "response.completed", response },
  ];
  events.forEach((event, sequence_number) => res.write(`event: ${event.type}\ndata: ${JSON.stringify({ ...event, sequence_number })}\n\n`));
  res.end();
}
async function cleanupFollowup(file) {
  const previous = JSON.parse(fs.readFileSync(path.resolve(file), "utf8"));
  const snapshot = await processSnapshot();
  const tracked = (previous.remainingOwnedProcesses || []).map((p) => {
    const now = snapshot.find((n) => n.ProcessId === p.ProcessId);
    const sameIdentity = now && now.CreationDate === p.CreationDate && now.ExecutablePath === p.ExecutablePath;
    return { pid: p.ProcessId, name: p.Name, parentPid: p.ParentProcessId,
      expectedCreatedAt: p.CreationDate, pidPresent: Boolean(now), sameIdentityPresent: Boolean(sameIdentity) };
  });
  const formal = (p) => /^(codex|ChatGPT|codex-plus-plus|codex-plus-manager)\.exe$/i.test(p.Name);
  const originals = previous.processesBefore.filter(formal).map((p) => ({
    pid: p.ProcessId, name: p.Name,
    unchanged: snapshot.some((n) => n.ProcessId === p.ProcessId &&
      n.CreationDate === p.CreationDate && n.ExecutablePath === p.ExecutablePath),
  }));
  const result = { checkedAt: new Date().toISOString(), sourceEvidence: path.resolve(file),
    action: "READ_ONLY_NO_PROCESS_TERMINATION", tracked, originals,
    allSixOldHelpersExited: tracked.length === 6 && tracked.every((p) => !p.sameIdentityPresent),
    originalAppIdentitiesUnchanged: originals.every((p) => p.unchanged) };
  const output = path.join(ROOT, "output/weixin-session-controls/native-cleanup-followup.json");
  fs.writeFileSync(output, JSON.stringify(result, null, 2) + "\n");
  console.log(JSON.stringify(result, null, 2));
  if (!result.allSixOldHelpersExited || !result.originalAppIdentitiesUnchanged) process.exitCode = 1;
}
async function main() {
  if (CLEANUP_ONLY) {
    if (!process.argv[3]) throw new Error("Usage: node weixin-native-projects.cjs --cleanup-followup <previous-evidence.json>");
    return cleanupFollowup(process.argv[3]);
  }
  if (!CLI || CLI.startsWith("--") || !path.isAbsolute(CLI) || !fs.existsSync(CLI)) {
    throw new Error("Usage: node weixin-native-projects.cjs <absolute-codex-executable> [app.asar] [--mock-first-message]");
  }
  fs.mkdirSync(RUN, { recursive: true });
  evidence.processesBefore = await processSnapshot();
  const sink = http.createServer((req, res) => {
    // 不记录请求头或正文；只给本地 /v1/responses 返回写死的模拟 SSE。
    evidence.network.push({ method: req.method, url: req.url, at: new Date().toISOString() });
    req.resume();
    if (MOCK && req.method === "POST" && req.url === "/v1/responses") return respondMockSse(res);
    res.writeHead(503, { "content-type": "application/json", connection: "close" });
    res.end(JSON.stringify({ error: { message: "Native probe denies all generation", type: "probe_denied" } }));
  });
  sink.on("connect", (req, socket) => {
    evidence.network.push({ method: "CONNECT", url: req.url, denied: true });
    socket.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
  });
  const deadline = setTimeout(() => {
    abort = true;
    evidence.failures.push("Global 240 second deadline exceeded");
    for (const session of sessions) session.stop().catch((e) => { evidence.failures.push(e.message); });
  }, TOTAL_MS);
  try {
    sink.listen(0, "127.0.0.1");
    await once(sink, "listening");
    const port = sink.address().port;
    evidence.endpoint = `http://127.0.0.1:${port}/v1`;
    if (MOCK) await mockFirstMessage(port);
    else await run(port);
  } catch (error) {
    evidence.failures.push(error.stack);
    process.exitCode = 1;
  } finally {
    for (const session of [...sessions]) {
      try { await session.stop(); } catch (error) { evidence.failures.push(error.message); process.exitCode = 1; }
    }
    sink.closeAllConnections();
    await new Promise((resolve) => sink.close(resolve));
    evidence.endpointClosed = !sink.listening;
    clearTimeout(deadline);
    try {
      evidence.processesAfter = await processSnapshot();
      const identities = new Map(evidence.processesAfter.map((p) => [p.ProcessId, p]));
      const formal = (p) => /^(codex|ChatGPT|codex-plus-plus|codex-plus-manager)\.exe$/i.test(p.Name);
      const changed = evidence.processesBefore.filter(formal).filter((p) => {
        const after = identities.get(p.ProcessId);
        return !after || after.ExecutablePath !== p.ExecutablePath || after.CreationDate !== p.CreationDate;
      });
      check("pre-existing process identities unchanged", changed.length === 0, changed);
      const owned = evidence.servers.flatMap((s) => s.ownedProcessTree || []);
      const pids = new Set(evidence.servers.map((s) => s.pid).concat(owned.map((p) => p.ProcessId)));
      evidence.remainingOwnedProcesses = evidence.processesAfter.filter((p) => {
        if (pids.has(p.ParentProcessId)) return true;
        if (!pids.has(p.ProcessId)) return false;
        const before = owned.find((o) => o.ProcessId === p.ProcessId);
        return !before || before.CreationDate === p.CreationDate;
      });
      check("owned server processes exited", evidence.remainingOwnedProcesses.length === 0, evidence.remainingOwnedProcesses);
    } catch (error) { evidence.failures.push(error.message); process.exitCode = 1; }
    check(MOCK ? "only allowlisted local mock RPCs sent" : "no generation RPC sent",
      evidence.servers.every((s) => s.calls.every((c) => ALLOWED.has(c.method))));
    check(MOCK ? "only local fixed mock SSE requests received" : "no HTTP requests reached the local endpoint",
      MOCK ? evidence.network.length > 0 && evidence.network.every((r) => r.method === "POST" && r.url === "/v1/responses") :
        evidence.network.length === 0, evidence.network);
    check("all app-server children exited through EOF", evidence.servers.every((s) => s.cleanup?.eofNormalExit === true));
    evidence.finishedAt = new Date().toISOString();
    evidence.ok = evidence.failures.length === 0 && evidence.checks.every((c) => c.passed);
    if (!evidence.ok) process.exitCode = 1;
    save();
    console.log(JSON.stringify({ ok: evidence.ok, evidence: path.join(RUN, "evidence.json"),
      checks: evidence.checks, failures: evidence.failures }, null, 2));
  }
}
main().catch((error) => { console.error(error); process.exitCode = 1; });
