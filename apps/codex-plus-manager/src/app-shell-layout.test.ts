/** 外壳布局：一条侧栏（网关 / Agent / 工具），⌘K 面板，右下角提示卡。直接扫源码，防止结构被无意改回。 */
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import ts from "typescript";

const text = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const styles = readFileSync(new URL("./styles.css", import.meta.url), "utf8");
const app = ts.createSourceFile("App.tsx", text, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);

function functionSource(name: string) {
  const declaration = app.statements.find(
    (statement): statement is ts.FunctionDeclaration => ts.isFunctionDeclaration(statement) && statement.name?.text === name,
  );
  assert.ok(declaration, `${name} should exist`);
  return declaration.getText(app);
}

test("只保留一条侧栏，旧的图标栏和 Agent 子侧栏都已移除", () => {
  assert.doesNotMatch(text, /function ApplicationRail|function ToolSwitcher|className="sidebar"|className="app-rail"/);
  assert.match(functionSource("App"), /<AppSidebar\b/);
  assert.match(styles, /\.shell \{\n  grid-template-columns: 220px minmax\(0, 1fr\);/);
  assert.doesNotMatch(styles, /\.app-rail\b|\.tool-chip\b|\.rail-brand\b/);
});

test("侧栏顶部是网关状态卡，点它进入总览；Agent 点名字先到各自的落脚页", () => {
  const sidebar = functionSource("AppSidebar");
  assert.match(sidebar, /className=\{`side-hub[^`]*`\} onClick=\{\(\) => onNavigate\("hub"\)\}/);
  assert.match(sidebar, /useGatewayPayload\(/);
  assert.match(text, /const agentHomeRoute: Record<string, Route> = \{ codex: "overview", claude: "claude", grok: "overview" \}/);
  assert.match(functionSource("App"), /onOpenAgent=\{\(toolId, destination\) => void switchTool\(toolId, destination \?\? agentHomeRoute\[toolId\] \?\? "overview"\)\}/);
});

test("Agent 一行一个始终可见（与设计图一致），当前 Agent 的页面单独一组，仍按 navigationSections 的归属过滤", () => {
  const sidebar = functionSource("AppSidebar");
  assert.match(sidebar, /className=\{`side-item side-agent \$\{current \? "current" : ""\}`\}/);
  assert.match(sidebar, /\{groupLabel\("agent-pages", activeToolName\)\}/);
  // 状态点表示接入方式而不是运行状态（运行状态只在顶栏显示，见 tool-chip-status.test.ts）。
  assert.match(sidebar, /tone: "direct"/);
  assert.doesNotMatch(sidebar, /codexAppRunning|running/);
  assert.match(sidebar, /navigationSections\s*\.map\(\(section\) => section\.routes\.filter/);
  assert.match(sidebar, /return item && \(!item\.tool \|\| item\.tool === activeTool\)/);
});

test("⌘K 打开命令面板，总览页渲染在 hub 路由", () => {
  const entry = functionSource("App");
  assert.match(entry, /event\.key\.toLowerCase\(\) === "k"/);
  assert.match(entry, /<CommandPalette commands=\{paletteCommands\}/);
  assert.match(entry, /route === "hub" \? \(\s*<HubOverview/);
});

test("提示卡按语气分色、悬停暂停、不显示进度条，失败可复制详情", () => {
  const notice = functionSource("NoticeDialog");
  assert.match(notice, /noticeTone\(notice\.status\)/);
  assert.match(notice, /onMouseEnter=\{\(\) => setPaused\(true\)\}/);
  assert.match(notice, /tone === "error" && notice\.message/);
  assert.doesNotMatch(notice, /toast-progress/);
  assert.doesNotMatch(styles, /\.toast-progress\b|@keyframes toast-progress/);
});

test("Claude / Grok 的供应商配置页已去掉：要么经网关接入供应商池，要么照 Agent 自己的配置直连", () => {
  assert.doesNotMatch(text, /id: "claudeRelay"/);
  assert.doesNotMatch(text, /id: "grok", get label/);
  assert.doesNotMatch(text, /<ClaudeProviderPage|function GrokScreen/);
  const sections = text.slice(text.indexOf("const navigationSections"), text.indexOf("const globalNavigationRoutes"));
  assert.doesNotMatch(sections, /"claudeRelay"|"grok"/);
  assert.match(sections, /"claude"/, "Claude 增强保留");
});

test("页面不再画第二条标题栏：PageHeader 和工具页的操作都送进外壳顶栏", () => {
  const management = readFileSync(new URL("./ManagementPages.tsx", import.meta.url), "utf8");
  const extra = readFileSync(new URL("./ManagementExtra.tsx", import.meta.url), "utf8");
  const cache = readFileSync(new URL("./agent-cache.tsx", import.meta.url), "utf8");
  assert.match(management, /export function PageHeader\([^)]*\) \{\n  return <TopbarActions>\{actions\}<\/TopbarActions>;/);
  assert.match(extra, /function Header\([^)]*\) \{ return <TopbarActions>\{actions\}<\/TopbarActions>; \}/);
  for (const source of [management, extra]) assert.doesNotMatch(source, /className="mg-header"/);
  assert.match(cache, /<TopbarActions>/);
  assert.doesNotMatch(cache, /CardHeader|className="panel agent-cache-panel"/);
  // 顶栏里给页面操作留了单独的挂载点，排在 Codex 状态与启动按钮左边。
  assert.match(text, /<div className="topbar-page-actions" data-topbar-actions \/>/);
});

test("紧凑密度层：管理页不再套外框，覆盖规则带 .shell 前缀不依赖加载顺序", () => {
  const layer = styles.slice(styles.indexOf("/* ── Graphite · 紧凑密度"));
  assert.match(layer, /\.shell \.mg-page \{\n  margin: 0 auto;\n  overflow: visible;\n  border: 0;/);
  assert.match(layer, /\.shell \.topbar \{\n  min-height: 44px;/);
  // 层内每条规则都以 .shell 开头
  const selectors = [...layer.matchAll(/^([^\s/@}][^{]*)\{$/gm)].map((match) => match[1]);
  assert.ok(selectors.length > 20);
  for (const selector of selectors) assert.match(selector, /^\.shell /, selector);
});

test("请求日志、会话、MCP、Skills 拎成「全局管理」，管理所有 Agent，不再挂在 Codex 下面", () => {
  assert.match(text, /const managementNavigationRoutes: Route\[\] = \["requestLog", "sessions", "context", "skills"\]/);
  for (const id of ["requestLog", "sessions", "context", "skills"]) {
    const line = text.split("\n").find((row) => row.includes(`{ id: "${id}",`));
    assert.ok(line, id);
    assert.doesNotMatch(line, /tool: "codex"/, `${id} 不再只属于 Codex`);
  }
  const sections = text.slice(text.indexOf("const navigationSections"), text.indexOf("const globalNavigationRoutes"));
  assert.doesNotMatch(sections, /"requestLog"|"sessions"|"context"|"skills"/);
  assert.match(functionSource("AppSidebar"), /managementNavigationRoutes\.map/);
  // 网关的会话页并进「会话」：Codex 页签是完整的会话管理，其余页签是多 Agent 会话列表。
  assert.doesNotMatch(text, /"gatewaySessions"/);
  const entry = functionSource("App");
  assert.match(entry, /route === "sessions" && sessionTab !== "codex" \? \(/);
  assert.match(entry, /<SessionsView agent=\{sessionTab\}/);
  assert.match(entry, /route === "sessions" && sessionTab === "codex" \? \(\s*<ProjectSessionPage/);
});

test("请求日志按 Agent 筛选；网关记录带 agent 字段，没有的算 Codex", () => {
  const screen = functionSource("RequestLogScreen");
  assert.match(screen, /agentFilter !== "all" && requestLogAgent\(entry\) !== agentFilter/);
  assert.match(screen, /\[entries, filter, query, sessionFilter, agentFilter\]/);
  assert.match(functionSource("requestLogAgent"), /return entry\.agent \|\| "codex";/);
});
