"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn, execFile } = require("node:child_process");
const execute = require("node:util").promisify(execFile);

const repo = path.resolve(__dirname, "../..");
const args = process.argv.slice(2);
const option = (key) => {
  const i = args.indexOf(key);
  assert(i >= 0 && args[i + 1], `Missing ${key}`);
  return path.resolve(args[i + 1]);
};
const cli = option("--cli");
const testBinary = option("--test-binary");
assert(fs.existsSync(cli) && fs.existsSync(testBinary), "CLI and existing test binary are required");
const output = path.join(repo, "output/weixin-session-controls",
  `native-visibility-seed-${new Date().toISOString().replace(/[:.]/g, "-")}`);
fs.mkdirSync(output, { recursive: true });
const dirs = {};
for (const name of ["home", "sqlite", "state", "appdata", "electron", "webview", "profile", "temp"]) {
  dirs[name] = path.join(output, name);
  fs.mkdirSync(dirs[name]);
}
const env = {};
for (const key of ["SystemRoot", "WINDIR", "ComSpec", "PATHEXT", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"]) {
  if (process.env[key]) env[key] = process.env[key];
}
Object.assign(env, {
  PATH: path.dirname(path.resolve(repo, "target/session-connection-release/release/codex-plus-plus.exe")) +
    path.delimiter + (process.env.PATH || ""),
  CODEX_PLUS_WEIXIN_ACCEPTANCE_ROOT: output, CODEX_PLUS_WEIXIN_ACCEPTANCE_CLI: cli,
  CODEX_HOME: dirs.home, CODEX_SQLITE_HOME: dirs.sqlite, CODEX_PLUS_STATE_DIR: dirs.state,
  APPDATA: dirs.appdata, LOCALAPPDATA: path.join(dirs.profile, "Local"), HOME: dirs.profile, USERPROFILE: dirs.profile,
  CODEX_ELECTRON_USER_DATA_PATH: dirs.electron, WEBVIEW2_USER_DATA_FOLDER: dirs.webview,
  TMP: dirs.temp, TEMP: dirs.temp, GIT_CONFIG_NOSYSTEM: "1",
  GIT_CONFIG_GLOBAL: path.join(dirs.profile, ".gitconfig"), GIT_TERMINAL_PROMPT: "0",
  GCM_INTERACTIVE: "Never", DO_NOT_TRACK: "1",
});
async function processes() {
  const code = 'ConvertTo-Json -Compress -InputObject @(Get-CimInstance Win32_Process | ForEach-Object { [pscustomobject]@{pid=$_.ProcessId;ppid=$_.ParentProcessId;name=$_.Name;exe=$_.ExecutablePath;born=$_.CreationDate.ToUniversalTime().ToString("o")} })';
  const result = await execute("powershell.exe", ["-NoProfile", "-NonInteractive", "-Command", code],
    { windowsHide: true, timeout: 15000, maxBuffer: 8e6 });
  return JSON.parse(result.stdout);
}
const same = (a, b) => a && b && a.pid === b.pid && a.exe === b.exe && a.born === b.born;
async function main() {
  const before = await processes();
  const report = { output, cli, testBinary, syntheticOnly: true, realWeixin: false,
    credentialSource: "NONE", startedAt: new Date().toISOString() };
  const child = spawn(testBinary, [
    "--exact", "connect::tests::installed_cli_controls_projects_and_footer_acceptance", "--ignored", "--nocapture",
  ], { env, cwd: repo, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
  const owned = new Map();
  let stdout = "", stderr = "", capturePending = false;
  child.stdout.on("data", (b) => { stdout += b.toString(); });
  child.stderr.on("data", (b) => { stderr += b.toString(); });
  async function capture() {
    if (capturePending) return;
    capturePending = true;
    try {
      const current = await processes();
      const root = current.find((p) => p.pid === child.pid);
      if (root) owned.set(root.pid, root);
      for (let changed = true; changed;) {
        changed = false;
        for (const p of current) if (!owned.has(p.pid) && owned.has(p.ppid) &&
          !before.some((b) => same(b, p))) { owned.set(p.pid, p); changed = true; }
      }
    } finally { capturePending = false; }
  }
  const monitor = setInterval(() => capture().catch((e) => { report.captureError = e.message; }), 1200);
  let timer;
  try {
    await capture();
    report.exit = await Promise.race([
      new Promise((resolve, reject) => {
        child.once("error", reject);
        child.once("exit", (code, signal) => resolve({ code, signal }));
      }),
      new Promise((_, reject) => { timer = setTimeout(() => reject(new Error("Seed fixture deadline: 150 seconds")), 150000); }),
    ]);
  } catch (e) { report.error = e.message; }
  finally {
    clearTimeout(timer);
    clearInterval(monitor);
    await capture();
    const current = await processes();
    const remaining = current.filter((p) => same(p, owned.get(p.pid)));
    report.remainingBeforeFallback = remaining;
    // 仅在失败路径按完整身份复核本次树；不按名称或目录结束其他测试。
    for (const p of remaining.reverse()) {
      const fresh = (await processes()).find((n) => n.pid === p.pid);
      if (same(p, fresh)) process.kill(p.pid);
    }
    const after = await processes();
    report.remaining = after.filter((p) => same(p, owned.get(p.pid)));
    const originals = before.filter((p) => /^(ChatGPT|codex|codex-plus-plus)\.exe$/i.test(p.name));
    report.originalsChanged = originals.filter((p) => !after.some((n) => same(p, n)));
    report.owned = [...owned.values()];
    report.finishedAt = new Date().toISOString();
    fs.writeFileSync(path.join(output, "test.stdout.txt"), stdout);
    fs.writeFileSync(path.join(output, "test.stderr.txt"), stderr);
    report.fixtureEvidence = fs.existsSync(path.join(output, "evidence.json")) ?
      JSON.parse(fs.readFileSync(path.join(output, "evidence.json"), "utf8")) : null;
    report.success = report.exit?.code === 0 && Boolean(report.fixtureEvidence?.localMocksOnly) &&
      !report.remaining.length && !report.originalsChanged.length;
    fs.writeFileSync(path.join(output, "seed-report.json"), JSON.stringify(report, null, 2));
    console.log(JSON.stringify(report, null, 2));
    if (!report.success) process.exitCode = 1;
    child.stdout.destroy();
    child.stderr.destroy();
  }
}
main().catch((e) => { console.error(e); process.exitCode = 1; });
