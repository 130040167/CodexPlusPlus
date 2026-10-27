import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

const read = (name: string) => readFileSync(new URL(`./${name}`, import.meta.url), "utf8");

test("「模型」页：模型 / 模型组两个页签，模型页是供应商 × 模型矩阵，不再重复总览的实时连线图", () => {
  const page = read("GatewayPage.tsx");
  assert.match(page, /role="tablist" aria-label=\{t\("模型"\)\}/);
  assert.match(page, /<ModelMatrix busy=\{busy\} onManage=\{onManage\} onOpenProvider=\{onOpenProvider\} payload=\{payload\} rows=\{rows\} run=\{run\} \/>/);
  assert.match(page, /<GroupsPanel busy=\{busy\} payload=\{payload\} run=\{run\} \/>/);
  assert.match(page, /<table className="gw-matrix">/);
  assert.doesNotMatch(page, /RouteDiagram|RouteCard|routeNarrative|DispatchTable/);
  assert.match(page, /\{ id: "routing", label: t\("模型"\) \}/);
  assert.match(read("App.tsx"), /id: "gatewayRouting", get label\(\) \{ return t\("模型"\); \}/);
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

test("「多家自动切换」关着时矩阵只说明由哪几家提供：不显示先后数字、不出现「调整顺序」", () => {
  const page = read("GatewayPage.tsx");
  assert.match(page, /<i>\{autoModels && multi \? index \+ 1 : null\}<\/i>/);
  assert.match(page, /\{autoModels \? \(\s*<>[\s\S]*?t\("调整顺序"\)/);
  // 列就是池里打开的供应商，顺序与池一致；短名显示，完整（打码）名放悬停提示。
  assert.match(page, /const providers = useMemo\(\(\) => agentPoolProviders\(payload\), \[payload\]\);/);
  assert.match(page, /tf\("在供应商池里打开「\{0\}」", \[maskAccountName\(entry\.name\)\]\)/);
});

test("侧栏按数据流排：供应商池 → 模型 → Agent 接入；模型页列头跳到供应商池并展开那一家；模型下拉的模型组可跳去管理", () => {
  const app = read("App.tsx");
  assert.match(app, /const gatewayNavigationRoutes: Route\[\] = \["hub", "gatewayProviders", "gatewayRouting", "gateway", "gatewayUsage", "gatewaySettings"\];/);
  const page = read("GatewayPage.tsx");
  assert.match(page, /onClick=\{\(\) => onOpenProvider\(entry\.id\)\}/);
  assert.match(page, /gatewayJump\.provider = id;\s*setTab\("providers"\);/);
  assert.match(page, /gatewayJump\.modelsTab = "groups";\s*setTab\("routing"\);/);
  assert.match(read("GatewayProviders.tsx"), /const jumped = gatewayJump\.provider;/);
  assert.match(read("GatewayAgents.tsx"), /section\.key === "group" && nav\.openModelGroups \?/);
});
