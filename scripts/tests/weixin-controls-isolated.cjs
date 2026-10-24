"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { testing } = require("./weixin-media-isolated.cjs");

function read(file) {
  return JSON.parse(fs.readFileSync(file, "utf8").replace(/^\uFEFF/, ""));
}

module.exports = {
  async prepare({ paths }) {
    const file = path.join(paths.state, "settings.json");
    const settings = read(file);
    Object.assign(settings, {
      weixinConnectEnabled: false,
      weixinConnectModel: "retired-weixin-model",
      weixinConnectReasoningEffort: "retired-effort",
      weixinConnectProjectId: "",
      weixinConnectWorkDir: paths.runtime,
      weixinConnectCodexPath: process.env.CODEX_PLUS_TEST_CLI || "",
    });
    settings.relayProfiles[0].model = "gpt-6.1-sol";
    settings.relayProfiles[0].modelList = "gpt-6.1-sol\ngpt-6-sol";
    fs.writeFileSync(file, JSON.stringify(settings, null, 2));
  },
  async run(context) {
    const { managerPage: page, invoke, dialogs, outputDir, check, signal } = context;
    await testing.installManagerUpdateHandling(context);
    await dialogs.manager.ensureClear();
    await page.getByRole("button", { name: "微信连接", exact: true }).click();
    await page.locator(".weixin-connect-page").waitFor();
    await dialogs.manager.ensureClear();
    signal.throwIfAborted();
    const field = (name) => page.locator("label.field")
      .filter({ has: page.locator("span").filter({ hasText: new RegExp(`^${name}$`) }) })
      .locator("select");
    const model = field("模型"), effort = field("推理强度"), project = field("项目");
    await page.getByText("当前模型已不在可用模型目录中，请重新选择。", { exact: true }).waitFor();
    assert.equal(await model.inputValue(), "retired-weixin-model");
    assert.equal(await effort.inputValue(), "retired-effort");
    await model.selectOption("gpt-6.1-sol");
    await effort.selectOption("low");
    const catalog = await invoke("weixin_connect_catalog");
    assert(catalog.models.includes("gpt-6.1-sol"));
    const advertised = catalog.modelMetadata["gpt-6.1-sol"].supportedReasoningEfforts.map((e) => e.reasoningEffort);
    const offered = await effort.locator("option").evaluateAll((items) => items.map((o) => o.value));
    assert.deepEqual(offered, ["", ...new Set(advertised)]);
    check("real catalog drives model and reasoning options; stale values remain visible until corrected");
    await project.locator("option").first().waitFor({ state: "attached" });
    assert.equal(await project.locator("option").first().getAttribute("value"), "");
    assert.equal(await project.locator("option").first().innerText(), "微信Bot");
    const before = await invoke("weixin_connect_projects");
    assert.equal(before.reservedProjectId, null);
    assert.equal(before.projects.length, 0);
    await page.getByRole("button", { name: "刷新项目", exact: true }).click();
    await page.getByRole("button", { name: "刷新项目", exact: true }).isEnabled();
    const after = await invoke("weixin_connect_projects");
    assert.equal(after.projects.length, 0, "opening/refreshing settings must not create the lazy project");
    check("reserved Weixin project stays first and project/list has no creation side effects");
    await page.getByRole("button", { name: "保存", exact: true }).click();
    await page.getByText("已保存，下一条微信消息生效。", { exact: true }).waitFor();
    const saved = await invoke("load_settings");
    assert.equal(saved.settings.weixinConnectModel, "gpt-6.1-sol");
    assert.equal(saved.settings.weixinConnectReasoningEffort, "low");
    assert.equal(saved.settings.weixinConnectProjectId, "");
    check("UI save persists selected model, effort, and lazy project identity");
    await project.scrollIntoViewIfNeeded();
    await dialogs.manager.ensureClear();
    await page.screenshot({ path: path.join(outputDir, "weixin-controls.png") });
    await dialogs.manager.ensureClear();
    fs.writeFileSync(path.join(outputDir, "weixin-controls-evidence.json"), JSON.stringify({
      model: saved.settings.weixinConnectModel,
      reasoningEffort: saved.settings.weixinConnectReasoningEffort,
      projectsBeforeChat: after.projects,
      offeredReasoningEfforts: offered,
      nativeProjectRenameVerified: false,
      nativeChatVisibilityVerified: false,
      realWeixinTransportVerified: false,
    }, null, 2));
  },
};
