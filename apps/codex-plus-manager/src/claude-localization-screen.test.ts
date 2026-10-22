import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";
import test from "node:test";
import ts from "typescript";

// 独立运行真实页面函数和事件处理器，替换 hooks/IPC；不启动本机 Claude 或探测 PATH。
const app = ts.createSourceFile("App.tsx", readFileSync(new URL("./App.tsx", import.meta.url), "utf8"), ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const component = app.statements.find((node): node is ts.FunctionDeclaration => ts.isFunctionDeclaration(node) && node.name?.text === "ClaudeLocalizationScreen");
assert.ok(component);
const source = ts.transpileModule(component.getText(app), {
  compilerOptions: { jsx: ts.JsxEmit.React, target: ts.ScriptTarget.ES2022 },
}).outputText;

type Element = { type: string; props: Record<string, any> };
function harness() {
  const hooks: any[] = [];
  /** 页头槽位：createPortal 渲染进来的元素记在这里，用来断言「启动增强版」在右上角。 */
  const topbarSlot = { portal: null as unknown };
  const cleanups: (() => void)[] = [];
  const calls: { command: string; resolve: (value: unknown) => void }[] = [];
  let cursor = 0;
  let changes = 0;
  const context = vm.createContext({
    React: { createElement: (type: string, props: object, ...children: unknown[]) => ({ type, props: { ...props, children } }) },
    ...Object.fromEntries(["Panel", "CardHead", "CardContent", "Button", "Info", "RefreshCw", "ToggleVisual", "PackageOpen", "Play", "Rocket", "ShieldCheck"].map((name) => [name, name])),
    t: (text: string) => text,
    listen: () => Promise.resolve(() => {}),
    document: { querySelector: (selector: string) => (selector === "[data-topbar-actions]" ? topbarSlot : null) },
    createPortal: (child: unknown, target: typeof topbarSlot) => { target.portal = child; return null; },
    stringifyError: String,
    invoke: (command: string) => new Promise((resolve) => calls.push({ command, resolve })),
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
    useLayoutEffect: (effect: () => (() => void)) => {
      const index = cursor++;
      if (!(index in hooks)) { hooks[index] = true; cleanups.push(effect()); }
    },
  });
  vm.runInContext(`${source}\nglobalThis.render = ClaudeLocalizationScreen;`, context);
  return {
    calls,
    render: () => { cursor = 0; return context.render() as Element; },
    /** 最近一次渲染时 portal 进页头的元素。 */
    topbar: () => topbarSlot.portal as Element | null,
    unmount: () => cleanups.forEach((cleanup) => cleanup?.()),
    changes: () => changes,
  };
}
function text(element: any): string {
  if (element == null || typeof element === "boolean") return "";
  if (Array.isArray(element)) return element.map(text).join("");
  return typeof element === "object" ? text(element.props.children) : String(element);
}
function buttons(element: any): Element[] {
  if (element == null || typeof element !== "object") return [];
  if (Array.isArray(element)) return element.flatMap(buttons);
  return [...(element.type === "Button" ? [element] : []), ...buttons(element.props.children)];
}
const tick = () => new Promise((resolve) => setImmediate(resolve));
const ready = {
  status: "ok", supported: true, prepared: true, needsRebuild: false,
  running: false, nodeAvailable: true, sourceVersion: "1.2.3", copyVersion: "1.2.3",
  catalogEntries: 1234, localization: true, voiceInput: true,
};

test("进入 Claude 页面不自动检测，只有点击才调用 IPC 且重复点击不并发", async () => {
  const screen = harness();
  let page = screen.render();
  assert.equal(screen.calls.length, 0);
  assert.match(text(page), /未检测/);
  const detect = buttons(page).find((button) => text(button) === "检测 Claude")!;
  detect.props.onClick();
  detect.props.onClick();
  assert.equal(screen.calls.length, 1);
  assert.equal(screen.calls[0].command, "claude_localization_status");
  assert.match(text(screen.render()), /检测中/);
  screen.calls[0].resolve({ ...ready, supported: false, prepared: false });
  await tick();
  page = screen.render();
  assert.match(text(page), /未检测到 Claude，请先安装官方应用/);
  assert.equal(buttons(page).find((button) => text(button) === "制作增强版")!.props.disabled, true);
});

test("检测中切走页面后，迟到的检测结果不能写回已卸载页面", async () => {
  const screen = harness();
  buttons(screen.render()).find((button) => text(button) === "检测 Claude")!.props.onClick();
  screen.unmount();
  const before = screen.changes();
  screen.calls[0].resolve(ready);
  await tick();
  assert.equal(screen.changes(), before);
});

test("检查副本失败或检测超时保留最后有效状态，同时显示可重试错误", async () => {
  const screen = harness();
  buttons(screen.render()).find((button) => text(button) === "检测 Claude")!.props.onClick();
  screen.calls[0].resolve(ready);
  await tick();
  buttons(screen.render()).find((button) => text(button) === "检查增强版")!.props.onClick();
  screen.calls[1].resolve({ status: "failed", message: "副本检查失败", data: {} });
  await tick();
  assert.match(text(screen.render()), /1\.2\.3/);
  assert.match(text(screen.render()), /副本检查失败/);
  buttons(screen.render()).find((button) => text(button) === "检测 Claude")!.props.onClick();
  screen.calls[2].resolve({ status: "failed", message: "Claude 检测超时，检测进程已结束" });
  await tick();
  const page = screen.render();
  assert.match(text(page), /1\.2\.3/);
  assert.match(text(page), /检测超时/);
  assert.equal(buttons(page).find((button) => text(button) === "检测 Claude")!.props.disabled, false);
});

test("「启动增强版」放在页头右上角（与 Codex 的启动按钮同位），页面主体里不再重复", async () => {
  const screen = harness();
  // 槽位在 layout effect 里取到，React 会在绘制前同步重渲染；harness 手动再渲染一次。
  screen.render();
  let page = screen.render();
  const launch = () => buttons(screen.topbar()).find((button) => text(button) === "启动增强版");
  assert.ok(launch(), "启动按钮必须 portal 进 [data-topbar-actions]");
  assert.equal(buttons(page).some((button) => text(button) === "启动增强版"), false, "页面主体里不应再有启动按钮");
  assert.equal(launch()!.props.disabled, true, "未检测前不能启动");
  buttons(page).find((button) => text(button) === "检测 Claude")!.props.onClick();
  screen.calls[0].resolve(ready);
  await tick();
  page = screen.render();
  assert.equal(launch()!.props.disabled, false);
  launch()!.props.onClick();
  assert.equal(screen.calls[1].command, "claude_localization_action");
  screen.render();
  assert.ok(buttons(screen.topbar()).some((button) => text(button) === "启动中…"));
});
