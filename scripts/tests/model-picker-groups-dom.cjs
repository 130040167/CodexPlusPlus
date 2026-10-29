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
        status: "ok", models: Object.keys(metadata), modelMetadata: metadata, codex_plus_default_group_id: "a",
      })};
      const codexPlusConfiguredDefaultModel = () => "a/sol";
      const codexPlusModelMetadata = model => codexModelCatalog.modelMetadata?.[model];
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
    const next = { status: "ok", models: ["a/sol"], modelMetadata: metadata, codex_plus_default_group_id: "a" };
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
    const gateway = {
      "a/sol": { displayName: "GPT-6.1-Sol\u200b", codex_plus_group: "供应商甲", codex_plus_group_id: "a" },
      "b/sol": { displayName: "GPT-6.1-Sol\u200b\u200b", codex_plus_group: "供应商乙", codex_plus_group_id: "b" },
      sol: { displayName: "GPT-6.1-Sol\u200c", codex_plus_group: "自动组", codex_plus_group_id: "codex-plus-gateway-auto" },
      "group/coding": { displayName: "编程模型组", codex_plus_group: "模型组", codex_plus_group_id: "codex-plus-gateway-groups", codex_plus_favorite: true },
    };
    await page.evaluate(({ metadata, html }) => {
      document.querySelector("._ModelList_test").innerHTML = html;
      window.testPicker.refresh({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
      window.testPicker.patch();
    }, { metadata: gateway, html: `<div data-menu-section-label>选择模型</div>${row("默认", "推荐模型集")}${Object.values(gateway).map(entry =>
      row(entry.displayName.replace(/^GPT-/u, "").replace(/-/gu, " "))).join("")}` });
    assert.deepEqual(await picker.locator(".codex-plus-model-group-title").allTextContents(), ["供应商甲", "供应商乙", "自动组", "模型组"]);
    const membership = await picker.locator("[data-codex-plus-model-group]").evaluateAll(rows =>
      rows.map(row => row.getAttribute("data-codex-plus-model-group")));
    assert.deepEqual(membership, ["a", "b", "codex-plus-gateway-auto", "codex-plus-gateway-groups"]);
    assert.equal(await picker.locator(".codex-plus-model-group-favorite").count(), 1);
    assert.equal((await picker.innerText()).match(/选择模型/g)?.length, 1);
    assert(!/推荐模型集|推荐默认集/.test(await picker.innerText()));
    await page.screenshot({ path: path.join(output, "gateway-groups.png") });
    // 原生去掉零宽后缀或把名称缩写时，使用行的真实模型身份，而非第一个同名供应商。
    const sameNames = Object.fromEntries(Object.entries(gateway).map(([slug, entry]) =>
      [slug, slug === "group/coding" ? entry : { ...entry, displayName: "GPT-Sol" }]));
    await page.evaluate(({ metadata, html }) => {
      document.querySelector("._ModelList_test").innerHTML = html;
      const slugs = Object.keys(metadata);
      [...document.querySelectorAll('._ModelList_test [role="menuitemradio"]')].slice(1).forEach((row, n) => {
        row.__reactFiber$test = { key: slugs[n], memoizedProps: {}, return: {
          memoizedProps: { model: "a/sol", models: slugs.map(model => ({ model })), onSelectModel() {} },
        } };
      });
      window.testPicker.refresh({ status: "ok", models: slugs, modelMetadata: metadata });
      window.__codexPlusModelPickerGroupsRuntime.scan();
    }, { metadata: sameNames, html: `<div data-menu-section-label>选择模型</div>${row("默认", "推荐模型集")}${Object.values(sameNames).map(entry => row(entry.displayName)).join("")}` });
    assert.deepEqual(await picker.locator("[data-codex-plus-model-group]").evaluateAll(rows =>
      rows.map(row => row.getAttribute("data-codex-plus-model-group"))),
    ["a", "b", "codex-plus-gateway-auto", "codex-plus-gateway-groups"]);
    // 目录尚未就绪时不拆除成功补丁；明确删成单组则仍可正确恢复。
    await page.evaluate(() => {
      window.testPicker.refresh({ status: "loading", models: [] });
      window.testPicker.patch();
    });
    assert.equal(await picker.locator(".codex-plus-model-group-header").count(), 4);
    await page.evaluate(metadata => {
      window.testPicker.refresh({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
      window.testPicker.patch();
    }, sameNames);
    // 模拟 React 回收注入的标题。逐帧采样，任何一帧都不得显示无分组平铺列表。
    const frames = await page.evaluate(async () => {
      document.querySelectorAll(".codex-plus-model-group-header, .codex-plus-model-group-toolbar").forEach(node => node.remove());
      const frames = [];
      for (let n = 0; n < 5; n++) {
        await new Promise(resolve => requestAnimationFrame(resolve));
        frames.push(document.querySelectorAll(".codex-plus-model-group-header").length);
      }
      return frames;
    });
    assert.deepEqual(frames, [4, 4, 4, 4, 4]);
    const ambiguous = await page.evaluate(() => {
      const rows = [...document.querySelectorAll('._ModelList_test [role="menuitemradio"]')].slice(1);
      rows.forEach(row => { delete row.__reactFiber$test; });
      return rows.slice(0, 3).map(row => window.__codexPlusModelPickerGroupsRuntime.describeRow(row).entry);
    });
    assert.deepEqual(ambiguous, [null, null, null], "ambiguous names must not be assigned to the first supplier");
    await page.evaluate(metadata => {
      window.testPicker.refresh({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
    }, gateway);
    // 弹层整体重建也应在首次绘制之前完成分组。
    assert.equal(await page.evaluate(async () => {
      const old = document.querySelector("[data-model-picker-view]");
      const fresh = old.cloneNode(true);
      fresh.querySelectorAll(".codex-plus-model-group-header, .codex-plus-model-group-toolbar").forEach(node => node.remove());
      // clone 不复制 React fiber；用唯一完整名重新渲染真实行。
      const labels = ["6.1 Sol\u200b", "6.1 Sol\u200b\u200b", "6.1 Sol\u200c", "编程模型组"];
      [...fresh.querySelectorAll('[role="menuitemradio"]')].slice(1).forEach((row, n) => {
        (row.querySelector(".codex-plus-model-group-native-label")
          || row.querySelector("[data-menu-row-content] .flex-1 span span")).textContent = labels[n];
      });
      old.replaceWith(fresh);
      return await new Promise(resolve => requestAnimationFrame(() =>
        resolve(fresh.querySelectorAll(".codex-plus-model-group-header").length)));
    }), 4);
    await page.evaluate(() => {
      const list = document.querySelector("._ModelList_test");
      [...list.querySelectorAll('[role="menuitemradio"]')].slice(2).forEach(node => node.remove());
      window.testPicker.patch();
    });
    assert.equal(await picker.locator(".codex-plus-model-group-header").count(), 1,
      "partial native redraws must preserve a group header while the catalog still has several groups");
    console.log("PASS: grouped layout, exact model IDs, duplicate names, refresh/loading continuity, React redraw frame checks, favorites and native cleanup");
  } finally {
    await browser.close();
  }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
