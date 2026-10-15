import assert from "node:assert/strict";
import { it } from "node:test";
import { readFile } from "node:fs/promises";

/**
 * 模型选择器分组折叠（聚合供应商场景）的契约测试。
 *
 * 这层跑不了真 DOM（仓库的注入层测试都是"读取分片源码 + 断言结构"这种风格，
 * 浏览器行为由 renderer-inject.test.ts 里的静态契约 + 人工/CDP 实测覆盖）。
 * 所以这里钉死三件事：
 *   1. 与后端约定的字段名不能漂（`codex_plus_group` / `codex_plus_group_id`）；
 *   2. 影响别人的行为约束：不搬动 React 管理的行、隐藏原生标签要退出无障碍树、
 *      单供应商不接管、聚合目录里"默认成员"那一组要能看懂；
 *   3. 自己的扫描/清理路径不能漏（否则会闪、会自喂扫描循环）。
 */

const SHARD_URL = new URL(
  "../../../assets/inject/renderer-inject/71-model-picker-groups.js",
  import.meta.url,
);
const MANIFEST_URL = new URL(
  "../../../assets/inject/renderer-inject/manifest.json",
  import.meta.url,
);

async function readShard() {
  return await readFile(SHARD_URL, "utf8");
}

it("分组折叠分片在 manifest 里排在模型目录之后、并在组装产物里", async () => {
  const manifest = JSON.parse(await readFile(MANIFEST_URL, "utf8")) as {
    fragments: Array<{ name: string }>;
  };
  const names = manifest.fragments.map((fragment) => fragment.name);
  const index = names.indexOf("71-model-picker-groups.js");
  assert.ok(index > 0, "分片必须登记进 manifest.json");
  assert.ok(
    names.indexOf("70-model-catalog.js") < index,
    "要排在 70-model-catalog.js 之后：它依赖 codexModelCatalog 快照",
  );
  assert.ok(
    names.indexOf("99-tail.js") > index,
    "必须排在 IIFE 收尾之前，否则代码不在主闭包里",
  );

  const artifact = await readFile(
    new URL("../../../assets/inject/renderer-inject.js", import.meta.url),
    "utf8",
  );
  assert.match(artifact, /codexPlusModelPickerGroupsRuntime/, "组装产物里要有本分片");
});

it("分组折叠读的字段名与后端发布一致", async () => {
  const source = await readShard();
  // 后端 aggregate_catalog.rs 发布 / model_suffix.rs 放行的两个键。
  assert.match(source, /entry\.codex_plus_group\b/);
  assert.match(source, /entry\.codex_plus_group_id\b/);
});

it("不搬动 React 管理的行，只用类名隐藏", async () => {
  const source = await readShard();
  // 行必须留在原位：只允许插标题/工具条、加类名和属性。
  assert.doesNotMatch(source, /appendChild\(entry\.row\)|appendChild\(row\)/);
  assert.doesNotMatch(source, /\.row\.remove\(\)/);
  assert.match(source, /codex-plus-model-group-collapsed\{display:none!important\}/);
  // 折叠状态必须落在行自己的类名上，而不是重排 DOM。
  assert.match(source, /classList\.toggle\("codex-plus-model-group-collapsed"/);
});

it("隐藏原生标签时同步退出无障碍树，避免读屏念两遍", async () => {
  const source = await readShard();
  assert.match(source, /native\.setAttribute\("aria-hidden", "true"\)/);
  assert.match(source, /native\.removeAttribute\("aria-hidden"\)/);
});

it("只有一个供应商时不接管原生列表", async () => {
  const source = await readShard();
  assert.match(source, /if \(groups\.length < 2\) \{/);
  // 单组时要能退回原始状态。
  assert.match(source, /codexPlusModelPickerClear\(list\)/);
  assert.match(source, /list\.removeAttribute\("data-codex-plus-model-groups"\)/);
});

it("默认成员的裸名模型归到默认供应商分组，不产生空标题", async () => {
  const source = await readShard();
  // 分组名解析：优先 codex_plus_group，其次 description 的 "上游 · 供应商"。
  assert.match(source, /const separator = description\.lastIndexOf\(" · "\)/);
  assert.match(source, /codexPlusModelPickerDefaultProviderName/);
  // 空名兜底成固定 id，避免折叠状态串组。
  assert.match(source, /group\.id = "__ungrouped__"/);
});

it("扫描器自己插的节点都带归属标记，避免自喂扫描循环", async () => {
  const source = await readShard();
  assert.match(
    source,
    /registerCodexPlusExtensionSelector\('\[data-codex-plus-ext="builtin-model-picker-groups"\]'\)/,
    "必须登记扩展选择器，否则扫描调度器会把我们插的标题当成 Codex 原生节点",
  );
  const marks = source.match(/data-codex-plus-ext", codexPlusModelPickerGroupsExtMark/g) ?? [];
  assert.ok(marks.length >= 4, "标题、工具条、行、标签都要打标记");
});

it("菜单关闭或组件重建后能自愈，不留下清不掉的残留", async () => {
  const source = await readShard();
  // React 回收兄弟节点后标题会消失，指纹没变也必须重建。
  assert.match(source, /codexPlusModelPickerHeadersIntact/);
  assert.match(source, /header\.nextElementSibling === first/);
  // 恢复时要摘干净自己加的类名与属性。
  assert.match(source, /row\.removeAttribute\(codexPlusModelPickerGroupAttr\)/);
  assert.match(source, /native\.removeAttribute\("data-codex-plus-ext"\)/);
});

it("重注入时与上一个实例交接，不先拆再建造成闪烁", async () => {
  const source = await readShard();
  assert.match(source, /window\.__codexPlusModelPickerGroupsRuntime\?\.dispose\?\.\(\)/);
  const dispose = source.match(/dispose\(\) \{[\s\S]*?\n    \},/)?.[0];
  assert.ok(dispose, "要有 dispose 实现");
  assert.doesNotMatch(dispose, /codexPlusModelPickerClear/, "dispose 不能顺手拆 DOM：重注入会闪");
  assert.match(source, /releaseDom\(\)/, "彻底还原要有单独出口");
});

it("折叠状态按分组 id 持久化，并且有中文文案", async () => {
  const source = await readShard();
  assert.match(source, /codexPlus\.modelPicker\.collapsedGroups\.v1/);
  assert.match(source, /全部展开/);
  assert.match(source, /全部折叠/);
  // 读屏文案要带展开/折叠状态。
  assert.match(source, /groupAria/);
});

it("默认供应商带「默认」标志，身份来自目录而不是猜名字", async () => {
  const source = await readShard();
  // 顶层 id 优先，其次目录条目的 default 标记，最后才是会话内记住的值。
  assert.match(source, /codex_plus_default_group_id/);
  assert.match(source, /codex_plus_default\b/);
  assert.match(source, /defaultGroupId/);
  // 标志要能随扫描补齐/摘掉，而不是建标题时定一次。
  assert.match(source, /codexPlusModelPickerSyncDefaultBadge/);
  assert.match(source, /codex-plus-model-group-default/);
  assert.match(source, /defaultBadge: "默认"/);
  // 默认分组在 id 去重改写之前先认出来，且按组内任意一行命中判定。
  assert.match(source, /group\.isDefault = !!defaultGroupId/);
  // 没有选中模型时优先展开默认供应商。
  assert.match(source, /keep = groups\.find\(\(group\) => group\.isDefault\)\?\.id/);
});

it("目录条目按别名匹配，供应商后缀优先定位分组 id", async () => {
  const source = await readShard();
  // 目录的展示名（GPT-6.1-Sol）与选择器里的行（6.1 Sol）不一样，必须登记别名。
  assert.match(source, /codexPlusModelPickerCatalogAliases/);
  assert.match(source, /replace\(\/\^gpt\[-\\s\]\*\/iu, ""\)/);
  // 早期版本的教训：拿目录展示名去拟合行文本会把同一家切成好几段。
  assert.match(source, /trustedProvider \? matchingProvider\?\.groupId : known\?\.groupId/);
  assert.match(source, /const provider = trustedProvider \|\| known\?\.provider/);
  // 组内任意一行命中默认分组即认定该组是默认（裸名行的 id 可能来自目录）。
  assert.match(source, /group\.entries\.some\(\(entry\) => entry\.groupId === defaultGroupId/);
});

it("只把目录确认过的后缀当作供应商，避免把 Sol 等模型名误建成分组", async () => {
  const source = await readShard();
  assert.match(source, /codexPlusModelPickerCatalogProviderNames/);
  assert.match(source, /trustedProvider/);
  assert.match(source, /providerNames\.has/);
});

it("Sol 是模型变体；两家供应商的数量和默认标签不被切碎", async () => {
  const source = await readShard();
  const functions = [
    "codexPlusModelPickerSplitLabel",
    "codexPlusModelPickerNormalizeStrict",
    "codexPlusModelPickerNormalize",
    "codexPlusModelPickerRows",
    "codexPlusModelPickerFilteredCatalog",
    "codexPlusModelPickerGroups",
    "codexPlusModelPickerUniqueGroupIds",
  ].map((name) => {
    const start = source.indexOf(`  function ${name}(`);
    assert.ok(start >= 0);
    const end = source.indexOf("\n  }\n", start);
    return source.slice(start, end + 5);
  }).join("\n");
  const metadata = new Map([
    ["6.1", { provider: "寒鹤", groupId: "hanhe" }],
    ["6.1(sol)", { provider: "寒鹤", groupId: "hanhe" }],
    ["6.1（浮生）", { provider: "浮生", groupId: "fusheng" }],
  ]);
  const run = new Function(
    "codexPlusModelPickerCatalogProviderMap",
    "codexPlusModelPickerCatalogProviderNames",
    "codexPlusModelPickerRowIdentity",
    "codexPlusModelPickerRememberProvider",
    "codexPlusModelPickerDefaultGroupId",
    "codexPlusModelPickerDefaultProviderName",
    "list",
    `const codexPlusModelPickerGroupNameMaxLength = 64;
     const codexModelCatalog = {};
     ${functions}
     return codexPlusModelPickerUniqueGroupIds(
       codexPlusModelPickerGroups(codexPlusModelPickerRows(list)));`,
  );
  const groups = run(
    () => metadata,
    () => new Set(["寒鹤", "浮生"]),
    (row: { text: string }) => ({ text: row.text, groupable: true }),
    () => {},
    () => "hanhe",
    () => "寒鹤",
    { querySelectorAll: () => ["6.1", "6.1 (Sol)", "6.1（浮生）"].map((text) => ({ text })) },
  );
  assert.deepEqual(groups.map((group: { name: string; entries: unknown[]; isDefault: boolean }) =>
    [group.name, group.entries.length, group.isDefault]), [
    ["寒鹤", 2, true],
    ["浮生", 1, false],
  ]);
  assert.equal(groups[0].entries[1].name, "6.1 (Sol)");
});

it("默认标志不改变无障碍语义：读屏会念出「默认供应商」", async () => {
  const source = await readShard();
  assert.match(source, /（默认供应商）/);
  assert.match(source, /\(default provider\)/);
});

async function catalogGroupingHarness() {
  const source = await readShard();
  const functions = [
    "providerNameOf",
    "codexPlusModelPickerDefaultGroupId",
    "codexPlusModelPickerDefaultProviderName",
    "codexPlusModelPickerSplitLabel",
    "codexPlusModelPickerNormalizeStrict",
    "codexPlusModelPickerNormalize",
    "codexPlusModelPickerCatalogAliases",
    "codexPlusModelPickerCatalogProviderMap",
    "codexPlusModelPickerCatalogProviderNames",
    "codexPlusModelPickerRows",
    "codexPlusModelPickerFilteredCatalog",
    "codexPlusModelPickerGroups",
    "codexPlusModelPickerUniqueGroupIds",
  ].map((name) => {
    const start = source.indexOf(`  function ${name}(`);
    assert.ok(start >= 0, name);
    const end = source.indexOf("\n  }\n", start);
    return source.slice(start, end + 5);
  }).join("\n");
  return new Function("catalog", "labels", `
    const codexModelCatalog = catalog;
    const codexPlusModelPickerGroupNameMaxLength = 24;
    const codexPlusModelPickerGroupsState = { defaultGroupId: "obsolete" };
    const codexPlusModelPickerRowIdentity = row => ({ text: row.text, groupable: true });
    ${functions}
    const list = { querySelectorAll: () => labels.map(text => ({text})) };
    return codexPlusModelPickerUniqueGroupIds(
      codexPlusModelPickerGroups(codexPlusModelPickerRows(list)));
  `);
}

function supplierCatalog(reverse = false) {
  const entries = [
    ["cpx-a/gpt-6.1-sol", { displayName: "GPT-6.1-Sol（浮生（ccswitch））",
      codex_plus_group: "浮生（ccswitch）", codex_plus_group_id: "a" }],
    ["gpt-6.1-sol", { displayName: "GPT-6.1-Sol",
      codex_plus_group: "寒鹤的小破站（ccswitch）", codex_plus_group_id: "b", codex_plus_default: true }],
  ] as const;
  return { status: "ok", models: entries.map(([slug]) => slug),
    codex_plus_default_group_id: "b",
    modelMetadata: Object.fromEntries(reverse ? [...entries].reverse() : entries) };
}

it("真实目录形式：嵌套括号、原生空格标签及同名裸 slug 均按实际供应商解析", async () => {
  const run = await catalogGroupingHarness();
  for (const reverse of [false, true]) {
    const groups = run(supplierCatalog(reverse), [
      "6.1 Sol（浮生（ccswitch））",
      "Codex Auto Review（浮生（ccswitch））",
      "6.1 Sol",
      "6 (Sol)",
      "6 Astra",
    ]);
    assert.deepEqual(groups.map((g: { id: string; name: string; entries: unknown[]; isDefault: boolean }) =>
      [g.id, g.name, g.entries.length, g.isDefault]), [
      ["a", "浮生（ccswitch）", 2, false],
      ["b", "寒鹤的小破站（ccswitch）", 3, true],
    ]);
    assert.equal(groups[0].entries[0].name, "6.1 Sol");
    assert.equal(groups[1].entries[1].name, "6 (Sol)");
  }
});

it("默认身份随目录更新，过期模型元数据不会抢占有效别名", async () => {
  const run = await catalogGroupingHarness();
  const catalog = supplierCatalog();
  catalog.models = ["cpx-a/gpt-6.1-sol"];
  catalog.codex_plus_default_group_id = "a";
  const groups = run(catalog, ["6.1 Sol（浮生（ccswitch））"]);
  assert.equal(groups[0].id, "a");
  assert.equal(groups[0].isDefault, true);
  const noDefault = run({ status: "ok", modelMetadata: {} }, ["Unrecognized (Variant)"]);
  assert.equal(noDefault[0].isDefault, false);
});

it("聚合目录筛选过的模型不会被原生缓存或另一供应商的同名模型补回", async () => {
  const run = await catalogGroupingHarness();
  const catalog = { ...supplierCatalog(),
    sources: [{ type: "aggregate_model_catalog" }] };
  const groups = run(catalog, [
    "6.1 Sol（浮生（ccswitch））", "Codex Auto Review（浮生（ccswitch））",
    "6.1 Sol", "6 (Sol)", "Image-2",
  ]);
  assert.deepEqual(groups.map((group: { id: string; entries: unknown[] }) =>
    [group.id, group.entries.length]), [["a", 1], ["b", 1]]);
});

it("聚合名单同时应用到本地模型数据层，远程主机和失效目录保留原列表", async () => {
  const source = await readFile(new URL(
    "../../../assets/inject/renderer-inject/70-model-catalog.js", import.meta.url), "utf8");
  const start = source.indexOf("  function codexPureApiAllowedModelSet(");
  const end = source.indexOf("\n  }\n", start);
  const resolve = new Function("profile", "catalog", "host", `
    const codexRemoteSessionActiveProfile = () => profile;
    const codexPlusModelUnlockEnabled = () => true;
    const codexPlusBackendSettingsLoaded = true;
    const codexModelCatalog = catalog;
    const codexPlusModelNames = () => catalog.models;
    ${source.slice(start, end + 5)}
    return codexPureApiAllowedModelSet(host);
  `);
  const catalog = { status: "ok", model_provider: "agg", models: ["first", "second"],
    sources: [{ id: "aggregate:agg" }] };
  assert.deepEqual([...resolve({ id: "agg", relayMode: "aggregate" }, catalog, "local")], catalog.models);
  assert.equal(resolve({ id: "agg", relayMode: "aggregate" }, catalog, "remote"), null);
  assert.equal(resolve({ id: "other", relayMode: "aggregate" }, catalog, "local"), null);
  assert.equal(resolve({ id: "agg", relayMode: "aggregate" }, { ...catalog, status: "failed" }, "local"), null);
});
