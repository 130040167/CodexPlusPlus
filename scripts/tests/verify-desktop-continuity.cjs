// Windows acceptance test. Uses real isolated Codex windows and synthetic local suppliers.
// NODE_PATH must resolve Playwright; no dependencies are installed by this script.
const assert = require("node:assert/strict");
const fs = require("node:fs/promises");
const os = require("node:os");
const path = require("node:path");
const net = require("node:net");
const http = require("node:http");
const { spawn, execFileSync } = require("node:child_process");
const { chromium } = require("playwright");

assert.equal(process.platform, "win32");
assert.equal(process.argv.length, 5, "Usage: node verify-desktop-continuity.cjs <manager.exe> <Codex app dir> <output dir>");
const [executable, appDir, outputDir] = process.argv.slice(2).map(value => path.resolve(value));
const token = `desktop-continuity-${Date.now()}`;
const temporary = path.join(os.tmpdir(), token);
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
async function bounded(promise, ms) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error("operation timed out")), ms);
    })]);
  } finally { clearTimeout(timer); }
}
const powershell = (script, env = process.env) => execFileSync("powershell.exe",
  ["-NoProfile", "-NonInteractive", "-Command", script],
  { env, encoding: "utf8", windowsHide: true, maxBuffer: 12e6 });
const inventory = () => JSON.parse(powershell(
  "Get-CimInstance Win32_Process | Select-Object ProcessId,ParentProcessId,Name,ExecutablePath,@{Name='CreationDate';Expression={$_.CreationDate.ToUniversalTime().ToString('o')}},CommandLine | ConvertTo-Json -Compress"));
const identity = process => ({
  ProcessId: process.ProcessId, ParentProcessId: process.ParentProcessId,
  Name: process.Name, ExecutablePath: process.ExecutablePath, CreationDate: process.CreationDate,
});
const sameProcess = (a, b) => a && b && a.ProcessId === b.ProcessId
  && a.CreationDate === b.CreationDate && a.ExecutablePath === b.ExecutablePath;
const before = inventory();
const protectedProcesses = before.filter(process => /^(codex-plus-plus|codex|ChatGPT|cc-switch)\.exe$/i.test(process.Name));
const children = [], contexts = [], desktops = [], owned = new Map(), servers = new Set(), ports = new Set();
const report = { temporary, executable, appDir, checks: {}, events: [], managers: [], desktops: [], cleanup: false };
const protectedFiles = new Map();
let mode = "complete", attempts = 0, held = [];

async function save() {
  await fs.mkdir(outputDir, { recursive: true });
  await fs.writeFile(path.join(outputDir, "desktop-verification.json"), JSON.stringify(report, null, 2));
}
async function pass(name, detail = {}) {
  report.checks[name] = true;
  report.events.push({ name, time: new Date().toISOString(), ...detail });
  console.log(`PASS ${name}`);
  await save();
}
async function until(check, message, timeout = 45000) {
  const deadline = Date.now() + timeout;
  while (Date.now() < deadline) {
    if (await check()) return;
    await pause(150);
  }
  throw new Error(message);
}
async function listen(server, port = 0) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(port, "127.0.0.1", resolve);
  });
  servers.add(server);
  ports.add(server.address().port);
  return server.address().port;
}
async function close(server) {
  if (!server?.listening) return;
  server.closeAllConnections?.();
  await new Promise(resolve => server.close(resolve));
  servers.delete(server);
}
async function freePort() {
  const server = net.createServer();
  const port = await listen(server);
  await close(server);
  return port;
}
function captureOwned() {
  const processes = inventory();
  for (const child of children) if (child.exitCode === null && !owned.has(child.pid)) {
    const candidate = processes.find(process => process.ProcessId === child.pid);
    if (candidate?.ParentProcessId === process.pid && candidate.ExecutablePath === executable) {
      owned.set(candidate.ProcessId, identity(candidate));
    }
  }
  const ids = new Set(processes.filter(process => sameProcess(process, owned.get(process.ProcessId)))
    .map(process => process.ProcessId));
  for (const process of processes) {
    if (/^(ChatGPT|codex|codex-plus-plus|msedgewebview2|cmd)\.exe$/i.test(process.Name)
      && process.CommandLine?.includes(temporary) && !before.some(prior => sameProcess(prior, process))) {
      ids.add(process.ProcessId);
    }
  }
  for (let changed = true; changed;) {
    changed = false;
    for (const process of processes) {
      if (ids.has(process.ParentProcessId) && !ids.has(process.ProcessId)
        && !before.some(prior => sameProcess(prior, process))) {
        ids.add(process.ProcessId);
        changed = true;
      }
    }
  }
  for (const process of processes) if (ids.has(process.ProcessId)) {
    assert.ok(!protectedProcesses.some(prior => sameProcess(prior, process)), "test must never own a protected process");
    owned.set(process.ProcessId, identity(process));
  }
  report.owned = [...owned.values()];
}
function killOwned(pid) {
  const actual = inventory().find(process => process.ProcessId === pid);
  if (!actual) return;
  assert.ok(sameProcess(actual, owned.get(pid)), `refused to terminate unverified PID ${pid}`);
  assert.ok(!protectedProcesses.some(process => sameProcess(process, actual)));
  powershell(
    "$ErrorActionPreference = 'Stop'; $target = [Diagnostics.Process]::GetProcessById([int]$env:DESKTOP_TEST_PID); " +
    "try { $null = $target.SafeHandle; $birth = [DateTimeOffset]::Parse($env:DESKTOP_TEST_BIRTH).UtcDateTime; " +
    "if ($target.Path -eq $env:DESKTOP_TEST_EXE -and " +
    "[Math]::Abs(($target.StartTime.ToUniversalTime() - $birth).Ticks) -lt 10) { " +
    "$target.Kill(); $null = $target.WaitForExit(2000) } else { throw 'Process identity changed' } } " +
    "finally { $target.Dispose() }",
    { ...process.env, DESKTOP_TEST_PID: String(pid), DESKTOP_TEST_EXE: actual.ExecutablePath,
      DESKTOP_TEST_BIRTH: actual.CreationDate });
}
async function protectOriginalFiles() {
  for (const file of [
    path.join(process.env.USERPROFILE, ".codex", "config.toml"),
    path.join(process.env.USERPROFILE, ".codex-session-delete", "settings.json"),
    ...(process.env.CODEX_PLUS_STATE_DIR ? [path.join(process.env.CODEX_PLUS_STATE_DIR, "settings.json")] : []),
  ]) protectedFiles.set(file, await fs.readFile(file).catch(error => {
    if (error.code !== "ENOENT") throw error;
    return null;
  }));
}
async function originalsIntact() {
  const current = inventory();
  assert.ok(protectedProcesses.every(original => current.some(process => sameProcess(original, process))),
    "original application process identities changed");
  for (const [file, original] of protectedFiles) {
    const current = await fs.readFile(file).catch(() => null);
    assert.ok(original === null ? current === null : current?.equals(original), "original configuration changed");
  }
}
async function supplier() {
  const server = http.createServer(async (request, response) => {
    try {
      if (request.method === "GET") {
        response.setHeader("content-type", "application/json");
        response.end(JSON.stringify({ object: "list", data: [{ id: "gpt-5.1-codex", object: "model", owned_by: "fixture" }] }));
        return;
      }
      const chunks = [];
      for await (const chunk of request) chunks.push(chunk);
      const body = JSON.parse(Buffer.concat(chunks).toString() || "{}");
      report.events.push({ name: "supplierRequest", time: new Date().toISOString(),
        url: request.url, model: body.model, stream: body.stream });
      assert.ok(request.url.endsWith("/responses"), `Unexpected supplier path ${request.url}`);
      const answer = `DESKTOP_RECOVERED_${mode}`;
      const responseId = `resp_${++attempts}`;
      const item = { id: `msg_${attempts}`, type: "message", role: "assistant", status: "completed",
        content: [{ type: "output_text", text: answer, annotations: [] }] };
      if (!body.stream) {
        response.setHeader("content-type", "application/json");
        response.end(JSON.stringify({ id: responseId, object: "response", status: "completed", output: [item] }));
        return;
      }
      response.setHeader("content-type", "text/event-stream");
      response.setHeader("cache-control", "no-cache");
      const send = (type, value) => response.write(`event: ${type}\ndata: ${JSON.stringify({ type, ...value })}\n\n`);
      send("response.created", { response: { id: responseId, object: "response", status: "in_progress", output: [] } });
      send("response.output_item.added", { output_index: 0, item: { ...item, status: "in_progress", content: [] } });
      send("response.content_part.added", { output_index: 0, item_id: item.id, content_index: 0,
        part: { type: "output_text", text: "", annotations: [] } });
      if (mode === "hold") {
        send("response.output_text.delta", { item_id: item.id, output_index: 0, content_index: 0, delta: "PARTIAL" });
        held.push(response);
        return;
      }
      send("response.output_text.delta", { item_id: item.id, output_index: 0, content_index: 0, delta: answer });
      send("response.output_text.done", { item_id: item.id, output_index: 0, content_index: 0, text: answer });
      send("response.output_item.done", { output_index: 0, item });
      send("response.completed", { response: { id: responseId, object: "response", status: "completed",
        output: [item], usage: { input_tokens: 10, output_tokens: 5, total_tokens: 15 } } });
      response.end();
    } catch (error) {
      report.supplierError = String(error);
      response.destroy(error);
    }
  });
  return `http://127.0.0.1:${await listen(server)}`;
}
async function context(name, upstream) {
  const state = path.join(temporary, name), home = path.join(state, "home");
  await fs.mkdir(home, { recursive: true });
  const profile = {
    id: "fixture", name: "Desktop Fixture", relayMode: "pureApi", protocol: "responses",
    webSearchHistoryCompat: true, apiKey: "synthetic-desktop-test", model: "gpt-5.1-codex",
    modelList: "gpt-5.1-codex", baseUrl: `${upstream}/v1`, upstreamBaseUrl: `${upstream}/v1`,
    configContents: `model = "gpt-5.1-codex"\nmodel_provider = "custom"\n[model_providers.custom]\nname = "Fixture"\nrequires_openai_auth = false\nwire_api = "responses"\nbase_url = "${upstream}/v1"\n`,
    authContents: '{"OPENAI_API_KEY":"synthetic-desktop-test"}',
  };
  await fs.writeFile(path.join(state, "settings.json"), JSON.stringify({
    localConfigProviderImported: true, providerSyncEnabled: false, codexAppAutoTakeover: false,
    enhancementsEnabled: false, codexAppStandaloneQuotaUnlock: false, codexAppPath: appDir,
    relayProfilesEnabled: true, activeRelayId: profile.id, relayProfiles: [profile],
    relayCommonConfigContents: "analytics.enabled=false\nsandbox_mode=\"danger-full-access\"\napproval_policy=\"never\"\n[windows]\nsandbox=\"unelevated\"\n[features]\nplugins=false\napps=false\nremote_plugin=false\n",
  }));
  const result = { state, home, sqlite: path.join(state, "sqlite"), debug: await freePort(),
    guard: await freePort(), launcherGuard: await freePort(), port: await freePort() };
  contexts.push(result);
  return result;
}
function environment(context) {
  const env = { ...process.env, CODEX_HOME: context.home, CODEX_SQLITE_HOME: context.sqlite,
    CODEX_PLUS_STATE_DIR: context.state, CODEX_PLUS_MANAGER_GUARD_PORT: String(context.guard),
    CODEX_PLUS_LAUNCHER_GUARD_PORT: String(context.launcherGuard), CODEX_PLUS_PROTOCOL_PROXY_PORT: String(context.port),
    WEBVIEW2_USER_DATA_FOLDER: path.join(context.state, "test-webview"),
    WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS: `--remote-debugging-port=${context.debug}`,
    CODEX_PLUS_HELPER_BIND: "127.0.0.1", OTEL_SDK_DISABLED: "true" };
  for (const name of ["CODEX_PLUS_GUARD_PORT", "CODEX_PLUS_MULTI_INSTANCE", "CODEX_PLUS_INSTANCE_ID",
    "CODEX_PLUS_INSTANCE_LABEL", "OPENAI_API_KEY", "OPENAI_BASE_URL"]) delete env[name];
  return env;
}
async function startManager(context) {
  const started = Date.now();
  context.current = spawn(executable, [], { env: environment(context), windowsHide: true, stdio: "ignore" });
  children.push(context.current);
  await until(async () => {
    assert.equal(context.current.exitCode, null, "manager exited before WebView became ready");
    try { return (await fetch(`http://127.0.0.1:${context.debug}/json`)).ok; } catch { return false; }
  }, "manager WebView did not start");
  context.browser = await chromium.connectOverCDP(`http://127.0.0.1:${context.debug}`);
  context.page = context.browser.contexts()[0].pages()[0];
  await context.page.waitForFunction(() => window.__TAURI_INTERNALS__?.invoke);
  captureOwned();
  report.managers.push({ state: context.state, pid: context.current.pid, started, helperPort: context.port });
  await save();
}
const invoke = (context, command, args = {}) => context.page.evaluate(
  ({ command, args }) => window.__TAURI_INTERNALS__.invoke(command, args), { command, args });
async function stopManager(context, crash = false) {
  if (!context.current || context.current.exitCode !== null) return;
  captureOwned();
  if (crash) killOwned(context.current.pid);
  else await invoke(context, "manager_exit_app").catch(() => {});
  await until(() => context.current.exitCode !== null, "manager did not exit", 15000);
  await context.browser?.close().catch(() => {});
  context.browser = context.page = null;
}
async function launchDesktop(context) {
  const instanceId = token;
  const root = path.join(context.state, "codex-multi", instanceId);
  const home = path.join(root, "codex-home"), workspace = path.join(context.state, "workspace");
  await fs.mkdir(home, { recursive: true });
  await fs.mkdir(workspace, { recursive: true });
  await fs.writeFile(path.join(home, ".codex-global-state.json"), JSON.stringify({
    "electron-saved-workspace-roots": [workspace],
    "active-workspace-roots": [workspace],
    "electron-persisted-atom-state": { "electron:onboarding-projectless-completed": true,
      last_completed_onboarding: Math.floor(Date.now() / 1000) },
  }));
  const result = await invoke(context, "launch_codex_multi_instance", { request: {
    instanceId, profileId: "fixture", appPath: appDir, debugPort: await freePort(),
  } });
  assert.notEqual(result.status, "failed", result.message);
  const desktop = { context, root, instanceId };
  desktops.push(desktop);
  await until(async () => {
    captureOwned();
    const result = await fs.readFile(path.join(root, "launch-result.json"), "utf8")
      .then(JSON.parse).catch(() => null);
    if (!result) return false;
    desktop.result = result;
    assert.notEqual(result.status, "failed", result.message);
    return result.identity;
  }, "real desktop launch did not become ready", 135000);
  desktop.id = desktop.result.identity;
  ports.add(desktop.id.debugPort);
  ports.add(desktop.id.helperPort);
  const listed = await invoke(context, "list_codex_multi_instances");
  desktop.manifest = listed.instances.find(instance => instance.instanceId === instanceId);
  assert.ok(desktop.manifest?.running, JSON.stringify(listed));
  const data = path.resolve(desktop.id.userDataDir.replace(/^\\\\\?\\/, ""));
  desktop.actualData = data;
  const browserProcess = inventory().find(process => process.ProcessId === desktop.id.processId);
  assert.equal(desktop.id.instanceId, token);
  assert.ok(browserProcess?.CommandLine.includes(data) && browserProcess.CommandLine.includes(
    `--remote-debugging-port=${desktop.id.debugPort}`), "launched browser must match its receipt");
  assert.ok(!protectedProcesses.some(original => sameProcess(original, browserProcess)));
  owned.set(browserProcess.ProcessId, identity(browserProcess));
  captureOwned();
  report.desktops.push({ ...desktop.id, codexHome: desktop.manifest.codexHome,
    sqliteHome: desktop.manifest.sqliteHome, workerPid: desktop.manifest.pid });
  await save();
  assert.ok(data.toLowerCase().startsWith(path.resolve(temporary).toLowerCase() + path.sep),
    "new Electron data must stay in the isolated state tree");
  desktop.browser = await chromium.connectOverCDP(`http://127.0.0.1:${desktop.id.debugPort}`);
  const errors = [];
  for (const context of desktop.browser.contexts()) {
    context.on("page", page => page.on("pageerror", error => errors.push(String(error))));
    for (const page of context.pages()) page.on("pageerror", error => errors.push(String(error)));
  }
  await until(async () => {
    const candidates = [];
    for (const page of desktop.browser.contexts().flatMap(context => context.pages())) {
      const ui = await page.evaluate(() => ({
        title: document.title, text: document.body?.innerText?.slice(0, 1500) || "",
        elements: document.body?.querySelectorAll("*").length,
        composer: !!document.querySelector('[contenteditable="true"]'),
      })).catch(() => null);
      candidates.push({ url: page.url(), ...ui });
      if (ui?.composer && page.url().startsWith("app://")) desktop.page = page;
    }
    desktop.candidates = candidates;
    return desktop.page;
  }, "real desktop has no rendered application page", 60000).catch(error => {
    report.events.push({ name: "blankDesktop", candidates: desktop.candidates, errors });
    throw error;
  });
  await desktop.page.waitForLoadState("domcontentloaded");
  await desktop.page.screenshot({ path: path.join(outputDir, `desktop-${desktops.length}.png`) });
  await pass(`realDesktop${desktops.length}Ready`, { processId: desktop.id.processId, debugPort: desktop.id.debugPort });
  return desktop;
}
async function snapshot(desktop, label) {
  const ui = await desktop.page.evaluate(() => ({
    url: location.href, text: document.body.innerText.slice(-12000),
    fields: [...document.querySelectorAll("input,textarea,[contenteditable=true]")].map(el => ({
      tag: el.tagName, role: el.getAttribute("role"), placeholder: el.getAttribute("placeholder"),
      aria: el.getAttribute("aria-label"), contenteditable: el.getAttribute("contenteditable"),
    })),
    buttons: [...document.querySelectorAll("button")].map(el => ({
      text: el.innerText, aria: el.getAttribute("aria-label"), title: el.getAttribute("title"),
      disabled: el.disabled,
    })).filter(button => button.text || button.aria || button.title),
  }));
  report.events.push({ name: label, ui });
  await desktop.page.screenshot({ path: path.join(outputDir, `${label}.png`) });
  await save();
}
async function sendPrompt(desktop, text) {
  const dismiss = desktop.page.getByRole("button", { name: "继续使用当前模型", exact: true });
  if (await dismiss.isVisible()) await dismiss.click();
  await until(async () => !await desktop.page.getByText("正在完成 Windows 设置", { exact: true }).isVisible(),
    "isolated desktop sandbox setup did not finish", 60000);
  if (await dismiss.isVisible()) await dismiss.click();
  await snapshot(desktop, "before-prompt");
  const composer = desktop.page.locator('[contenteditable="true"]').first();
  await composer.waitFor({ state: "visible", timeout: 15000 });
  await composer.fill(text);
  await composer.press("Enter");
}
async function assertDesktopAlive(desktop) {
  const browser = await fetch(`http://127.0.0.1:${desktop.id.debugPort}/json/version`,
    { signal: AbortSignal.timeout(2000) }).then(response => response.json());
  assert.ok(browser.webSocketDebuggerUrl.endsWith(`/devtools/browser/${desktop.id.browserId}`));
  const processes = inventory();
  assert.ok(processes.some(process => sameProcess(process, owned.get(desktop.id.processId))));
}
async function closeDesktop(desktop) {
  if (!desktop.id) return;
  try {
    await assertDesktopAlive(desktop);
    const session = await desktop.browser.newBrowserCDPSession();
    await bounded(session.send("Browser.close"), 5000).catch(() => {});
  } catch {}
  if (desktop.browser) await bounded(desktop.browser.close(), 3000).catch(() => {});
}

(async () => {
  await fs.mkdir(outputDir, { recursive: true });
  try {
    await protectOriginalFiles();
    const upstream = await supplier();
    const a = await context("manager-a", upstream), b = await context("manager-b", upstream);
    await startManager(a);
    const first = await launchDesktop(a);
    await originalsIntact();
    await startManager(b);
    const second = await launchDesktop(b);
    assert.notEqual(first.id.userDataDir, second.id.userDataDir);
    assert.notEqual(first.manifest.codexHome, second.manifest.codexHome);
    assert.notEqual(first.manifest.sqliteHome, second.manifest.sqliteHome);
    assert.notEqual(first.id.processId, second.id.processId);
    assert.notEqual(first.id.browserId, second.id.browserId);
    await pass("sameNameAcrossManagersIsIsolated");
    const repeated = await invoke(a, "launch_codex_multi_instance", { request: {
      instanceId: token, profileId: "fixture", appPath: appDir, debugPort: first.id.debugPort,
    } });
    assert.notEqual(repeated.status, "failed", repeated.message);
    assert.equal(repeated.pid, first.manifest.pid);
    await assertDesktopAlive(first);
    await pass("repeatedLaunchKeepsOriginalDesktop");
    await sendPrompt(second, "Return a short synthetic response without tools.");
    await second.page.getByText("DESKTOP_RECOVERED_complete", { exact: true }).waitFor({ timeout: 30000 });
    await pass("secondRealDesktopCompletesSyntheticTurn");
    const workerPid = first.manifest.pid;
    await stopManager(a);
    await assertDesktopAlive(first);
    await startManager(a);
    const listed = await invoke(a, "list_codex_multi_instances");
    assert.ok(listed.instances.some(instance => instance.instanceId === token && instance.running));
    await pass("managerRestartRediscoversLiveDesktop");
    await stopManager(a, true);
    await assertDesktopAlive(first);
    await assertDesktopAlive(second);
    await startManager(a);
    const afterCrash = await invoke(a, "list_codex_multi_instances");
    assert.ok(afterCrash.instances.some(instance => instance.instanceId === token && instance.running));
    await pass("managerCrashRestartPreservesBothDesktops");
    await sendPrompt(first, "Return a short synthetic response without tools.");
    await first.page.getByText("DESKTOP_RECOVERED_complete", { exact: true }).waitFor({ timeout: 30000 });
    await pass("firstRealDesktopCompletesAfterManagerRestart");
    // Reproduce a lost proxy host while preserving its real Codex desktop and app-server.
    const resume = { state: path.join(first.root, "worker-home", ".codex-session-delete"),
      home: first.manifest.codexHome, sqlite: first.manifest.sqliteHome, debug: await freePort(),
      guard: await freePort(), launcherGuard: await freePort(), port: await freePort() };
    contexts.push(resume);
    mode = "hold";
    await sendPrompt(first, "Return another short synthetic response without tools.");
    await until(() => held.length > 0, "desktop did not start held streaming request");
    captureOwned();
    killOwned(workerPid);
    mode = "after_crash";
    const blocker = http.createServer((_request, response) => response.writeHead(503).end());
    await listen(blocker, resume.port);
    await startManager(resume);
    await first.page.getByText("DESKTOP_RECOVERED_after_crash", { exact: true }).waitFor({ timeout: 90000 });
    await assertDesktopAlive(first);
    await pass("realDesktopRetriesAfterProxyHostCrash", { cachedPort: first.id.helperPort, newPort: resume.port });
    const resumeLog = await fs.readFile(path.join(resume.state, "codex-plus.log"), "utf8");
    assert.ok(resumeLog.includes("helper.proxy_primary_waiting"));
    await pass("realDesktopRecoversWhileNewPrimaryIsOccupied");
    await close(blocker);
    mode = "hold";
    const previousHeld = held.length;
    await sendPrompt(first, "Return a final short synthetic response without tools.");
    await until(() => held.length > previousHeld, "desktop did not start second held stream");
    await stopManager(resume);
    mode = "after_exit";
    resume.port = await freePort();
    const history = path.join(resume.state, "proxy-endpoints.json");
    await fs.access(path.join(resume.state, "proxy-endpoints.backup.json"));
    await fs.writeFile(history, '{"truncated":');
    await startManager(resume);
    await first.page.getByText("DESKTOP_RECOVERED_after_exit", { exact: true }).waitFor({ timeout: 90000 });
    await assertDesktopAlive(first);
    await pass("realDesktopRetriesAfterNormalProxyHostExit");
    const repaired = JSON.parse(await fs.readFile(history, "utf8"));
    assert.ok(repaired.homes.some(home => home.ports.includes(first.id.helperPort)));
    await pass("realDesktopRecoversWithCorruptedPrimaryHistory");
    const manifestPath = path.join(second.root, "instance.json"), receiptPath = path.join(second.root, "launch-result.json");
    const savedManifest = await fs.readFile(manifestPath, "utf8"), savedReceipt = await fs.readFile(receiptPath, "utf8");
    try {
      const staleManifest = JSON.parse(savedManifest), staleReceipt = JSON.parse(savedReceipt);
      staleManifest.identity.browserId = staleReceipt.identity.browserId = "stale-test-browser";
      await fs.writeFile(receiptPath, JSON.stringify(staleReceipt));
      await fs.writeFile(manifestPath, JSON.stringify(staleManifest));
      const refused = await invoke(b, "stop_codex_multi_instance", { request: { instanceId: token } });
      assert.equal(refused.status, "failed", "stale receipt must never authorize a stop");
      await assertDesktopAlive(first);
      await assertDesktopAlive(second);
      await pass("staleIdentityRefusesToStopEitherDesktop");
    } finally {
      await fs.writeFile(receiptPath, savedReceipt);
      await fs.writeFile(manifestPath, savedManifest);
    }
    const stopped = await invoke(b, "stop_codex_multi_instance", { request: { instanceId: token } });
    assert.notEqual(stopped.status, "failed", stopped.message);
    await assertDesktopAlive(first);
    await originalsIntact();
    await pass("stoppingOneRealInstancePreservesOthers");
    await snapshot(first, "desktop-recovered");
  } catch (error) {
    report.error = error.stack || String(error);
    console.error(report.error);
    for (const desktop of desktops) if (desktop.page) {
      await snapshot(desktop, `failure-${desktops.indexOf(desktop)}`).catch(() => {});
    }
    for (const context of contexts) {
      const log = await fs.readFile(path.join(context.state, "codex-plus.log"), "utf8").catch(() => "");
      report.events.push({ name: "diagnostic", state: context.state, text: log.slice(-20000) });
    }
    process.exitCode = 1;
  } finally {
    for (const desktop of desktops) await closeDesktop(desktop).catch(() => {});
    for (const context of contexts) await stopManager(context).catch(error => {
      report.cleanupErrors = [...(report.cleanupErrors || []), String(error)];
    });
    captureOwned();
    for (const process of inventory()) if (sameProcess(process, owned.get(process.ProcessId))) {
      try { killOwned(process.ProcessId); }
      catch (error) { report.cleanupErrors = [...(report.cleanupErrors || []), String(error)]; }
    }
    for (const response of held) response.destroy();
    for (const server of [...servers]) await close(server);
    await until(() => !inventory().some(process => sameProcess(process, owned.get(process.ProcessId))),
      "owned test descendants remain alive", 15000).catch(error => {
        report.cleanupErrors = [...(report.cleanupErrors || []), String(error)];
      });
    report.checks.ownedProcessesExited = !inventory().some(process => sameProcess(process, owned.get(process.ProcessId)));
    report.checks.originalsPreserved = await originalsIntact().then(() => true, () => false);
    report.checks.portsReleased = true;
    report.unreleasedPorts = [];
    for (const port of [...ports]) {
      const server = net.createServer();
      try { await listen(server, port); }
      catch { report.unreleasedPorts.push(port); }
      finally { await close(server); }
    }
    // Windows may retain TIME_WAIT sockets after every owner has exited.
    if (report.unreleasedPorts.length) {
      const listeners = JSON.parse(powershell(
        "ConvertTo-Json -Compress -InputObject @(Get-NetTCPConnection -State Listen -ErrorAction SilentlyContinue | Select-Object LocalPort,OwningProcess)"));
      report.remainingListeners = listeners.filter(listener => report.unreleasedPorts.includes(listener.LocalPort));
      report.checks.portsReleased = report.remainingListeners.length === 0;
    }
    if (report.checks.ownedProcessesExited) {
      assert.equal(path.dirname(temporary), os.tmpdir());
      assert.equal(path.basename(temporary), token);
      try {
        for (const desktop of desktops) if (desktop.actualData && !desktop.actualData.startsWith(temporary + path.sep)) {
          assert.equal(desktop.actualData, path.join(process.env.LOCALAPPDATA, "CodexPlusPlus", "multi", token));
          powershell("$ErrorActionPreference = 'Stop'; Add-Type -AssemblyName Microsoft.VisualBasic; " +
            "[Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory($env:DESKTOP_TEST_TEMP, 'OnlyErrorDialogs', 'SendToRecycleBin')",
          { ...process.env, DESKTOP_TEST_TEMP: desktop.actualData });
        }
        powershell("$ErrorActionPreference = 'Stop'; Add-Type -AssemblyName Microsoft.VisualBasic; " +
          "[Microsoft.VisualBasic.FileIO.FileSystem]::DeleteDirectory($env:DESKTOP_TEST_TEMP, 'OnlyErrorDialogs', 'SendToRecycleBin')",
        { ...process.env, DESKTOP_TEST_TEMP: temporary });
        report.cleanup = true;
      } catch (error) { report.cleanupErrors = [...(report.cleanupErrors || []), String(error)]; }
    }
    report.owned = [...owned.values()];
    await save();
    console.log(JSON.stringify({ checks: report.checks, cleanup: report.cleanup, error: report.error }, null, 2));
    if (!report.cleanup || report.cleanupErrors?.length || Object.values(report.checks).some(value => !value)) process.exitCode = 1;
  }
})();
