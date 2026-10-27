import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import { modelRoutable, pickerSections, type GatewayPayload } from "./gateway-model.ts";

function payload(): GatewayPayload {
  return {
    gateway: { version: 1, bindings: {}, keyPolicy: { mode: "sticky" }, restoreOnExit: true, autoModels: true },
    port: 1,
    helperRunning: true,
    pool: [
      { id: "chatgpt", name: "ChatGPT Pro · 2649340134@qq.com", baseUrl: "", protocol: "responses", official: true, models: ["gpt-6.1-sol", "gpt-6.1-terra"], keyCount: 1, enabled: true },
      { id: "jojo", name: "jojocodemax", baseUrl: "https://jojo.test/v1", protocol: "responses", official: false, models: ["gpt-6.1-sol", "gpt-image-2"], keyCount: 1, enabled: true },
    ],
    groups: [],
    agents: [],
    health: { keys: [], providers: [] },
    codexProviders: [],
    claudeProviders: [],
    grokProfiles: [],
  };
}

test("模型选择器：多家都有的模型在每家名下固定发给那一家，另有一份「多家自动切换」；只有一家的用裸名", () => {
  const sections = pickerSections(payload(), "", () => "");
  const byKey = Object.fromEntries(sections.map((section) => [section.key, section.items.map((item) => item.model)]));
  assert.deepEqual(byKey["provider:chatgpt"], ["chatgpt/gpt-6.1-sol", "gpt-6.1-terra"]);
  assert.deepEqual(byKey["provider:jojo"], ["jojo/gpt-6.1-sol", "gpt-image-2"]);
  assert.deepEqual(byKey.auto, ["gpt-6.1-sol"]);
  const auto = sections.find((section) => section.key === "auto")!;
  assert.equal(auto.items[0].sub, "ChatGPT Pro · 26****34@qq.com → jojocodemax", "列出按顺序尝试的几家，账号打码");
  for (const model of ["chatgpt/gpt-6.1-sol", "jojo/gpt-6.1-sol", "gpt-6.1-sol"]) assert.equal(modelRoutable(payload(), model), true, model);
});

test("「多家自动切换」默认关：关着时没有自动那一组，只能选具体某一家", () => {
  const data = payload();
  data.gateway.autoModels = undefined;
  const keys = pickerSections(data, "", () => "").map((section) => section.key);
  assert.deepEqual(keys, ["provider:chatgpt", "provider:jojo"]);
  const page = readFileSync(new URL("./GatewayPage.tsx", import.meta.url), "utf8");
  assert.match(page, /"save_gateway_auto_models", \{ enabled: !autoModels \}/, "开关在模型调度页");
});

test("模型按钮：自动切换的模型带「自动」标签与专用图标；Codex 目录侧同样拆开（后端测试覆盖）", () => {
  const ui = readFileSync(new URL("./gateway-ui.tsx", import.meta.url), "utf8");
  assert.match(ui, /export function ModelViewGlyph/);
  assert.match(ui, /export function AutoTag/);
  for (const file of ["./GatewayAgents.tsx", "./TrayPanel.tsx"]) {
    assert.match(readFileSync(new URL(file, import.meta.url), "utf8"), /<AutoTag view=\{display\} \/>/, file);
  }
});
