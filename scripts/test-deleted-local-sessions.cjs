const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const source = fs.readFileSync(path.join(__dirname, "../assets/inject/renderer-inject/80-session-share.js"), "utf8");
const code = source.slice(0, source.indexOf("  function normalizeWorkspacePath("));
const rows = [];
const styles = new Map();
function row(id, host) {
  const attributes = new Map();
  return { id, host, attributes,
    setAttribute(k, v) { attributes.set(k, v); },
    removeAttribute(k) { attributes.delete(k); },
  };
}
let response = { status: "ok", thread_ids: ["a"] };
const context = vm.createContext({
  window: {},
  document: {
    getElementById(id) { return styles.get(id); },
    createElement() { return { setAttribute() {} }; },
    head: { appendChild(node) { styles.set(node.id, node); } },
  },
  sessionRows() { return rows; },
  sessionRefFromRow(r) { return { session_id: r.id, host_id: r.host }; },
  async postJson() { return response; },
});
vm.runInContext(`${code}\nthis.api = { applyDeletedLocalSessions, refreshDeletedLocalSessions, state: deletedLocalSessions };`, context);
async function run() {
  const { api } = context;
  rows.push(row("a", "local"), row("a", "remote"), row("b", "local"), row("a", null));
  await api.refreshDeletedLocalSessions();
  assert.equal(rows[0].attributes.get("data-codex-plus-deleted-local"), "true");
  for (const r of rows.slice(1)) assert.equal(r.attributes.size, 0);
  // React replaces a row from its cached conversation list.
  rows[0] = row("a", "local");
  api.applyDeletedLocalSessions();
  assert.equal(rows[0].attributes.size, 1);
  rows[0].host = "remote";
  api.applyDeletedLocalSessions();
  assert.equal(rows[0].attributes.size, 0);
  rows[0].host = "local";
  api.applyDeletedLocalSessions();
  assert.equal(rows[0].attributes.size, 1);
  // Reused nodes must regain visibility after their identity changes.
  rows[0].id = "b";
  api.applyDeletedLocalSessions();
  assert.equal(rows[0].attributes.size, 0);
  rows[0].id = "a";
  // Reinjection retains the known deletion set.
  vm.runInContext(`(() => {${code}\napplyDeletedLocalSessions();})();`, context);
  assert.equal(rows[0].attributes.size, 1);
  assert.equal(styles.size, 1);
  // A stale response cannot re-hide a restored session.
  const pending = api.refreshDeletedLocalSessions();
  api.state.revision++;
  api.state.ids.delete("a");
  await pending;
  api.applyDeletedLocalSessions();
  assert.equal(rows[0].attributes.size, 0);
  response = { status: "ok", thread_ids: [] };
  await api.refreshDeletedLocalSessions();
  assert.equal(api.state.ids.size, 0);
  console.log("deleted local sessions: replacement, reuse, host isolation, reinjection, undo race passed");
}
run().catch((error) => { console.error(error); process.exitCode = 1; });
