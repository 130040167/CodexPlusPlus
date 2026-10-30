import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";

type ModelMetadata = Record<string, {
  displayName: string;
  codex_plus_group: string;
  codex_plus_group_id: string;
  codex_plus_favorite?: boolean;
}>;

type Catalog = {
  status: string;
  models: string[];
  modelMetadata: ModelMetadata;
  codex_plus_default_group_id?: string;
};

const source = readFileSync(new URL("../../../assets/inject/renderer-inject/71-model-picker-groups.js", import.meta.url), "utf8");
const startup = source.indexOf("\n  codexPlusModelPickerInstallStyle();");
assert.ok(startup > 0);

function fixture(catalog: Catalog) {
  const context: any = {
    window: {}, codexModelCatalog: catalog,
    registerCodexPlusExtensionSelector() {}, queueMicrotask() {},
  };
  vm.runInNewContext(`${source.slice(0, startup)}
    globalThis.testModelPicker = {
      rows: codexPlusModelPickerRows,
      groups: (list) => codexPlusModelPickerUniqueGroupIds(
        codexPlusModelPickerGroups(codexPlusModelPickerRows(list), codexPlusModelPickerDefaultProviderName())),
      refresh: (catalog) => { codexModelCatalog = catalog; codexPlusModelPickerCatalogChanged(); },
    };`, context);
  return context.testModelPicker;
}

function row(text: string, options: { model?: string; selectedParent?: string; subtitle?: string } = {}) {
  const label = { textContent: text, classList: { contains: () => false }, getAttribute: () => null };
  const children = [label];
  if (options.subtitle) children.push({ ...label, textContent: options.subtitle });
  const inner = { children, firstElementChild: label, classList: { contains: (name: string) => name === "flex-col" && !!options.subtitle } };
  const outer = options.subtitle ? inner : { firstElementChild: inner, classList: { contains: () => false } };
  const content = { querySelector: (selector: string) => selector === ".flex-1" ? outer : null };
  const element: any = {
    querySelector: (selector: string) => selector === "[data-menu-row-content]" ? content : null,
    classList: { contains: () => false }, getAttribute: () => null,
  };
  if (options.model || options.selectedParent) {
    element.__reactFiber$fixture = { key: options.model, memoizedProps: {}, return: {
      memoizedProps: { model: options.selectedParent || "a/sol", models: [], onSelectModel() {} },
    } };
  }
  return element;
}

function list(rows: unknown[]) {
  return { querySelectorAll: (selector: string) => {
    assert.equal(selector, '[role="menuitemradio"]');
    return rows;
  } };
}

function entries(api: any, rows: unknown[]) {
  return Array.from(api.rows(list(rows)), (entry: any) => ({
    name: entry.name, groupId: entry.groupId, favorite: entry.favorite,
  }));
}

test("abbreviated same-name models retain zero-width identities, groups and favorites", () => {
  const metadata: ModelMetadata = {
    "a/sol": { displayName: "GPT-6.1-Sol\u200b", codex_plus_group: "Provider A", codex_plus_group_id: "a" },
    "b/sol": { displayName: "GPT-6.1-Sol\u200b\u200b", codex_plus_group: "Provider B", codex_plus_group_id: "b" },
    sol: { displayName: "GPT-6.1-Sol\u200c", codex_plus_group: "Auto", codex_plus_group_id: "auto" },
    "group/coding": { displayName: "Coding Models", codex_plus_group: "Model Groups", codex_plus_group_id: "groups", codex_plus_favorite: true },
    "a/astra": { displayName: "GPT-Astra", codex_plus_group: "Provider A", codex_plus_group_id: "a" },
  };
  const api = fixture({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata, codex_plus_default_group_id: "a" });
  const rows = [row("Default", { subtitle: "Recommended" }), row("6.1 Sol\u200b"), row("6.1 Sol\u200b\u200b"), row("6.1 Sol\u200c"), row("Coding Models"), row("GPT-Astra")];
  assert.deepEqual(entries(api, rows).map((entry) => [entry.groupId, entry.favorite]), [
    ["a", false], ["b", false], ["auto", false], ["groups", true], ["a", false],
  ]);
  assert.deepEqual(Array.from(api.groups(list(rows)), (group: any) => [group.id, group.entries.length, group.isDefault]), [
    ["a", 2, true], ["b", 1, false], ["auto", 1, false], ["groups", 1, false],
  ]);
});

test("row model identity disambiguates identical names without inheriting the selected parent model", () => {
  const metadata: ModelMetadata = {
    "a/sol": { displayName: "GPT-Sol", codex_plus_group: "Provider A", codex_plus_group_id: "a" },
    "b/sol": { displayName: "GPT-Sol", codex_plus_group: "Provider B", codex_plus_group_id: "b" },
    sol: { displayName: "GPT-Sol", codex_plus_group: "Auto", codex_plus_group_id: "auto" },
  };
  const api = fixture({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
  const rows = [row("GPT-Sol", { model: "a/sol" }), row("GPT-Sol", { model: "b/sol" }),
    row("GPT-Sol", { model: "sol" }), row("GPT-Sol", { selectedParent: "a/sol" }), row("GPT-Sol")];
  assert.deepEqual(entries(api, rows).map((entry) => entry.groupId), ["a", "b", "auto"]);
});

test("parenthesized model variants are preserved unless the catalog identifies a provider", () => {
  const metadata: ModelMetadata = {
    "a/variant": { displayName: "GPT-6 (Sol)", codex_plus_group: "Provider A", codex_plus_group_id: "a" },
    "b/sol": { displayName: "GPT-Sol（Provider B）", codex_plus_group: "Provider B", codex_plus_group_id: "b" },
  };
  const api = fixture({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
  assert.deepEqual(entries(api, [row("GPT-6 (Sol)"), row("GPT-Sol（Provider B）")]), [
    { name: "GPT-6 (Sol)", groupId: "a", favorite: false },
    { name: "GPT-Sol", groupId: "b", favorite: false },
  ]);
});

test("catalog refresh excludes removed models even when stale metadata remains", () => {
  const metadata: ModelMetadata = {
    "a/sol": { displayName: "GPT-Sol", codex_plus_group: "Provider A", codex_plus_group_id: "a" },
    "b/astra": { displayName: "GPT-Astra（Provider B）", codex_plus_group: "Provider B", codex_plus_group_id: "b" },
  };
  const api = fixture({ status: "ok", models: Object.keys(metadata), modelMetadata: metadata });
  const rows = [row("GPT-Sol"), row("GPT-Astra（Provider B）")];
  assert.deepEqual(entries(api, rows).map((entry) => entry.groupId), ["a", "b"]);
  api.refresh({ status: "ok", models: ["a/sol"], modelMetadata: metadata });
  assert.deepEqual(entries(api, rows).map((entry) => entry.groupId), ["a"]);
});
