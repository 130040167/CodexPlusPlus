"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { spawn } = require("node:child_process");
if (process.argv[2] === "--skill-runner") {
  const file = path.resolve(process.argv[3]);
  const source = fs.readFileSync(file, "utf8");
  const anchor = "page.setDefaultTimeout(15000);";
  const pageAnchor = 'const page = browser.contexts().flatMap(c => c.pages()).find(p => !appOnly || p.url().startsWith("app://"));';
  assert.equal(source.split(anchor).length, 2, "Reviewed readiness-timeout adapter anchor changed");
  assert.equal(source.split(pageAnchor).length, 2, "Reviewed page-readiness adapter anchor changed");
  const Module = require("node:module");
  const runner = new Module(file);
  runner.filename = file;
  runner.paths = Module._nodeModulePaths(path.dirname(file));
  process.argv = [process.execPath, file, ...process.argv.slice(4)];
  process.mainModule = runner;
  runner._compile(source
    .replace(anchor, "page.setDefaultTimeout(options.timeout * 1000);")
    .replace(pageAnchor, 'let page; await until(() => { page = browser.contexts().flatMap(c => c.pages()).find(p => !appOnly || p.url().startsWith("app://")); return page; }, "Playwright application page");'), file);
  return;
}
const repo = path.resolve(__dirname, "../..");
const args = process.argv.slice(2);
function arg(key) {
  const i = args.indexOf(key);
  assert(i >= 0 && args[i + 1], `Missing ${key}`);
  return path.resolve(args[i + 1]);
}
const seed = arg("--seed"), cli = arg("--cli"), manager = arg("--manager");
const skill = arg("--skill"), playwright = arg("--playwright");
for (const file of [cli, manager, path.join(skill, "scripts/windows-session.cjs"), playwright]) assert(fs.existsSync(file), file);
assert(fs.existsSync(path.join(seed, "seed-report.json")));
const output = path.join(repo, "output/weixin-session-controls", `native-visibility-launch-${Date.now()}`);
fs.mkdirSync(output, { recursive: true });
const profile = path.join(output, "profile");
fs.mkdirSync(profile);
const env = {};
for (const key of ["SystemRoot", "WINDIR", "ComSpec", "PATH", "PATHEXT", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS"]) {
  if (process.env[key]) env[key] = process.env[key];
}
Object.assign(env, {
  // WebView2 使用真实 Windows 系统 profile 定位运行库；应用数据仍由技能全部隔离。
  HOME: profile, USERPROFILE: process.env.USERPROFILE, LOCALAPPDATA: process.env.LOCALAPPDATA,
  APPDATA: path.join(profile, "Roaming"), TMP: profile, TEMP: profile,
  CODEX_PLUS_NATIVE_VISIBILITY_SEED: seed, CODEX_PLUS_NATIVE_VISIBILITY_CLI: cli,
  GIT_CONFIG_NOSYSTEM: "1", GIT_CONFIG_GLOBAL: path.join(profile, ".gitconfig"),
  GIT_TERMINAL_PROMPT: "0", GCM_INTERACTIVE: "Never", DO_NOT_TRACK: "1",
});
const options = [
  __filename, "--skill-runner", path.join(skill, "scripts/windows-session.cjs"), "--repo", repo, "--mode", "paired",
  "--manager", manager, "--scenario", path.join(__dirname, "weixin-native-visibility-paired.cjs"),
  "--playwright-module", playwright, "--timeout", "120",
  "--build-label", "Existing session-connection-release candidate; synthetic seed only; no real Weixin service",
];
const child = spawn(process.execPath, options, { env, cwd: repo, windowsHide: true, stdio: ["ignore", "pipe", "pipe"] });
const record = { seed, cli, manager, skill, playwright, pid: child.pid, output, startedAt: new Date().toISOString(),
  runnerAdaptation: "In-memory bounded Playwright page discovery and readiness timeout; installed skill unchanged" };
fs.writeFileSync(path.join(output, "launch.json"), JSON.stringify(record, null, 2));
console.log(JSON.stringify(record, null, 2));
let stdout = "", stderr = "";
child.stdout.on("data", (b) => { stdout += b.toString(); process.stdout.write(b); });
child.stderr.on("data", (b) => { stderr += b.toString(); process.stderr.write(b); });
child.once("error", (e) => { console.error(e); process.exitCode = 1; });
child.once("close", (code, signal) => {
  Object.assign(record, { code, signal, finishedAt: new Date().toISOString() });
  fs.writeFileSync(path.join(output, "launch.json"), JSON.stringify(record, null, 2));
  fs.writeFileSync(path.join(output, "runner.stdout.txt"), stdout);
  fs.writeFileSync(path.join(output, "runner.stderr.txt"), stderr);
  process.exitCode = code || 0;
});
