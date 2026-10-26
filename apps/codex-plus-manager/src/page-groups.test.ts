import assert from "node:assert/strict";
import test from "node:test";
import { groupOfRoute, groupTarget, visibleGroupRoutes } from "./page-groups.ts";

type R = "overview" | "relay" | "proxy" | "claude" | "enhance";
const owner: Record<R, string | undefined> = { overview: undefined, relay: "codex", proxy: "codex", claude: "claude", enhance: "codex" };
const groups = [
  { id: "overview", routes: ["overview"] as R[] },
  { id: "providers", routes: ["relay", "proxy"] as R[] },
  { id: "enhance", routes: ["claude", "enhance"] as R[] },
];

test("按当前 Agent 过滤分组里的页面", () => {
  assert.deepEqual(visibleGroupRoutes(groups[1], (r) => owner[r], "codex"), ["relay", "proxy"]);
  assert.deepEqual(visibleGroupRoutes(groups[1], (r) => owner[r], "claude"), []);
  assert.deepEqual(visibleGroupRoutes(groups[2], (r) => owner[r], "claude"), ["claude"]);
  assert.deepEqual(visibleGroupRoutes(groups[0], (r) => owner[r], "grok"), ["overview"]);
});

test("找到页面所在的分组；点分组回到上次看的页面", () => {
  assert.equal(groupOfRoute(groups, "proxy" as R)?.id, "providers");
  assert.equal(groupOfRoute(groups, "overview" as R)?.id, "overview");
  assert.equal(groupTarget(["relay", "proxy"], "proxy"), "proxy");
  assert.equal(groupTarget(["relay", "proxy"], undefined), "relay");
  assert.equal(groupTarget(["claude"], "enhance"), "claude", "记住的页面当前 Agent 看不到时回到第一页");
});
