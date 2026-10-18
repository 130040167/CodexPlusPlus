import assert from "node:assert/strict";
import { it } from "node:test";
import { readFile } from "node:fs/promises";
import vm from "node:vm";

const CATALOG_URL = new URL(
  "../../../assets/inject/renderer-inject/70-model-catalog.js",
  import.meta.url,
);

it("string model collections withdraw models from the previous aggregate", async () => {
  const source = await readFile(CATALOG_URL, "utf8");
  const block = source.match(/  function patchModelNameArray\([^]*?\n  \}/)?.[0];
  assert.ok(block);
  const patch = vm.runInNewContext(`(() => {${block}; return patchModelNameArray;})()`, {
    stringArrayLooksPatchable: Array.isArray,
    codexAggregateAllowedModelSet: () => new Set(["cpx-current/sol"]),
    codexPureApiAllowedModelSet: () => null,
    prepareModelCollection: () => false,
    finishModelCollection: () => {},
    codexPlusModelNames: () => ["cpx-current/sol"],
  });
  const models = ["cpx-old/kimi", "cpx-current/sol"];
  assert.equal(patch(models, "local"), true);
  assert.deepEqual(models, ["cpx-current/sol"]);
});
