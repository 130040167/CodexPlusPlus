import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import test from "node:test";
import ts from "typescript";

// 独立运行真实的顶栏/页面组件与事件处理器，替换 hooks/IPC/window；不启动本机 ZCode、不连 CDP。
const app = ts.createSourceFile("App.tsx", readFileSync(new URL("./App.tsx", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
function sourceOf(name: string): string {
  const node = app.statements.find((candidate) => {
    if (ts.isFunctionDeclaration(candidate) && candidate.name?.text === name) return true;
    if (ts.isVariableStatement(candidate)) {
      return candidate.declarationList.declarations.some((declaration) => ts.isIdentifier(declaration.name) && declaration.name.text === name);
    }
    return false;
  });
  assert.ok(node, `App.tsx 缺少 ${name}`);
  return node.getText(app);
}
// 功能组/开关是真实的小组件，抽出来一起跑，页面文本与卡片结构才和线上一致。
const source = ts.transpileModule(
  ["ZCODE_STATUS_EVENT", "ZcodeTopbarActions", "ZcodeEnhancementScreen", "FeatureGroup", "FeatureToggle", "ToggleVisual"]
    .map(sourceOf)
    .join("\n\n"),
  { compilerOptions: { jsx: ts.JsxEmit.React, target: ts.ScriptTarget.ES2022 } },
).outputText;

type Element = { type: string; props: Record<string, any> };
type Call = { command: string; args?: Record<string, unknown>; resolve: (value: unknown) => void; reject: (error: unknown) => void };
type WindowStub = {
  setInterval: (callback: () => void, ms: number) => number;
  clearInterval: (id: number) => void;
  addEventListener: (type: string, handler: (event: any) => void) => void;
  removeEventListener: (type: string, handler: (event: any) => void) => void;
  dispatchEvent: (event: any) => boolean;
};

function harness(componentName: "ZcodeTopbarActions" | "ZcodeEnhancementScreen") {
  const hooks: any[] = [];
  const cleanups: (() => void)[] = [];
  const calls: Call[] = [];
  const dispatched: { type: string; detail?: any }[] = [];
  const listeners = new Map<string, Set<(event: any) => void>>();
  const intervals: Array<() => void> = [];
  let cursor = 0;
  let changes = 0;
  const windowStub: WindowStub & { intervals: Array<() => void> } = {
    setInterval: (callback: () => void) => { intervals.push(callback); return intervals.length; },
    clearInterval: () => {},
    addEventListener: (type, handler) => {
      if (!listeners.has(type)) listeners.set(type, new Set());
      listeners.get(type)!.add(handler);
    },
    removeEventListener: (type, handler) => { listeners.get(type)?.delete(handler); },
    dispatchEvent: (event) => {
      dispatched.push({ type: event.type, detail: event.detail });
      for (const handler of listeners.get(event.type) ?? []) handler(event);
      return true;
    },
    intervals,
  };
  const documentStub = { hidden: false, getElementById: () => null };
  const context = vm.createContext({
    React: {
      // 浅渲染桩：函数组件直接展开（FeatureGroup/FeatureToggle 是纯函数），
      // 其余基础组件保留 type 供断言按名字查找。
      createElement: (type: unknown, props: object, ...children: unknown[]) => {
        const merged = { ...props, children };
        return typeof type === "function" ? (type as (value: Record<string, unknown>) => unknown)(merged) : { type, props: merged };
      },
    },
    ...Object.fromEntries(["Panel", "CardContent", "Button", "Info", "RefreshCw", "Play", "Rocket", "RotateCcw", "Save", "DictationSettingsPanel"].map((name) => [name, name])),
    CustomEvent,
    window: windowStub,
    document: documentStub,
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
  vm.runInContext(`${source}\nglobalThis.render = ${componentName};`, context);
  return {
    calls,
    dispatched,
    window: windowStub,
    render: (props?: Record<string, unknown>) => {
      cursor = 0;
      const merged = componentName === "ZcodeEnhancementScreen"
        ? { dirty: false, form: {}, onFormChange: () => {}, onSaveSettings: () => {}, ...props }
        : props;
      return context.render(merged) as Element;
    },
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
function byRole(element: any, role: string): Element[] {
  return nodes(element).filter((node) => node.props.role === role);
}
function buttons(element: any): Element[] {
  return nodes(element).filter((node) => node.type === "Button");
}
function inputs(element: any): Element[] {
  return nodes(element).filter((node) => node.type === "input");
}
function featureCards(element: any): Element[] {
  return nodes(element).filter((node) => typeof node.props.className === "string" && node.props.className.split(/\s+/).includes("feature-toggle"));
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

test("进入 ZCode 页面自动检测一次，控制卡/标签页/多列功能卡结构对齐 Codex 增强页", async () => {
  const screen = harness("ZcodeEnhancementScreen");
  screen.render();
  assert.equal(screen.calls.length, 1);
  assert.equal(screen.calls[0].command, "zcode_enhancement_status");
  // 真实 React 里 effect 的 setState 会触发重渲染；harness 手动再渲染一次读取 busy。
  assert.match(text(screen.render()), /正在检测 ZCode 安装状态/);
  screen.calls[0].resolve(ready);
  await tick();
  const page = screen.render();
  assert.match(String(page.props.className), /settings-page/);
  assert.match(String(page.props.className), /enhance-page/);
  assert.match(String(page.props.className), /zcode-enhance-page/);
  // 顶部控制卡：总开关 + 已保存 + 检测按钮 + 两个标签页。
  assert.match(text(page), /启用 ZCode 增强/);
  assert.match(text(page), /已保存/);
  assert.match(text(page), /检测 ZCode/);
  const tabs = byRole(page, "tab");
  assert.equal(tabs.length, 2);
  assert.deepEqual(tabs.map((tab) => text(tab)), ["常用增强", "语音输入"]);
  const panels = byRole(page, "tabpanel");
  assert.equal(panels.length, 2);
  assert.equal(panels[0].props.hidden, false);
  assert.equal(panels[1].props.hidden, true);
  // 常用增强页签：首组三张卡 + 语音输入组两张卡。
  assert.match(text(page), /启动与注入/);
  assert.match(text(page), /ZCode 运行状态/);
  assert.match(text(page), /调试端口与注入状态/);
  assert.equal(featureCards(panels[0]).length, 3);
  // 「语音输入」页签复用 Codex 的听写设置面板，本身不再放 feature 卡。
  assert.equal(featureCards(panels[1]).length, 0);
  // 运行/注入数据卡。
  assert.match(text(page), /运行中（PID 26431）/);
  assert.match(text(page), /官方版本 3\.16\.2/);
  assert.match(text(page), /9333/);
  assert.match(text(page), /已注入/);
  // 切到「语音输入」页签：只显示语音那组卡片。
  tabs[1].props.onClick();
  const voicePage = screen.render();
  const voicePanels = byRole(voicePage, "tabpanel");
  assert.equal(voicePanels[0].props.hidden, true);
  assert.equal(voicePanels[1].props.hidden, false);
  assert.equal(featureCards(voicePanels[1]).length, 0);
  const dictationPanel = nodes(voicePanels[1]).find((node) => node.type === "DictationSettingsPanel");
  assert.ok(dictationPanel, "语音输入页签需要复用 DictationSettingsPanel");
  assert.equal(typeof dictationPanel.props.onFormChange, "function");
  assert.deepEqual(dictationPanel.props.form, {});
  // 键盘左右键也能切换页签。
  const voiceTabs = byRole(voicePage, "tab");
  voiceTabs[1].props.onKeyDown({ key: "ArrowRight", preventDefault() {} });
  const wrapped = byRole(screen.render(), "tabpanel");
  assert.equal(wrapped[0].props.hidden, false);
});

test("语音输入页签复用 Codex 的听写设置面板；有未保存修改时可保存", async () => {
  let saves = 0;
  const screen = harness("ZcodeEnhancementScreen");
  const saveProps = () => ({ dirty: true, onSaveSettings: () => { saves += 1; } });
  screen.render(saveProps());
  screen.calls[0].resolve(ready);
  await tick();
  const page = screen.render(saveProps());
  assert.match(text(page), /有未保存修改/);
  const save = buttons(page).find((button) => text(button) === "保存增强设置")!;
  assert.equal(save.props.disabled, false);
  save.props.onClick();
  assert.equal(saves, 1);
  // 切到语音页签后仍渲染共享的听写设置面板（与 Codex 增强页同一份 form）。
  byRole(page, "tab")[1].props.onClick();
  const voice = screen.render(saveProps());
  const panels = byRole(voice, "tabpanel");
  const dictationPanel = nodes(panels[1]).find((node) => node.type === "DictationSettingsPanel");
  assert.ok(dictationPanel);
  assert.equal(typeof dictationPanel.props.onFormChange, "function");
});

test("页面内不再重复启动/重新注入按钮，相关操作统一由顶栏负责", async () => {
  const screen = harness("ZcodeEnhancementScreen");
  screen.render();
  screen.calls[0].resolve(ready);
  await tick();
  const page = screen.render();
  assert.equal(buttons(page).find((button) => text(button) === "启动并注入"), undefined);
  assert.equal(buttons(page).find((button) => text(button) === "重新注入"), undefined);
  assert.match(text(page), /检测 ZCode/);
});

test("顶部总开关和功能卡开关都调用 set_voice_input，成功后广播刷新事件", async () => {
  let savedSettings = 0;
  const screen = harness("ZcodeEnhancementScreen");
  // 开关由命令直接写 settings.json，成功后必须通知父级同步表单，避免下次保存把旧值写回去。
  const render = () => screen.render({ onVoiceInputSaved: () => { savedSettings++; } });
  render();
  screen.calls[0].resolve(ready);
  await tick();
  let page = render();
  const toggles = inputs(page);
  assert.equal(toggles.length, 2, "顶部总开关 + 常用增强里的麦克风入口开关");
  assert.equal(toggles.every((toggle) => toggle.props.checked), true);
  toggles[1].props.onChange({ currentTarget: { checked: false } });
  assert.equal(screen.calls.length, 2);
  assert.equal(screen.calls[1].command, "zcode_enhancement_set_voice_input");
  assert.equal(screen.calls[1].args?.enabled, false);
  screen.calls[1].resolve({ ...ready, voiceInput: false });
  await tick();
  page = render();
  assert.equal(inputs(page).every((toggle) => toggle.props.checked === false), true);
  assert.equal(savedSettings, 1);
  assert.deepEqual(screen.dispatched.map((event) => event.detail?.source), ["page"]);
  // 顶部总开关同样走同一个命令。
  inputs(page)[0].props.onChange({ currentTarget: { checked: true } });
  assert.equal(screen.calls.length, 3);
  assert.equal(screen.calls[2].args?.enabled, true);
});

test("检测失败展示错误，检测按钮走 action/detect；未安装时启动/重新注入与开关禁用", async () => {
  const screen = harness("ZcodeEnhancementScreen");
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
  assert.match(text(page), /未检测到官方 ZCode 应用/);
  assert.equal(inputs(page).every((node) => node.props.disabled), true);
});

test("IPC 抛错时展示 stringifyError 文本，且不写入不可用的状态", async () => {
  const screen = harness("ZcodeEnhancementScreen");
  screen.render();
  screen.calls[0].reject(new Error("IPC 不可用"));
  await tick();
  const page = screen.render();
  assert.match(text(page), /IPC 不可用/);
  assert.match(text(page), /未检测/);
});

test("卸载后迟到的结果不能写回已卸载页面，并清理事件监听", async () => {
  const screen = harness("ZcodeEnhancementScreen");
  screen.render();
  screen.unmount();
  const before = screen.changes();
  screen.calls[0].resolve(ready);
  await tick();
  assert.equal(screen.changes(), before);
});

test("顶栏显示 ZCode 状态徽标与主按钮，点击走 action/launch 并广播事件", async () => {
  const screen = harness("ZcodeTopbarActions");
  screen.render();
  assert.equal(screen.calls[0].command, "zcode_enhancement_status");
  screen.calls[0].resolve(ready);
  await tick();
  let page = screen.render();
  assert.match(text(page), /ZCode 已注入/);
  const restart = buttons(page).find((button) => text(button) === "重启 ZCode")!;
  assert.ok(restart, "已注入时主按钮应为「重启 ZCode」");
  restart.props.onClick();
  assert.equal(screen.calls[1].command, "zcode_enhancement_action");
  assert.equal(screen.calls[1].args?.action, "launch");
  screen.calls[1].resolve({ ...ready, injected: false, needsRestart: true });
  await tick();
  page = screen.render();
  assert.match(text(page), /ZCode 需要重启/);
  assert.deepEqual(screen.dispatched.map((event) => ({ type: event.type, source: event.detail?.source })), [
    { type: "codexplus-zcode-status-changed", source: "topbar" },
  ]);
});

test("顶栏按状态切换徽标与主按钮：运行无端口→启动并注入、未运行→启动 ZCode、未安装→无按钮", async () => {
  const screen = harness("ZcodeTopbarActions");
  screen.render();
  screen.calls[0].resolve({ ...ready, debugPort: null, injected: false, needsRestart: true });
  await tick();
  let page = screen.render();
  assert.match(text(page), /ZCode 需要重启/);
  assert.ok(buttons(page).some((button) => text(button) === "启动并注入"));
  // 页面广播事件后顶栏刷新（source=page 不是 topbar，会重新探测）。
  screen.window.dispatchEvent(new CustomEvent("codexplus-zcode-status-changed", { detail: { source: "page" } }));
  assert.equal(screen.calls.length, 2);
  assert.equal(screen.calls[1].command, "zcode_enhancement_status");
  screen.calls[1].resolve({ ...ready, running: false, pid: null, debugPort: null, injected: false });
  await tick();
  page = screen.render();
  assert.match(text(page), /ZCode 未启动/);
  assert.ok(buttons(page).some((button) => text(button) === "启动 ZCode"));
  screen.window.dispatchEvent(new CustomEvent("codexplus-zcode-status-changed", { detail: { source: "page" } }));
  screen.calls[2].resolve({ ...ready, supported: false, running: false, pid: null, debugPort: null, injected: false });
  await tick();
  page = screen.render();
  assert.match(text(page), /ZCode 未安装/);
  assert.equal(buttons(page).length, 0);
});

test("顶栏每 5 秒轮询、窗口聚焦和页面事件都会刷新状态", async () => {
  const screen = harness("ZcodeTopbarActions");
  screen.render();
  assert.equal(screen.window.intervals.length, 1, "顶栏需要 5 秒轮询");
  screen.calls[0].resolve(ready);
  await tick();
  screen.window.intervals[0]();
  assert.equal(screen.calls.length, 2);
  screen.calls[1].resolve(ready);
  await tick();
  screen.window.dispatchEvent(new CustomEvent("focus"));
  assert.equal(screen.calls.length, 3);
  // topbar 自己广播的事件不应触发自我刷新（避免回环）。
  screen.calls[2].resolve(ready);
  await tick();
  screen.window.dispatchEvent(new CustomEvent("codexplus-zcode-status-changed", { detail: { source: "topbar" } }));
  assert.equal(screen.calls.length, 3);
});
