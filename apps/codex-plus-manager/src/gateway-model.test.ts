import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";
import {
  bindingFromDraft,
  blankBinding,
  draftFromBinding,
  bindingDisplay,
  formatRemaining,
  gatewayPresets,
  groupSlug,
  memberStatus,
  parseContextWindow,
  pickerSections,
  presetAdded,
  presetHost,
  providerDraftFromPreset,
  routeCards,
  routeMembers,
  routeNarrative,
  parseTargetValue,
  splitModels,
  targetModels,
  targetValue,
  validateDraft,
  validateProviderDraft,
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
      { id: "ds", name: "DeepSeek", baseUrl: "https://api.deepseek.com/", protocol: "chatCompletions", official: false, models: ["deepseek-v4-pro", "deepseek-v4-flash"], keyCount: 2 },
      { id: "glm", name: "GLM", baseUrl: "https://glm.test/v1", protocol: "anthropicMessages", official: false, models: ["glm-5.3"], keyCount: 1 },
      { id: "bare", name: "Bare", baseUrl: "https://bare.test/v1", protocol: "responses", official: false, models: [], keyCount: 1 },
    ],
    codexProviders: [],
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

const initials = (name: string) => name.slice(0, 2).toUpperCase();

test("模型选择器：路由组在前，每个供应商一组；可按名字筛选，未知模型给出自定义项", () => {
  const all = pickerSections(payload(), "", initials);
  assert.deepEqual(all.map((section) => section.key), ["group", "provider:ds", "provider:glm"]);
  assert.deepEqual(all[0].items[0], { target: "group:g", model: "", label: "Group", sub: "group/group", avatar: "GR" });
  assert.deepEqual(all[1].items.map((item) => item.model), ["deepseek-v4-pro", "deepseek-v4-flash"]);
  // 供应商名命中时列出它的全部模型；模型名命中时只列命中的。
  assert.deepEqual(pickerSections(payload(), "deep", initials).map((section) => section.key), ["provider:ds", "custom"]);
  assert.deepEqual(pickerSections(payload(), "flash", initials)[0].items.map((item) => item.model), ["deepseek-v4-flash"]);
  const custom = pickerSections(payload(), "kimi-k3", initials);
  assert.equal(custom.at(-1)?.key, "custom");
  assert.ok(custom.at(-1)?.items.every((item) => item.model === "kimi-k3" && item.custom));
  assert.equal(pickerSections(payload(), "deepseek-v4-pro", initials).some((section) => section.key === "custom"), false);
});

test("选择器按钮：供应商显示模型名，路由组显示组名", () => {
  assert.deepEqual(bindingDisplay(payload(), "provider:ds", ""), { title: "deepseek-v4-pro", sub: "DeepSeek", kind: "provider" });
  assert.deepEqual(bindingDisplay(payload(), "provider:ds", "deepseek-v4-flash"), { title: "deepseek-v4-flash", sub: "DeepSeek", kind: "provider" });
  assert.deepEqual(bindingDisplay(payload(), "group:g", ""), { title: "Group", sub: "group/group", kind: "group" });
  assert.equal(bindingDisplay(payload(), "", "").kind, null);
});

function withActivity(): GatewayPayload {
  const data = payload();
  data.gateway.bindings = {
    claude: { ...blankBinding(), enabled: true, target: { kind: "group", id: "g" } },
    grok: { ...blankBinding(), enabled: true, target: { kind: "group", id: "g" } },
  };
  data.agents = [
    { agent: "claude", name: "Claude Code", enabled: true, baseUrl: "", defaultModel: "", models: [] },
    { agent: "grok", name: "Grok", enabled: true, baseUrl: "", defaultModel: "", models: [] },
  ];
  data.activity = {
    agents: [{
      agent: "claude",
      requests: 3,
      rerouted: 1,
      errors: 0,
      inFlight: [],
      last: {
        requestedModel: "claude-sonnet-5",
        model: "deepseek-v4-pro",
        groupId: "g",
        planned: [{ relayId: "ds", relayName: "DeepSeek", model: "deepseek-v4-pro" }, { relayId: "glm", relayName: "GLM", model: "glm-5.3" }],
        steps: [
          { relayId: "ds", relayName: "DeepSeek", keyId: "one", model: "deepseek-v4-pro", outcome: "failed", failure: "rate_limited", status: 429, elapsedMs: 100 },
          { relayId: "glm", relayName: "GLM", keyId: "default", model: "glm-5.3", outcome: "answered", status: 200, elapsedMs: 900 },
        ],
        result: "answered",
        finishedAtMs: 1,
        elapsedMs: 1200,
      },
    }],
  };
  return data;
}

test("成员状态：正在回答 > 熔断 / Key 全部冷却 > 最近回答了谁 > Key 数", () => {
  const data = withActivity();
  assert.deepEqual(memberStatus(data, "glm"), { state: "answered", text: "回答了 {0}", args: ["Claude Code"] });
  assert.deepEqual(memberStatus(data, "ds"), { state: "idle", text: "{0} 个 Key", args: [2] });
  data.health.keys = [{ relayId: "ds", keyId: "one", state: "cooling", remainingMs: 35_000, lastFailure: "rate_limited" }];
  assert.deepEqual(memberStatus(data, "ds"), { state: "idle", text: "{0} 个 Key · {1} 个冷却中", args: [2, 1] });
  data.health.keys.push({ relayId: "ds", keyId: "two", state: "cooling", remainingMs: 20_000, lastFailure: "rate_limited" });
  assert.deepEqual(memberStatus(data, "ds"), { state: "cooling", text: "被限流 · {0} 后恢复", args: ["20s"] });
  data.health.providers = [{ relayId: "glm", breakerOpen: true, remainingMs: 60_000, consecutiveErrors: 3 }];
  assert.equal(memberStatus(data, "glm").state, "cooling");
  data.activity!.agents[0].inFlight = ["glm"];
  assert.deepEqual(memberStatus(data, "glm"), { state: "busy", text: "正在回答…", args: [] });
  assert.deepEqual(memberStatus(payload(), "bare"), { state: "idle", text: "一个 Key", args: [] });
});

test("路由卡片：同一目标的 Agent 合在一起；成员模型取最近一次计划", () => {
  const data = withActivity();
  assert.deepEqual(routeCards(data), [{ target: "group:g", agents: ["claude", "grok"] }]);
  const records = data.activity!.agents.map((agent) => agent.last!).filter(Boolean);
  assert.deepEqual(routeMembers(data, "group:g", records), [
    { relayId: "ds", name: "DeepSeek", model: "deepseek-v4-pro" },
    { relayId: "glm", name: "GLM", model: "glm-5.3" },
  ]);
  assert.deepEqual(routeMembers(data, "provider:bare", []), [{ relayId: "bare", name: "Bare", model: "" }]);
});

test("上一个请求的路由说明：先列计划，再逐个尝试，最后谁回答", () => {
  const data = withActivity();
  const lines = routeNarrative("claude", data.activity!.agents[0].last!, data);
  assert.deepEqual(lines.map((line) => line.tone), ["muted", "step", "step", "ok"]);
  assert.deepEqual(lines[0].args, ["Claude Code", "claude-sonnet-5", "Group", "DeepSeek/deepseek-v4-pro, GLM/glm-5.3"]);
  assert.deepEqual(lines[2].args, ["DeepSeek", "one", "被限流", " · HTTP 429"]);
  assert.deepEqual(lines[3].args, ["GLM", "glm-5.3", "1.2 s"]);
});

test("路由组短名按组名生成，不显示随机 id", () => {
  assert.equal(groupSlug("Daily coding"), "group/daily-coding");
  assert.equal(groupSlug(" 系统默认 "), "group/系统默认");
  assert.equal(groupSlug(""), "group/unnamed");
});

test("内置预设：厂商在前、中转其次、本机最后；地址相同的标记为已添加", () => {
  const presets = gatewayPresets();
  const sections = presets.map((preset) => preset.section);
  assert.equal(sections.indexOf("relay") > sections.lastIndexOf("vendor"), true);
  assert.equal(sections.indexOf("local") > sections.lastIndexOf("relay"), true);
  const anthropic = presets.find((preset) => preset.key === "anthropic");
  assert.equal(anthropic?.protocol, "anthropicMessages");
  const deepseek = presets.find((preset) => preset.key === "deepseek")!;
  assert.equal(deepseek.section, "vendor");
  assert.equal(presetAdded(payload(), deepseek), true, "末尾斜杠和大小写不影响判断");
  assert.equal(presetAdded(payload(), anthropic!), false);
  assert.equal(new Set(presets.map((preset) => preset.key)).size, presets.length, "预设 key 不重复");
  assert.equal(presetHost("https://api.deepseek.com/"), "api.deepseek.com");
  assert.equal(presetHost("http://127.0.0.1:11434/v1"), "localhost:11434");
});

test("预设表单：本机服务预填 Key；校验名称、地址、Key 和模型", () => {
  const ollama = gatewayPresets().find((preset) => preset.key === "ollama")!;
  const draft = providerDraftFromPreset(ollama);
  assert.equal(draft.apiKey, "ollama");
  assert.equal(validateProviderDraft(draft), "至少填一个模型");
  assert.equal(validateProviderDraft({ ...draft, model: "qwen3" }), null);
  assert.equal(validateProviderDraft({ ...draft, model: "qwen3", baseUrl: "api.x" }), "地址要以 http:// 或 https:// 开头");
  assert.equal(validateProviderDraft({ ...draft, model: "qwen3", apiKey: " " }), "请填写 API Key");
  assert.equal(validateProviderDraft(providerDraftFromPreset(null)), "请填写名称");
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
    "add_gateway_pool_providers",
    "remove_gateway_pool_provider",
    "create_gateway_pool_provider",
  ]) {
    assert.match(lib, new RegExp(`gateway_commands::${command},`));
  }
});
