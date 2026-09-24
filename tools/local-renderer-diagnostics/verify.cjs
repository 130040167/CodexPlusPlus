const assert = require('node:assert/strict');
const { EventEmitter } = require('node:events');
const { readFileSync } = require('node:fs');
const vm = require('node:vm');
const source = readFileSync(require.resolve('./main-hook.cjs'), 'utf8');
function fixture(maxBytes = 65536) {
  const lines = [];
  const app = new EventEmitter();
  const contents = new EventEmitter();
  let reloadCount = 0;
  let cleared = 0;
  let createdTimers = 0;
  contents.id = 1;
  contents.getURL = () => 'app://-/index.html?secret=do-not-record';
  contents.getType = () => 'window';
  contents.getOSProcessId = () => 42;
  contents.reload = () => ++reloadCount;
  app.getAppMetrics = () => [{ pid: 42, type: 'Tab', cpu: { percentCPUUsage: 2, cumulativeCPUUsage: 10 },
    memory: { privateBytes: 2048, workingSetSize: 1024, peakWorkingSetSize: 4096 } }];
  const electron = { app, webContents: { getAllWebContents: () => [contents] } };
  const modules = { electron, 'node:fs': { mkdirSync() {}, existsSync: () => false,
    appendFileSync: (_file, line) => lines.push(JSON.parse(line)) },
    'node:path': require('node:path'), 'node:os': { freemem: () => 100, totalmem: () => 1000 } };
  const context = vm.createContext({ process: { pid: 7, mainModule: { require: name => modules[name] } },
    module: { exports: {} }, Buffer, setInterval: () => { createdTimers++; return { unref() {} }; }, clearInterval: () => cleared++ });
  vm.runInContext(source, context);
  const options = { output: 'test.jsonl', maxBytes, intervalMs: 15000, expiresAt: Date.now() + 60000 };
  return { context, options, lines, app, contents, reloadCount: () => reloadCount, cleared: () => cleared,
    activeTimers: () => createdTimers - cleared, install: () => context.module.exports(options) };
}
const test = fixture();
const originalReload = test.contents.reload;
test.install();
test.install();
assert.equal(test.contents.listenerCount('render-process-gone'), 1);
assert.equal(test.app.listenerCount('web-contents-created'), 1);
assert.equal(test.reloadCount(), 0);
test.contents.on('render-process-gone', () => test.contents.reload());
test.contents.emit('render-process-gone', {}, { reason: 'oom', exitCode: -536870904 });
const gone = test.lines.findIndex(line => line.event === 'render-process-gone');
const reload = test.lines.findIndex(line => line.event === 'main-navigation-call');
assert.ok(gone >= 0 && reload > gone);
assert.equal(test.lines[gone].detail.reason, 'oom');
assert.equal(test.lines[gone].detail.lastMetrics[0].privateKB, 2048);
assert.equal(test.reloadCount(), 1);
assert.ok(!JSON.stringify(test.lines).includes('do-not-record'));
test.context.__codexLocalRendererDiagnostics.stop('test');
assert.equal(test.contents.reload, originalReload);
assert.equal(test.app.listenerCount('web-contents-created'), 0);
assert.equal(test.contents.listenerCount('render-process-gone'), 1);
assert.equal(test.cleared(), 1);
assert.equal(test.context.__codexLocalRendererDiagnostics, undefined);
const bounded = fixture(100);
bounded.install();
assert.equal(bounded.context.__codexLocalRendererDiagnostics, undefined);
assert.equal(bounded.activeTimers(), 0);
assert.equal(bounded.app.listenerCount('web-contents-created'), 0);
assert.ok(bounded.lines.reduce((sum, line) => sum + Buffer.byteLength(JSON.stringify(line) + '\n'), 0) <= 100);
console.log('diagnostics: idempotence, crash-before-reload, metrics, redaction, cleanup and size cap passed');
const monitorOptions = require('./options.cjs');
const started = Date.parse('2026-09-23T06:30:00Z');
const week = monitorOptions(['--days=7'], started);
assert.equal(week.expiresAt, Date.parse('2026-09-30T06:30:00Z'));
assert.equal(week.expired, false);
assert.equal(week.maxBytes, 128 * 1024 * 1024);
assert.equal(week.staleMs, 90000);
assert.equal(week.statePath, null);
assert.equal(monitorOptions(['--state=C:/logs/monitor-state.json'], started).statePath,
  'C:/logs/monitor-state.json');
const deadline = '--until=2026-09-30T06:30:00Z';
assert.equal(monitorOptions([deadline], started + 86400000).expiresAt, week.expiresAt);
assert.equal(monitorOptions([deadline], week.expiresAt).expired, true);
assert.throws(() => monitorOptions(['--days=8'], started));
assert.throws(() => monitorOptions(['--until=invalid'], started));
console.log('diagnostics: seven-day deadline, fixed-deadline restart, expiry and input validation passed');
