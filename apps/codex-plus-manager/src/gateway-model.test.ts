import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  bindingFromDraft,
  blankBinding,
  draftFromBinding,
  formatRemaining,
  parseContextWindow,
  parseTargetValue,
  splitModels,
  targetModels,
  targetValue,
  validateDraft,
  type GatewayPayload,
} from "./gateway-model.ts";

const app = readFileSync(new URL("./App.tsx", import.meta.url), "utf8");
const lib = readFileSync(new URL("../src-tauri/src/lib.rs", import.meta.url), "utf8");

function payload(): GatewayPayload {
  return {
    gateway: { version: 1, bindings: {}, keyPolicy: { mode: "sticky" }, restoreOnExit: false },
    port: 57321,
    helperRunning: true,
    pool: [
      { id: "ds", name: "DeepSeek", protocol: "chatCompletions", official: false, models: ["deepseek-v4-pro", "deepseek-v4-flash"], keyCount: 2 },
      { id: "glm", name: "GLM", protocol: "anthropicMessages", official: false, models: ["glm-5.3"], keyCount: 1 },
      { id: "bare", name: "Bare", protocol: "responses", official: false, models: [], keyCount: 1 },
    ],
    groups: [{ id: "g", name: "Group", strategy: "failover", members: ["ds", "glm"] }],
    agents: [],
    health: { keys: [], providers: [] },
    claudeProviders: [],
    grokProfiles: [],
  };
}

test("目标值在下拉框字符串与结构之间往返", () => {
  assert.equal(targetValue({ kind: "group", id: "g" }), "group:g");
  assert.deepEqual(parseTargetValue("provider:ds"), { kind: "provider", id: "ds" });
  assert.deepEqual(parseTargetValue("provider:a:b"), { kind: "provider", id: "a:b" });
  assert.equal(parseTargetValue("other:x"), null);
  assert.equal(parseTargetValue("provider:"), null);
  assert.equal(targetValue(blankBinding().target), "");
});

test("上下文窗口支持 k / M 写法", () => {
  assert.equal(parseContextWindow(""), null);
  assert.equal(parseContextWindow("200000"), 200000);
  assert.equal(parseContextWindow("200k"), 200000);
  assert.equal(parseContextWindow("1M"), 1000000);
  assert.ok(Number.isNaN(parseContextWindow("abc") as number));
});

test("表单与绑定互转：多行模型去重，token 与恢复现场沿用旧值", () => {
  const previous = { ...blankBinding(), enabled: true, token: "cpx-gw-claude-x", restore: { claudePreviousProviderId: "kimi" } };
  const draft = { ...draftFromBinding(previous), target: "group:g", model: " deepseek-v4-pro ", extraModels: "glm-5.3\nglm-5.3, deepseek-v4-flash", haiku: "deepseek-v4-flash", contextWindow: "1M" };
  const binding = bindingFromDraft(draft, previous);
  assert.deepEqual(binding.target, { kind: "group", id: "g" });
  assert.equal(binding.model, "deepseek-v4-pro");
  assert.deepEqual(binding.extraModels, ["glm-5.3", "deepseek-v4-flash"]);
  assert.equal(binding.claudeRoles.haiku, "deepseek-v4-flash");
  assert.equal(binding.contextWindow, 1000000);
  assert.equal(binding.token, "cpx-gw-claude-x");
  assert.equal(binding.restore?.claudePreviousProviderId, "kimi");
  assert.deepEqual(splitModels(" a \n\n b,a"), ["a", "b"]);
});

test("校验：必须选目标、目标存在、没有默认模型时要填主模型、窗口格式正确", () => {
  const data = payload();
  const base = draftFromBinding(undefined);
  assert.equal(validateDraft(base, data), "请选择供应商或路由组");
  assert.equal(validateDraft({ ...base, target: "provider:gone" }, data), "所选供应商或路由组已不存在");
  assert.equal(validateDraft({ ...base, target: "provider:bare" }, data), "该供应商没有默认模型，请填写主模型");
  assert.equal(validateDraft({ ...base, target: "provider:bare", model: "x" }, data), null);
  assert.equal(validateDraft({ ...base, target: "provider:ds", contextWindow: "12" }, data), "上下文窗口格式不对，例如 200000、200k 或 1M");
  assert.equal(validateDraft({ ...base, target: "group:g" }, data), null);
});

test("路由组的可选模型是成员模型的并集", () => {
  assert.deepEqual(targetModels(payload(), "group:g"), ["deepseek-v4-pro", "deepseek-v4-flash", "glm-5.3"]);
  assert.deepEqual(targetModels(payload(), "provider:glm"), ["glm-5.3"]);
  assert.deepEqual(targetModels(null, "provider:glm"), []);
});

test("冷却剩余时间显示", () => {
  assert.equal(formatRemaining(0), "");
  assert.equal(formatRemaining(1500), "2s");
  assert.equal(formatRemaining(90_000), "2m");
  assert.equal(formatRemaining(3_900_000), "1h5m");
});

test("网关页接入导航，命令已注册", () => {
  assert.match(app, /id: "gateway"/);
  assert.match(app, /route === "gateway" \? \(/);
  for (const command of [
    "load_gateway",
    "save_gateway_binding",
    "disable_gateway_binding",
    "rotate_gateway_token",
    "save_gateway_options",
    "import_claude_provider_to_gateway_pool",
    "import_grok_profile_to_gateway_pool",
    "reset_gateway_key_health",
  ]) {
    assert.match(lib, new RegExp(`gateway_commands::${command},`));
  }
});
