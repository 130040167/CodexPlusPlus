function installDiagnostics(options) {
  const key = '__codexLocalRendererDiagnostics';
  if (globalThis[key]) return globalThis[key].status();
  const electron = process.mainModule.require('electron');
  const fs = process.mainModule.require('node:fs');
  const path = process.mainModule.require('node:path');
  const os = process.mainModule.require('node:os');
  fs.mkdirSync(path.dirname(options.output), { recursive: true });
  const cleanup = [];
  const attached = new Map();
  let stopped = false;
  let lastMetrics = [];
  let writes = 0;
  let writeFailures = 0;
  let bytes = fs.existsSync(options.output) ? fs.statSync(options.output).size : 0;
  let windowStart = Date.now();
  let windowEvents = 0;
  let dropped = 0;
  function write(event, detail) {
    if (stopped) return;
    if (Date.now() - windowStart > 60000) {
      windowStart = Date.now();
      windowEvents = 0;
    }
    if (++windowEvents > 240 && !event.includes('gone') && event !== 'stopped') {
      dropped++;
      return;
    }
    try {
      const line = JSON.stringify({ at: new Date().toISOString(), pid: process.pid, event, detail }) + '\n';
      if (bytes + Buffer.byteLength(line) > options.maxBytes) {
        stop('size-limit');
        return;
      }
      fs.appendFileSync(options.output, line, 'utf8');
      bytes += Buffer.byteLength(line);
      writes++;
    } catch {
      writeFailures++;
    }
  }
  function identify(contents) {
    try {
      const url = contents.getURL();
      return { id: contents.id, type: contents.getType(), osPid: contents.getOSProcessId(),
        surface: url.startsWith('app://-/') ? 'codex' : 'other' };
    } catch {
      return { id: contents.id, destroyed: true };
    }
  }
  function listen(emitter, event, listener, listeners = cleanup) {
    emitter.prependListener(event, listener);
    listeners.push(() => emitter.removeListener(event, listener));
  }
  function sample() {
    try {
      lastMetrics = electron.app.getAppMetrics().map(metric => ({ pid: metric.pid, type: metric.type,
        cpuPercent: metric.cpu.percentCPUUsage, cpuSeconds: metric.cpu.cumulativeCPUUsage,
        workingSetKB: metric.memory.workingSetSize, peakWorkingSetKB: metric.memory.peakWorkingSetSize,
        privateKB: metric.memory.privateBytes }));
      write('process-metrics', { processes: lastMetrics, freePhysicalBytes: os.freemem(),
        totalPhysicalBytes: os.totalmem(), contents: electron.webContents.getAllWebContents().map(identify),
        systemMemoryKB: process.getSystemMemoryInfo?.(), dropped, writeFailures });
    } catch {
      write('metrics-unavailable', {});
    }
  }
  function attach(contents) {
    if (attached.has(contents.id)) return;
    const listeners = [];
    attached.set(contents.id, listeners);
    const record = (event, detail = {}) => write(event, { contents: identify(contents), ...detail });
    listen(contents, 'render-process-gone', (_event, detail) => {
      record('render-process-gone', { reason: detail.reason, exitCode: detail.exitCode, lastMetrics });
    }, listeners);
    for (const event of ['did-start-loading', 'did-stop-loading', 'dom-ready', 'unresponsive', 'responsive']) {
      listen(contents, event, () => record(event), listeners);
    }
    listen(contents, 'did-start-navigation', (_event, _url, inPlace, mainFrame, processId, routingId) => {
      if (mainFrame) record('navigation-start', { inPlace, processId, routingId });
    }, listeners);
    listen(contents, 'did-fail-load', (_event, errorCode, _description, _url, mainFrame) => {
      if (mainFrame) record('load-failed', { errorCode });
    }, listeners);
    for (const method of ['reload', 'reloadIgnoringCache', 'loadURL', 'loadFile']) {
      const original = contents[method];
      if (typeof original !== 'function') continue;
      const wrapped = function (...args) {
        const frames = (new Error().stack || '').split('\n').slice(2, 8).map(frame =>
          frame.replace(/(?:[A-Za-z]:)?[^\s()]*[\\/]/g, '').replace(/[?#][^\s)]*/g, '').slice(0,180));
        record('main-navigation-call', { method, frames });
        return Reflect.apply(original, this, args);
      };
      contents[method] = wrapped;
      listeners.push(() => { if (contents[method] === wrapped) contents[method] = original; });
    }
    listen(contents, 'destroyed', () => {
      record('contents-destroyed');
      for (const remove of listeners) { try { remove(); } catch {} }
      attached.delete(contents.id);
    }, listeners);
    record('contents-attached');
  }
  function status() {
    return { version: 1, pid: process.pid, output: options.output, writes, bytes, dropped, writeFailures,
      stopped, contents: attached.size, expiresAt: options.expiresAt };
  }
  function stop(reason = 'manual') {
    if (stopped) return status();
    if (bytes < options.maxBytes - 1024) write('stopped', { reason });
    stopped = true;
    for (const remove of cleanup) { try { remove(); } catch {} }
    for (const listeners of attached.values()) for (const remove of listeners) { try { remove(); } catch {} }
    attached.clear();
    delete globalThis[key];
    return status();
  }
  globalThis[key] = { stop, status };
  listen(electron.app, 'web-contents-created', (_event, contents) => attach(contents));
  listen(electron.app, 'child-process-gone', (_event, detail) => {
    write('child-process-gone', { type: detail.type, reason: detail.reason, exitCode: detail.exitCode });
  });
  for (const contents of electron.webContents.getAllWebContents()) {
    if (stopped) break;
    attach(contents);
  }
  if (stopped) return status();
  const timer = setInterval(() => {
    if (Date.now() >= options.expiresAt) stop('expired');
    else sample();
  }, options.intervalMs);
  timer.unref?.();
  cleanup.push(() => clearInterval(timer));
  write('installed', { version: 1, intervalMs: options.intervalMs, expiresAt: options.expiresAt });
  sample();
  return status();
}
module.exports = installDiagnostics;
