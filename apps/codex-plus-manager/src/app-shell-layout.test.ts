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
  assert.match(styles, /\.shell \{\n  grid-template-columns: 248px minmax\(0, 1fr\);/);
  assert.doesNotMatch(styles, /\.app-rail\b|\.tool-chip\b|\.rail-brand\b/);
});

test("侧栏顶部是网关状态卡，点它进入总览；Agent 点名字先到各自的落脚页", () => {
  const sidebar = functionSource("AppSidebar");
  assert.match(sidebar, /className=\{`side-hub[^`]*`\} onClick=\{\(\) => onNavigate\("hub"\)\}/);
  assert.match(sidebar, /useGatewayPayload\(/);
  assert.match(text, /const agentHomeRoute: Record<string, Route> = \{ codex: "overview", claude: "claudeRelay", grok: "grok" \}/);
  assert.match(functionSource("App"), /onOpenAgent=\{\(toolId, destination\) => void switchTool\(toolId, destination \?\? agentHomeRoute\[toolId\] \?\? "overview"\)\}/);
});

test("所有 Agent 排成页签始终可见，下面是当前 Agent 的页面，仍按 navigationSections 的归属过滤", () => {
  const sidebar = functionSource("AppSidebar");
  assert.match(sidebar, /className="side-agent-tabs"/);
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
