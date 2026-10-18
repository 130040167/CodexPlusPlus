const assert = require("node:assert/strict");
const fs = require("node:fs");
const http = require("node:http");
const path = require("node:path");
const { chromium } = require("playwright");

const source = fs.readFileSync(path.join(__dirname, "../assets/inject/renderer-inject/70-model-catalog.js"), "utf8");
const functions = source.slice(source.indexOf("  function codexRememberedThreadModels()"),
  source.indexOf("  function codexPerModelContextEnabled()"));
const html = `<!doctype html><meta charset="utf-8"><title>Thread effort regression fixture</title>
<button data-codex-intelligence-trigger>Model</button>
<button data-reasoning-slider>Power</button><output id="effort"></output>
<script>
const codexPlusBackendSettingsLoaded = true;
const codexRemoteSessionActiveProfile = () => ({ id: "fixture" });
const codexModelCatalog = { status: "ok", codex_plus_default_group_id: "a" };
const codexPlusModelNames = () => ["sol"];
const codexPlusModelMetadata = () => ({ codex_plus_group_id: "a" });
${functions}
const owner = { conversationId: location.pathname, permissionsHostId: "local" };
const picker = {
  model: "sol", reasoningEffort: "low", models: [{ model: "sol" }],
  onSelectModel(model, effort) {
    this.model = model; this.reasoningEffort = effort; render();
  }
};
const fiber = { memoizedProps: picker, return: { memoizedProps: owner } };
for (const node of document.querySelectorAll("button")) node.__reactFiber$fixture = fiber;
function render() { document.querySelector("output").textContent = picker.reasoningEffort; }
const slider = document.querySelector("[data-reasoning-slider]");
slider.addEventListener("pointerup", () => picker.onSelectModel("sol", "high"));
slider.addEventListener("keydown", event => {
  if (event.key === "ArrowRight") picker.onSelectModel("sol", "xhigh");
});
render();
syncCodexRetainedThreadModels();
</script>`;

(async () => {
  const server = http.createServer((_req, res) => {
    res.setHeader("Content-Type", "text/html; charset=utf-8");
    res.end(html);
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  let browser;
  try {
    const origin = `http://127.0.0.1:${server.address().port}`;
    browser = await chromium.launch({ channel: "msedge", headless: true });
    let context = await browser.newContext();
    let page = await context.newPage();
    const errors = [];
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(origin + "/one");
    await page.locator("[data-reasoning-slider]").click();
    await page.waitForFunction(() => JSON.parse(localStorage.getItem("codexPlus.threadModels.v1") || "[]")[0]?.effort === "high");
    await page.reload();
    await page.waitForFunction(() => document.querySelector("output").textContent === "high");
    await page.locator("[data-reasoning-slider]").press("ArrowRight");
    await page.waitForFunction(() => JSON.parse(localStorage.getItem("codexPlus.threadModels.v1"))[0].effort === "xhigh");
    await page.goto(origin + "/two");
    assert.equal(await page.locator("output").textContent(), "low");
    await page.goto(origin + "/one");
    await page.waitForFunction(() => document.querySelector("output").textContent === "xhigh");
    const storageState = await context.storageState();
    await browser.close();
    browser = await chromium.launch({ channel: "msedge", headless: true });
    context = await browser.newContext({ storageState });
    page = await context.newPage();
    page.on("pageerror", error => errors.push(error.message));
    await page.goto(origin + "/one");
    await page.waitForFunction(() => document.querySelector("output").textContent === "xhigh");
    assert.deepEqual(errors, []);
    console.log("PASS: native-shaped slider pointer/keyboard events, reload, thread navigation, browser recreation with serialized storage");
  } finally {
    await browser?.close();
    await new Promise(resolve => server.close(resolve));
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
