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
    await page.getByText("微信连接", { exact: true }).first().click();
    await page.locator(".weixin-connect-page").waitFor();
    await dialogs.manager.ensureClear();
    await page.locator(".weixin-connect-page").getByRole("button", { name: "会话管理", exact: true }).click();
    signal.throwIfAborted();
    const field = (name) => page.locator("label.field")
      .filter({ has: page.locator("span").filter({ hasText: new RegExp(`^${name}$`) }) })
      .locator("input");
    const model = field("模型"), effort = field("推理强度");
    assert.equal(await model.inputValue(), "retired-weixin-model");
    assert.equal(await effort.inputValue(), "retired-effort");
    await model.fill("gpt-6.1-sol");
    await effort.fill("low");
    const catalog = await invoke("weixin_connect_catalog");
    assert(catalog.models.includes("gpt-6.1-sol"));
    check("real catalog is available while model and reasoning fields accept the next-message settings");
    const before = await invoke("weixin_connect_projects");
    assert.equal(before.reservedProjectId, null);
    assert.equal(before.projects.length, 0);
    const after = await invoke("weixin_connect_projects");
    assert.equal(after.projects.length, 0, "opening/refreshing settings must not create the lazy project");
    check("reserved Weixin project stays first and project/list has no creation side effects");
    // 微信页使用通用 SettingsAutoSave；不点击旧的手动保存入口，等待防抖保存
    // 完成后直接从后端读取，验证下一次打开页面仍保留结果。
    let saved;
    const deadline = Date.now() + 10000;
    do {
      signal.throwIfAborted();
      saved = await invoke("load_settings");
      if (saved.settings.weixinConnectModel === "gpt-6.1-sol"
        && saved.settings.weixinConnectReasoningEffort === "low") break;
      await page.waitForTimeout(250);
    } while (Date.now() < deadline);
    assert.equal(saved.settings.weixinConnectModel, "gpt-6.1-sol");
    assert.equal(saved.settings.weixinConnectReasoningEffort, "low");
    assert.equal(saved.settings.weixinConnectProjectId, "");
    check("Weixin settings auto-save persists model, effort, and lazy project identity");
    await page.getByRole("button", { name: "会话管理", exact: true }).click();
    await dialogs.manager.ensureClear();
    await page.getByText("微信连接", { exact: true }).first().click();
    await page.locator(".weixin-connect-page").waitFor();
    await dialogs.manager.ensureClear();
    await page.locator(".weixin-connect-page").getByRole("button", { name: "会话管理", exact: true }).click();
    const reopenedModel = field("模型");
    const reopenedEffort = field("推理强度");
    assert.equal(await reopenedModel.inputValue(), "gpt-6.1-sol");
    assert.equal(await reopenedEffort.inputValue(), "low");
    check("Weixin settings remain selected after leaving and reopening the page");
    await dialogs.manager.ensureClear();
    await page.screenshot({ path: path.join(outputDir, "weixin-controls.png") });
    await dialogs.manager.ensureClear();
    fs.writeFileSync(path.join(outputDir, "weixin-controls-evidence.json"), JSON.stringify({
      model: saved.settings.weixinConnectModel,
      reasoningEffort: saved.settings.weixinConnectReasoningEffort,
      projectsBeforeChat: after.projects,
      offeredReasoningEfforts: [],
      nativeProjectRenameVerified: false,
      nativeChatVisibilityVerified: false,
      realWeixinTransportVerified: false,
    }, null, 2));
  },
};
