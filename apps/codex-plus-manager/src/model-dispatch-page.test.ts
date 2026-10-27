import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const read = (name: string) => readFileSync(new URL(`./${name}`, import.meta.url), "utf8");

test("模型调度页只讲规则：调度表 + 模型组，不再重复总览的实时连线图", () => {
  const page = read("GatewayPage.tsx");
  assert.match(page, /<DispatchTable busy=\{busy\} onManage=\{onManage\} payload=\{payload\} run=\{run\} \/>/);
  assert.doesNotMatch(page, /RouteDiagram|RouteCard|routeNarrative/);
  assert.match(page, /\{ id: "routing", label: t\("模型调度"\) \}/);
  assert.match(read("App.tsx"), /id: "gatewayRouting", get label\(\) \{ return t\("模型调度"\); \}/);
});

test("总览「最近请求」可展开看这次怎么走的，右上角去请求日志", () => {
  const hub = read("HubOverview.tsx");
  assert.match(hub, /<h3>\{t\("最近请求"\)\}<\/h3>/);
  assert.match(hub, /onNavigate\("requestLog"\)/);
  assert.doesNotMatch(hub, /gatewayRouting/);
  assert.match(hub, /routeNarrative\(item\.agent, record, payload\)/);
});

test("界面不再出现「路由组」，统一叫模型组", () => {
  for (const name of ["GatewayPage.tsx", "GatewayAgents.tsx", "HubOverview.tsx", "gateway-model.ts"]) {
    assert.doesNotMatch(read(name), /"[^"\n]*路由组[^"\n]*"/, name);
  }
});
