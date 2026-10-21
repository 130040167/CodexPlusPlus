import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import test from "node:test";
import ts from "typescript";

// 独立运行真实页面函数和事件处理器，替换 hooks/IPC；不启动本机 ZCode、不连 CDP。
const app = ts.createSourceFile("App.tsx", readFileSync(new URL("./App.tsx", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
function sourceOf(name: string): string {
  const node = app.statements.find((candidate): candidate is ts.FunctionDeclaration => ts.isFunctionDeclaration(candidate) && candidate.name?.text === name);
  assert.ok(node, `App.tsx 缺少 ${name}`);
  return node.getText(app);
}
// 功能组/开关是真实的小组件，抽出来一起跑，页面文本与开关结构才和线上一致。
const source = ts.transpileModule(
  [sourceOf("ZcodeEnhancementScreen"), sourceOf("FeatureGroup"), sourceOf("FeatureToggle"), sourceOf("ToggleVisual")].join("\n\n"),
  { compilerOptions: { jsx: ts.JsxEmit.React, target: ts.ScriptTarget.ES2022 } },
).outputText;

type Element = { type: string; props: Record<string, any> };
type Call = { command: string; args?: Record<string, unknown>; resolve: (value: unknown) => void; reject: (error: unknown) => void };
function harness() {
  const hooks: any[] = [];
  const cleanups: (() => void)[] = [];
  const calls: Call[] = [];
  let cursor = 0;
  let changes = 0;
  const context = vm.createContext({
    React: {
      // 浅渲染桩：函数组件直接展开（FeatureGroup/FeatureToggle 是纯函数），
      // 其余基础组件保留 type 供断言按名字查找。
      createElement: (type: unknown, props: object, ...children: unknown[]) => {
        const merged = { ...props, children };
        return typeof type === "function" ? (type as (value: Record<string, unknown>) => unknown)(merged) : { type, props: merged };
      },
    },
    ...Object.fromEntries(["Panel", "CardContent", "Button", "Info", "RefreshCw", "Play", "RotateCcw"].map((name) => [name, name])),
    t: (text: string) => text,
    tf: (template: string, args: Array<string | number>) => template.replace(/\{(\d+)\}/g, (_match, index) => String(args[Number(index)] ?? "")),
    stringifyError: String,
    invoke: (command: string, args?: Record<string, unknown>) => new Promise((resolve, reject) => calls.push({ command, args, resolve, reject })),
    useState: (initial: unknown) => {
      const index = cursor++;
      if (!(index in hooks)) hooks[index] = initial;
      return [hooks[index], (value: unknown) => { changes++; hooks[index] = value; }];
    },
    useRef: (initial: unknown) => {
      const index = cursor++;
      if (!(index in hooks)) hooks[index] = { current: initial };
      return hooks[index];
    },
    useEffect: (effect: () => (() => void)) => {
      const index = cursor++;
      if (!(index in hooks)) { hooks[index] = true; cleanups.push(effect()); }
    },
  });
  vm.runInContext(`${source}\nglobalThis.render = ZcodeEnhancementScreen;`, context);
  return {
    calls,
    render: (props?: Record<string, unknown>) => { cursor = 0; return context.render(props) as Element; },
    unmount: () => cleanups.forEach((cleanup) => cleanup?.()),
    changes: () => changes,
  };
}
function text(element: any): string {
  if (element == null || typeof element === "boolean") return "";
  if (Array.isArray(element)) return element.map(text).join("");
  return typeof element === "object" ? text(element.props.children) : String(element);
}
function nodes(element: any): Element[] {
  if (element == null || typeof element !== "object") return [];
  if (Array.isArray(element)) return element.flatMap(nodes);
  return [...(typeof element.type === "string" ? [element as Element] : []), ...nodes(element.props.children)];
}
function buttons(element: any): Element[] {
  return nodes(element).filter((node) => node.type === "Button");
}
function inputs(element: any): Element[] {
  return nodes(element).filter((node) => node.type === "input");
}
const tick = () => new Promise((resolve) => setImmediate(resolve));
const ready = {
  status: "ok",
  message: "ZCode 增强状态已读取。",
  supported: true,
  appPath: "/Applications/ZCode.app",
  sourceVersion: "3.16.2",
  running: true,
  pid: 26431,
  debugPort: 9333,
  injected: true,
  voiceInput: true,
  voiceServerReady: true,
  needsRestart: false,
  dictationEnabled: true,
};

test("进入 ZCode 页面自动检测一次，成功后展示版本、PID、端口与注入状态", async () => {
  const screen = harness();
  screen.render();
  assert.equal(screen.calls.length, 1);
  assert.equal(screen.calls[0].command, "zcode_enhancement_status");
  // 真实 React 里 effect 的 setState 会触发重渲染；harness 手动再渲染一次读取 busy。
  assert.match(text(screen.render()), /检测中/);
  screen.calls[0].resolve(ready);
  await tick();
  const page = screen.render();
  // 布局对齐「Codex增强」设置页：单列 settings-page，不再用 Claude 增强页的两栏容器。
  assert.match(String(page.props.className), /settings-page/);
  assert.match(String(page.props.className), /enhance-page/);
  assert.match(String(page.props.className), /zcode-enhance-page/);
  assert.doesNotMatch(String(page.props.className), /claude-localization-page/);
  assert.match(text(page), /启动与注入/);
  assert.match(text(page), /语音输入/);
  assert.match(text(page), /官方版本/);
  assert.match(text(page), /3\.16\.2/);
  assert.match(text(page), /运行中（PID 26431）/);
  assert.match(text(page), /9333/);
  assert.match(text(page), /已注入/);
  assert.equal(buttons(page).find((button) => text(button) === "启动并注入")!.props.disabled, false);
  assert.equal(buttons(page).find((button) => text(button) === "重新注入")!.props.disabled, false);
});

test("顶部总开关和功能组开关都调用 zcode_enhancement_set_voice_input 并回填返回状态", async () => {
  let savedSettings = 0;
  const screen = harness();
  // 开关由命令直接写 settings.json，成功后必须通知父级同步表单，避免下次保存把旧值写回去。
  const render = () => screen.render({ onVoiceInputSaved: () => { savedSettings++; } });
  render();
  screen.calls[0].resolve(ready);
  await tick();
  let page = render();
  const toggles = inputs(page);
  assert.equal(toggles.length, 2, "顶部总开关和「语音输入」组各有一个开关");
  assert.equal(toggles[0].props.checked, true);
  assert.equal(toggles[1].props.checked, true);
  toggles[1].props.onChange({ currentTarget: { checked: false } });
  assert.equal(screen.calls.length, 2);
  assert.equal(screen.calls[1].command, "zcode_enhancement_set_voice_input");
  assert.equal(screen.calls[1].args?.enabled, false);
  assert.match(text(screen.render()), /已注入/);
  screen.calls[1].resolve({ ...ready, voiceInput: false });
  await tick();
  page = render();
  assert.deepEqual(inputs(page).map((node) => node.props.checked), [false, false]);
  assert.equal(savedSettings, 1);
  // 顶部总开关同样走同一个命令。
  inputs(page)[0].props.onChange({ currentTarget: { checked: true } });
  assert.equal(screen.calls.length, 3);
  assert.equal(screen.calls[2].command, "zcode_enhancement_set_voice_input");
  assert.equal(screen.calls[2].args?.enabled, true);
});

test("启动并注入按钮调用 action/launch，成功后用返回状态刷新页面", async () => {
  const screen = harness();
  screen.render();
  screen.calls[0].resolve(ready);
  await tick();
  const launch = buttons(screen.render()).find((button) => text(button) === "启动并注入")!;
  launch.props.onClick();
  assert.equal(screen.calls.length, 2);
  assert.equal(screen.calls[1].command, "zcode_enhancement_action");
  assert.equal(screen.calls[1].args?.action, "launch");
  assert.match(text(screen.render()), /启动中/);
  screen.calls[1].resolve({ ...ready, injected: false, needsRestart: true, voiceInput: false });
  await tick();
  assert.match(text(screen.render()), /需要重启/);
  assert.match(text(screen.render()), /注入前的进程/);
});

test("检测失败展示错误，检测按钮走 action/detect 且未安装时禁用启动", async () => {
  const screen = harness();
  screen.render();
  screen.calls[0].resolve({ status: "failed", message: "未找到官方 ZCode.app" });
  await tick();
  let page = screen.render();
  assert.match(text(page), /未找到官方 ZCode\.app/);
  assert.match(text(page), /未检测/);
  const detect = buttons(page).find((button) => text(button) === "检测 ZCode")!;
  detect.props.onClick();
  assert.equal(screen.calls.length, 2);
  assert.equal(screen.calls[1].command, "zcode_enhancement_action");
  assert.equal(screen.calls[1].args?.action, "detect");
  screen.calls[1].resolve({
    ...ready, supported: false, running: false, pid: null, debugPort: null,
    injected: false, voiceInput: false, needsRestart: false,
  });
  await tick();
  page = screen.render();
  assert.match(text(page), /未安装 ZCode/);
  assert.equal(buttons(page).find((button) => text(button) === "启动并注入")!.props.disabled, true);
  assert.equal(buttons(page).find((button) => text(button) === "重新注入")!.props.disabled, true);
  assert.equal(inputs(page).every((node) => node.props.disabled), true);
});

test("IPC 抛错时展示 stringifyError 文本，且不写入不可用的状态", async () => {
  const screen = harness();
  screen.render();
  screen.calls[0].reject(new Error("IPC 不可用"));
  await tick();
  const page = screen.render();
  assert.match(text(page), /IPC 不可用/);
  assert.match(text(page), /未检测/);
});

test("卸载后迟到的结果不能写回已卸载页面", async () => {
  const screen = harness();
  screen.render();
  screen.unmount();
  const before = screen.changes();
  screen.calls[0].resolve(ready);
  await tick();
  assert.equal(screen.changes(), before);
});
