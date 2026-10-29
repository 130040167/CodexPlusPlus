"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { chromium } = require("playwright");

async function main() {
  const browser = await chromium.launch({
    headless: true,
    ...(process.platform === "win32" ? { channel: "msedge" } : {}),
  });
  try {
    const page = await browser.newPage({ viewport: { width: 720, height: 700 } });
    const metadata = {
      "a/sol": { displayName: "GPT-Sol", codex_plus_group: "供应商甲", codex_plus_group_id: "a", codex_plus_default: true },
      "b/sol": { displayName: "GPT-Sol（供应商乙）", codex_plus_group: "供应商乙", codex_plus_group_id: "b" },
      "a/astra": { displayName: "GPT-Astra", codex_plus_group: "供应商甲", codex_plus_group_id: "a" },
      "b/kimi": { displayName: "Kimi（供应商乙）", codex_plus_group: "供应商乙", codex_plus_group_id: "b" },
      "a/variant": { displayName: "GPT-6 (Sol)", codex_plus_group: "供应商甲", codex_plus_group_id: "a" },
    };
    const escape = text => text.replaceAll("&", "&amp;").replaceAll("<", "&lt;");
    const row = (text, subtitle = "") => `<div role="menuitemradio" tabindex="-1"><div data-menu-row-content>${
      subtitle
        ? `<div class="flex-1 flex-col"><span>${escape(text)}</span><span>${subtitle}</span></div>`
        : `<span class="flex-1"><span><span>${escape(text)}</span></span></span>`
    }</div></div>`;
    await page.setContent(`<html lang="zh"><head><style>
      body{font:14px Arial;background:#fff;color:#202020}
      [data-model-picker-view]{width:380px;margin:30px;border:1px solid #ccc;padding:10px}
      [role=menuitemradio]{padding:8px}
      [data-menu-row-content]{display:flex}
      .flex-1{flex:1;min-width:0}.flex-col{display:flex;flex-direction:column}
    </style></head><body><div data-model-picker-view="simple"><div class="_ModelList_test">
      <div data-menu-section-label>选择模型</div>
      ${row("默认", "推荐默认集")}
      ${Object.values(metadata).map(entry => row(entry.displayName)).join("")}
    </div></div></body></html>`);
    const shard = fs.readFileSync(path.join(__dirname, "../../assets/inject/renderer-inject/71-model-picker-groups.js"), "utf8");
    await page.addScriptTag({ content: `(() => {
      const setTimeout = () => 0;
      const registerCodexPlusExtensionSelector = () => {};
      let codexModelCatalog = ${JSON.stringify({
        models: Object.keys(metadata), modelMetadata: metadata, codex_plus_default_group_id: "a",
      })};
      const codexPlusConfiguredDefaultModel = () => "a/sol";
      const codexPlusModelMetadata = model => codexModelCatalog.modelMetadata[model];
      ${shard}
      window.testPicker = {
        patch: () => codexPlusModelPickerPatch(document.querySelector("._ModelList_test")),
        clear: () => codexPlusModelPickerClear(document.querySelector("._ModelList_test")),
        refresh: catalog => { codexModelCatalog = catalog; codexPlusModelPickerCatalogChanged(); },
      };
    })();` });
    await page.evaluate(() => window.testPicker.patch());
    const picker = page.locator("[data-model-picker-view]");
    assert.equal((await picker.innerText()).match(/选择模型/g)?.length, 1);
    assert(!/推荐默认集|推荐模型集/.test(await picker.innerText()));
    assert.equal(await picker.locator(".codex-plus-model-default-value").innerText(), "GPT-Sol");
    assert.deepEqual(await picker.locator(".codex-plus-model-group-title").allTextContents(), ["供应商甲", "供应商乙"]);
    await picker.getByRole("button", { name: "全部折叠", exact: true }).click();
    assert.equal(await picker.locator("[data-codex-plus-model-group]:visible").count(), 0);
    await picker.getByRole("button", { name: "全部展开", exact: true }).click();
    const layout = await picker.locator(".codex-plus-model-group-header, [data-codex-plus-model-group]").evaluateAll(nodes =>
      nodes.map(node => ({ group: node.getAttribute("data-codex-plus-model-group") || node.getAttribute("data-codex-plus-model-group-id"), y: node.getBoundingClientRect().y }))
        .sort((a, b) => a.y - b.y).map(node => node.group));
    assert.deepEqual(layout, ["a", "a", "a", "a", "b", "b", "b"]);
    assert((await picker.innerText()).includes("GPT-6 (Sol)"));
    await picker.locator(".codex-plus-model-group-header").first().focus();
    await page.keyboard.press("ArrowDown");
    assert.equal(await page.evaluate(() => document.activeElement.innerText), "GPT-Sol");
    await page.keyboard.press("ArrowDown");
    assert.equal(await page.evaluate(() => document.activeElement.innerText), "GPT-Astra");
    await page.keyboard.press("ArrowDown");
    assert.equal(await page.evaluate(() => document.activeElement.innerText), "GPT-6 (Sol)");
    const toolbar = await picker.locator(".codex-plus-model-group-toolbar").boundingBox();
    assert(toolbar && toolbar.height < 40);
    await page.evaluate(() => window.testPicker.patch());
    assert.equal(await picker.locator(".codex-plus-model-group-header").count(), 2);
    const output = path.join(__dirname, "../../output/model-picker-dom");
    fs.mkdirSync(output, { recursive: true });
    await page.screenshot({ path: path.join(output, "grouped.png") });
    // 目录删除旧模型后不从旧 metadata 或持久缓存恢复。
    const next = { models: ["a/sol"], modelMetadata: metadata, codex_plus_default_group_id: "a" };
    await page.evaluate(catalog => {
      window.testPicker.refresh(catalog);
      const list = document.querySelector("._ModelList_test");
      [...list.querySelectorAll('[role="menuitemradio"]')].slice(2).forEach(node => node.remove());
      window.testPicker.patch();
    }, next);
    assert.equal(await picker.locator(".codex-plus-model-group-header").count(), 0);
    assert(!/推荐默认集/.test(await picker.innerText()));
    await page.evaluate(() => window.testPicker.clear());
    assert((await picker.innerText()).includes("推荐默认集"));
    assert.equal(await picker.locator("[data-codex-plus-model-ordering]").count(), 0);
    console.log("PASS: DOM layout, provider identity, default subtitle, collapse/expand, keyboard order, catalog refresh, cleanup");
  } finally {
    await browser.close();
  }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
