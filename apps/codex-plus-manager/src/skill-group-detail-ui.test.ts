import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import vm from "node:vm";
import ts from "typescript";

type Element = { type: any; props: Record<string, any>; children: any[] };
const source = readFileSync(new URL("./ManagementExtra.tsx", import.meta.url), "utf8");
const file = ts.createSourceFile("ManagementExtra.tsx", source, ts.ScriptTarget.Latest, true, ts.ScriptKind.TSX);
const declarations = file.statements.filter((node) =>
  ts.isFunctionDeclaration(node) && ["SkillDetailDrawer", "SkillActionMenu"].includes(node.name?.text || ""));
assert.equal(declarations.length, 2);
const code = ts.transpileModule(declarations.map((node) => node.getText(file)).join("\n"), {
  compilerOptions: { jsx: ts.JsxEmit.React, jsxFactory: "jsx", jsxFragmentFactory: "fragment", module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;

const skill = { id: "sample", name: "Sample", repoKey: "owner/repo@main", repoPath: "skills/sample", installed: true, bundled: false };
const groups = [
  { key: "builtin", label: "内置", kind: "builtin", items: [] },
  { key: "user", label: "用户", kind: "user", items: [] },
  { key: "repo:owner/repo", label: "来源仓库", kind: "repo", items: [] },
  { key: "custom:工作", label: "工作", kind: "custom", items: [skill] },
];

function createHarness() {
  const slots: any[] = [];
  let cursor = 0;
  const assignments: string[] = [];
  const context: Record<string, any> = {
    exports: {}, jsx: (type: any, props: any, ...children: any[]) => ({ type, props: props || {}, children }), fragment: "fragment",
    useState: (initial: any) => {
      const index = cursor++;
      if (!(index in slots)) slots[index] = initial;
      return [slots[index], (value: any) => { slots[index] = typeof value === "function" ? value(slots[index]) : value; }];
    },
    useEffect: () => {},
    usePopoverLayer: () => ({ rootRef: null, menuRef: null, menuStyle: {} }),
    createPortal: (node: any) => node, document: { body: {} },
    repoUrl: () => "https://github.com/owner/repo/tree/main/skills/sample",
    sourceLabel: () => "owner/repo", importChannel: () => "", t: (value: string) => value,
  };
  for (const name of ["ManagementDrawer", "AppSelect", "ArrowRight", "ExternalLink", "Search", "Edit3", "MoreHorizontal", "ArrowLeft", "Check", "BookOpen", "FolderOpen", "ChevronRight", "Trash2"]) context[name] = name;
  vm.createContext(context);
  vm.runInContext(code, context);
  const props = { skill, groups, group: groups[3], originGroup: groups[2], assignment: "custom:工作", busy: false, canUninstall: true,
    onAssign: (value: string) => assignments.push(value), onClose: () => {}, onOpenUrl: () => {}, onOpenRepo: () => {}, onEditSource: () => {}, onMatch: () => {},
    onDetail: () => {}, onUninstall: () => {} };
  const render = (component: string, overrides = {}) => {
    cursor = 0;
    const elements: Element[] = [];
    const visit = (node: any) => {
      if (!node || typeof node !== "object") return;
      if (Array.isArray(node)) return node.forEach(visit);
      elements.push(node);
      node.children?.forEach(visit);
    };
    visit(context[component]({ ...props, ...overrides }));
    return elements;
  };
  return { render, assignments };
}

test("detail displays current group and can choose another group or automatic source", () => {
  const ui = createHarness();
  const elements = ui.render("SkillDetailDrawer");
  const select = elements.find((node) => node.type === "AppSelect")!;
  assert.equal(select.props.value, "custom:工作");
  assert.deepEqual(Array.from(select.props.options, (option: any) => option.label), ["用户", "来源仓库", "工作", "按来源自动分组 · 来源仓库"]);
  select.props.onChange("user");
  select.props.onChange("");
  assert.deepEqual(ui.assignments, ["user", ""]);
  assert.equal(ui.render("SkillDetailDrawer", { skill: { ...skill, bundled: true }, group: groups[0] }).some((node) => node.type === "AppSelect"), false);
});

test("installed skill can edit and rematch an existing source; bundled and uninstalled entries cannot", () => {
  const ui = createHarness();
  const labels = (overrides = {}) => ui.render("SkillDetailDrawer", overrides)
    .filter((node) => node.type === "button").map((node) => node.props["aria-label"]);
  assert.ok(labels().includes("修改仓库来源"));
  assert.ok(labels().includes("匹配仓库来源"));
  assert.ok(!labels({ skill: { ...skill, bundled: true } }).includes("修改仓库来源"));
  assert.ok(!labels({ skill: { ...skill, installed: false } }).includes("匹配仓库来源"));
});

test("group submenu shows the current group first and retains automatic source reset", () => {
  const ui = createHarness();
  let elements = ui.render("SkillActionMenu");
  elements.find((node) => node.type === "button" && node.props["aria-haspopup"] === "menu")!.props.onClick();
  elements = ui.render("SkillActionMenu");
  elements.find((node) => node.type === "button" && node.children.some((child: any) => child?.children?.includes("切换分组")))!.props.onClick();
  elements = ui.render("SkillActionMenu");
  const choices = elements.filter((node) => node.props.role === "menuitemradio");
  assert.equal(choices[0].props["aria-checked"], "true");
  assert.equal(choices[0].children[0].children[0], "工作");
  const reset = elements.find((node) => node.props.role === "menuitem" && node.children[0]?.children?.includes("按来源自动分组"))!;
  reset.props.onClick();
  assert.deepEqual(ui.assignments, [""]);
});
