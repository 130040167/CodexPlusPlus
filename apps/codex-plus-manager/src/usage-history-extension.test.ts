import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { test } from "node:test";

const shard = new URL("../../../assets/inject/renderer-inject/91-extension-api.js", import.meta.url);

async function routesAndCall(bridge: (route: string, payload: unknown) => Promise<unknown>) {
  const source = await readFile(shard, "utf8");
  const start = source.indexOf("const codexPlusExtensionRoutes = new Set([");
  const end = source.indexOf("\n  /**\n   * 当前脚本的 key", start);
  assert.ok(start > 0 && end > start);
  return new Function("window", `${source.slice(start, end)}
    return { routes: [...codexPlusExtensionRoutes], call: codexPlusExtensionCall };`)({
      __codexSessionDeleteBridge: bridge,
    }) as { routes: string[]; call: (route: string, payload?: unknown) => Promise<unknown> };
}

test("usage history route agrees with extension declarations and rejects write routes", async () => {
  const { routes, call } = await routesAndCall(async () => ({ status: "ok" }));
  const declarations = await readFile(
    new URL("../../../types/codex-plus-extensions.d.ts", import.meta.url), "utf8",
  );
  const union = declarations.match(/type CodexPlusExtensionRoute\s*=([\s\S]*?);/)?.[1];
  assert.ok(union);
  const declared = Array.from(union.matchAll(/"([^"]+)"/g), match => match[1]);
  assert.deepEqual([...routes].sort(), [...declared].sort());
  assert.ok(routes.includes("/usage/history"));
  for (const route of ["/settings/set", "/delete"]) {
    await assert.rejects(call(route), /未开放/);
  }
});

test("usage history forwards structured query and preserves partial and unavailable responses", async () => {
  const request = { includeActivity: true, includeSessionCursors: true, scopeSessionId: "session" };
  for (const status of ["ok", "partial", "unavailable"]) {
    const response = { status, complete: status === "ok", accountingVersion: 2, days: [] };
    const { call } = await routesAndCall(async (route, payload) => {
      assert.equal(route, "/usage/history");
      assert.deepEqual(payload, request);
      return response;
    });
    assert.deepEqual(await call("/usage/history", request), response);
  }
  const { call } = await routesAndCall(async () => ({ status: "failed", message: "history unavailable" }));
  await assert.rejects(call("/usage/history", request), /history unavailable/);
});
