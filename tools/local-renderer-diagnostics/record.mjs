import { readFileSync, mkdirSync, appendFileSync, existsSync, statSync, writeFileSync, renameSync } from 'node:fs';
import { resolve, join } from 'node:path';
import { homedir } from 'node:os';
import { setTimeout as delay } from 'node:timers/promises';
import monitorOptions from './options.cjs';

const args = process.argv.slice(2);
const outputDir = resolve(args.find(arg => !arg.startsWith('--')) || join(homedir(), '.codex-session-delete', 'renderer-diagnostics'));
const sampleMs = 15000;
const stopRequested = args.includes('--stop');
const once = args.includes('--once');
const { expiresAt, maxBytes, staleMs, statePath: requestedStatePath, expired } = monitorOptions(args);
if (expired && !stopRequested) {
  console.log(JSON.stringify({ status: 'expired', expiresAt }));
  process.exit(0);
}
const run = new Date().toISOString().replace(/[:.]/g, '-');
mkdirSync(outputDir, { recursive: true });
const output = join(outputDir, `observer-${run}.jsonl`);
const mainOutput = join(outputDir, `main-${run}.jsonl`);
const statePath = resolve(requestedStatePath || join(outputDir, 'monitor-state.json'));
let bytes = 0;
let halted = false;
let haltReason = null;
let currentMain;
let mainSocket;
let activeMainOutput = mainOutput;
let eventWindow = Date.now();
let eventCount = 0;
const pages = new Map();
function writeState(status, reason = null) {
  const state = {
    version: 1,
    pid: process.pid,
    status,
    reason,
    expiresAt,
    lastHeartbeatAt: Date.now(),
    observerOutput: output,
    mainOutput: activeMainOutput,
    bytes: existsSync(output) ? statSync(output).size : bytes,
  };
  const temporary = `${statePath}.${process.pid}.tmp`;
  try {
    mkdirSync(resolve(statePath, '..'), { recursive: true });
    writeFileSync(temporary, JSON.stringify(state));
    renameSync(temporary, statePath);
  } catch {
    try { writeFileSync(statePath, JSON.stringify(state)); } catch {}
  }
}
const write = (event, detail = {}) => {
  if (Date.now() - eventWindow >= 60000) { eventWindow = Date.now(); eventCount = 0; }
  if (++eventCount > 240 && !event.includes('crash')) return;
  const line = JSON.stringify({ at: new Date().toISOString(), event, detail }) + '\n';
  if (bytes + Buffer.byteLength(line) > maxBytes) { haltReason = 'size-limit'; halted = true; return; }
  appendFileSync(output, line);
  bytes += Buffer.byteLength(line);
};
async function connect(url, receive = () => {}) {
  const socket = new WebSocket(url);
  const pending = new Map();
  let nextId = 0;
  socket.addEventListener('message', event => {
    const message = JSON.parse(event.data);
    const request = pending.get(message.id);
    if (request) {
      pending.delete(message.id);
      clearTimeout(request.timer);
      if (message.error) request.reject(new Error(`CDP ${message.error.code}`));
      else request.resolve(message.result);
    } else if (message.method) receive(message);
  });
  socket.addEventListener('error', () => {});
  socket.addEventListener('close', () => {
    for (const request of pending.values()) { clearTimeout(request.timer); request.reject(new Error('closed')); }
    pending.clear();
  });
  await new Promise((resolveOpen, rejectOpen) => {
    const timer = setTimeout(() => { socket.close(); rejectOpen(new Error('connect-timeout')); }, 4000);
    socket.addEventListener('open', () => { clearTimeout(timer); resolveOpen(); }, { once: true });
    socket.addEventListener('error', () => { clearTimeout(timer); rejectOpen(new Error('connect-error')); }, { once: true });
  });
  return {
    alive: () => socket.readyState === WebSocket.OPEN,
    close: () => socket.close(),
    call(method, params = {}) {
      return new Promise((resolveCall, rejectCall) => {
        const id = ++nextId;
        const timer = setTimeout(() => { pending.delete(id); rejectCall(new Error('timeout')); }, 4000);
        pending.set(id, { resolve: resolveCall, reject: rejectCall, timer });
        socket.send(JSON.stringify({ id, method, params }));
      });
    },
  };
}
async function targets(port) {
  return (await fetch(`http://127.0.0.1:${port}/json/list`, { signal: AbortSignal.timeout(3000) })).json();
}
const probe = `(() => {const health=window.__codexPlusBridgeHealth||{};return {
  timeOrigin:performance.timeOrigin,navigationType:performance.getEntriesByType('navigation')[0]?.type,
  visibility:document.visibilityState,focused:document.hasFocus(),generation:window.__codexPlusBackendGeneration,
  hasBridge:typeof window.__codexSessionDeleteBridge==='function',lastSuccessAt:health.lastSuccessAt,
  lastInjectionAt:health.lastInjectionAt,lastAttemptAt:health.lastAttemptAt,
  scriptCount:Object.keys(window.__codexPlusUserScripts?.scripts||{}).length,
  initialized:window.__codexPlusUserScripts?.initialized,refreshPending:window.__codexPlusUserScripts?.refreshPending,
  heapLimit:performance.memory?.jsHeapSizeLimit};})()`;
async function sampleMain() {
  const target = (await targets(9329))[0];
  if (!target) return;
  if (!mainSocket?.alive() || currentMain !== target.id) {
    mainSocket?.close();
    mainSocket = await connect(target.webSocketDebuggerUrl);
    currentMain = target.id;
  }
  let result = await mainSocket.call('Runtime.evaluate', { expression: stopRequested ?
    `globalThis.__codexLocalRendererDiagnostics?.stop('manual')` :
    `globalThis.__codexLocalRendererDiagnostics?.status()`, returnByValue: true });
  if (!stopRequested && result.result?.value && result.result.value.expiresAt !== expiresAt) {
    await mainSocket.call('Runtime.evaluate', {
      expression: `globalThis.__codexLocalRendererDiagnostics?.stop('deadline-updated')`, returnByValue: true });
    result = { result: {} };
  }
  if (!stopRequested && result.result?.value === undefined && !result.exceptionDetails) {
    const source = readFileSync(new URL('./main-hook.cjs', import.meta.url), 'utf8').replace('module.exports = installDiagnostics;', '');
    const options = { output: mainOutput, intervalMs: sampleMs, maxBytes, expiresAt };
    result = await mainSocket.call('Runtime.evaluate', { expression:
      `(function(){${source}\nreturn installDiagnostics(${JSON.stringify(options)});})()`, returnByValue: true });
  }
  if (result.exceptionDetails) throw new Error('main-install-failed');
  if (result.result?.value?.stopped) halted = true;
  if (result.result?.value?.output) activeMainOutput = result.result.value.output;
  write('main-status', result.result?.value);
  return result.result?.value;
}
async function samplePages() {
  const found = (await targets(9229)).filter(target => target.type === 'page' && target.url.startsWith('app://-/'));
  for (const [id, page] of pages) if (!found.some(target => target.id === id) || !page.alive()) { page.close(); pages.delete(id); }
  for (const target of found) {
    try {
      if (!pages.has(target.id)) {
        const page = await connect(target.webSocketDebuggerUrl, message => {
          const params = message.params || {};
          if (message.method === 'Inspector.targetCrashed') write('target-crashed', { targetId: target.id });
          if (message.method === 'Page.frameRequestedNavigation') write('renderer-navigation-requested', {
            targetId: target.id, reason: params.reason, disposition: params.disposition });
          if (message.method === 'Page.frameNavigated' && !params.frame?.parentId) write('frame-navigated', { targetId: target.id });
          if (message.method === 'Runtime.exceptionThrown') write('javascript-exception', {
            targetId: target.id, className: params.exceptionDetails?.exception?.className,
            lineNumber: params.exceptionDetails?.lineNumber, columnNumber: params.exceptionDetails?.columnNumber,
            frames: params.exceptionDetails?.stackTrace?.callFrames?.slice(0, 6).map(frame => ({
              scriptId: frame.scriptId, lineNumber: frame.lineNumber, columnNumber: frame.columnNumber })) });
        });
        pages.set(target.id, page);
        await page.call('Page.enable');
        await page.call('Runtime.enable');
        await page.call('Inspector.enable');
        write('page-attached', { targetId: target.id });
      }
      const page = pages.get(target.id);
      const heap = await page.call('Runtime.getHeapUsage');
      const dom = await page.call('Memory.getDOMCounters');
      const result = await page.call('Runtime.evaluate', { expression: probe, returnByValue: true });
      write('renderer-sample', { targetId: target.id, heap, dom, state: result.result?.value });
    } catch (error) {
      write('page-probe-failed', { targetId: target.id, error: error.message });
      pages.get(target.id)?.close();
      pages.delete(target.id);
    }
  }
}
process.on('SIGINT', () => { haltReason = 'signal'; halted = true; });
process.on('SIGTERM', () => { haltReason = 'signal'; halted = true; });
const stopFile = join(outputDir, 'STOP');
write('observer-start', { pid: process.pid, expiresAt, sampleMs });
writeState('starting');
try {
  do {
    if (existsSync(stopFile)) { haltReason = 'stop-file'; halted = true; break; }
    try {
      const status = await sampleMain();
      if (once || stopRequested) console.log(JSON.stringify(status));
    } catch (error) { write('main-probe-failed', { error: error.message }); }
    if (!stopRequested) {
      try { await samplePages(); } catch { write('renderer-port-unavailable'); }
    }
    writeState(halted ? (haltReason || 'halted') : 'running', haltReason);
    if (once || stopRequested || halted) break;
    await delay(sampleMs);
  } while (Date.now() < expiresAt);
} finally {
  if (!once && mainSocket?.alive()) {
    try { await mainSocket.call('Runtime.evaluate', { expression: `globalThis.__codexLocalRendererDiagnostics?.stop('observer-stopped')`, returnByValue: true }); } catch {}
  }
  mainSocket?.close();
  for (const page of pages.values()) page.close();
  write('observer-stopped');
  writeState(Date.now() >= expiresAt ? 'expired' : (haltReason || 'stopped'), haltReason);
}
console.log(JSON.stringify({ output, mainOutput: activeMainOutput, statePath, expiresAt, staleMs,
  bytes: existsSync(output) ? statSync(output).size : 0 }));
