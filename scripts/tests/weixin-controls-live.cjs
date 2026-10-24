"use strict";

// Explicit live mode: authorized credentials are used only in the isolated runtime.
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const crypto = require("node:crypto");
const { testing } = require("./weixin-media-isolated.cjs");
const read = file => JSON.parse(fs.readFileSync(file, "utf8").replace(/^\uFEFF/, ""));
const write = (file, value) => fs.writeFileSync(file, JSON.stringify(value, null, 2));
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
let current, manifest, stopConfirmed = false;

function phase(name, extra = {}) {
  const value = { phase: name, nonce: manifest.nonce, ...extra };
  write(path.join(current.outputDir, "live-phase.json"), value);
  console.log(JSON.stringify(value));
}

function scrub(label) {
  const result = testing.scrubCredentials(current, label);
  for (const dir of ["runtime", "archived-test-data"]) {
    const state = path.join(current.outputDir, dir, "state");
    if (!fs.existsSync(state)) continue;
    for (const file of fs.readdirSync(state)) {
      if (/^weixin-session-options-.*\.json$/.test(file)) write(path.join(state, file), {});
    }
    const optionsDirectory = path.join(state, "weixin-session-options");
    if (fs.existsSync(optionsDirectory)) {
      for (const entry of fs.readdirSync(optionsDirectory, { withFileTypes: true })) {
        if (entry.isFile() && /^account-[0-9a-f]*\.json$/.test(entry.name)) {
          write(path.join(optionsDirectory, entry.name), {});
        }
      }
    }
  }
  write(path.join(current.outputDir, `credentials-${label}.json`), result);
  assert(result.success, "Credential cleanup failed; inspect scrub evidence");
  return result;
}

function rollouts(root) {
  if (!fs.existsSync(root)) return [];
  return fs.readdirSync(root, { withFileTypes: true }).flatMap(entry => {
    const file = path.join(root, entry.name);
    return entry.isDirectory() ? rollouts(file) : entry.name.endsWith(".jsonl") ? [file] : [];
  });
}

function nativeEvidence() {
  const results = [];
  for (const file of rollouts(path.join(current.paths.home, "sessions"))) {
    const entries = fs.readFileSync(file, "utf8").split(/\r?\n/).filter(Boolean).map(JSON.parse);
    const turns = entries.filter(e => e.type === "turn_context").map(e => ({
      model: e.payload.model, effort: e.payload.effort, sandbox: e.payload.sandbox_policy?.type,
    }));
    const tools = entries.filter(e => e.type === "response_item" &&
      ["function_call", "custom_tool_call", "function_call_output", "custom_tool_call_output"].includes(e.payload?.type))
      .map(e => e.payload).filter(e => /weixin_session_settings/.test(e.name || "") ||
        /weixin_session_settings/.test(e.input || "") ||
        /savedOverrides|appliesFrom/.test(e.output || ""));
    const replies = entries.filter(e => e.type === "response_item" && e.payload?.role === "assistant")
      .flatMap(e => e.payload.content || []).filter(e => e.type === "output_text")
      .map(e => e.text);
    results.push({ file: path.relative(current.outputDir, file), turns, tools, replies });
  }
  return results;
}

module.exports = {
  async prepare(context) {
    assert.equal(process.env.CODEX_PLUS_WEIXIN_LIVE_TEST, "1", "Explicit live test flag required");
    assert.equal(process.env.CODEX_PLUS_WEIXIN_ORIGINAL_STOPPED, "1", "Pause original consumer first");
    current = context;
    process.once("exit", () => {
      try {
        scrub("after-exit");
        const reportPath = path.join(current.outputDir, "report.json");
        if (fs.existsSync(reportPath)) {
          const report = read(reportPath);
          report.checks = report.checks.map(text => text === "scenario completed without model generation"
            ? "explicitly authorized real Weixin and real model generation completed" : text);
          report.liveWeixin = { nonce: manifest?.nonce, stopConfirmed, modelTraffic: "real-authorized" };
          write(reportPath, report);
        }
      } catch { process.exitCode = 1; }
    });
    try {
      const source = await testing.readAuthorizedSource(process.env, "prepare");
      assert.equal(source.settings.weixinConnectSandbox, "danger-full-access", "Preserve actual permissions");
      const catalog = read(process.env.CODEX_PLUS_WEIXIN_TEST_CATALOG);
      assert(catalog.models.some(model => model.slug === "gpt-6-sol"));
      assert(catalog.models.some(model => model.slug === "gpt-6.1-sol"));
      const catalogPath = path.join(context.paths.home, "live-model-catalog.json");
      write(catalogPath, catalog);
      const workspace = path.join(context.paths.runtime, "workspace");
      fs.mkdirSync(workspace);
      testing.seedBoundary(context, source);
      const settingsPath = path.join(context.paths.state, "settings.json");
      const settings = read(settingsPath);
      for (const key of ["weixinConnectBaseUrl", "weixinConnectToken", "weixinConnectAccountId",
        "weixinConnectAllowFrom", "weixinConnectRouteTag", "weixinConnectSandbox"]) settings[key] = source.settings[key];
      Object.assign(settings, {
        relayProfilesEnabled: false, enhancementsEnabled: false, weixinConnectEnabled: false,
        weixinConnectWorkDir: workspace, weixinConnectModel: "gpt-6.1-sol",
        weixinConnectReasoningEffort: "medium", weixinConnectProjectId: "",
        weixinConnectCodexPath: process.env.CODEX_PLUS_WEIXIN_TEST_CLI,
      });
      settings.relayProfiles[0].model = "gpt-6.1-sol";
      settings.relayProfiles[0].modelList = catalog.models.map(model => model.slug).join("\n");
      write(settingsPath, settings);
      assert(source.model?.base && source.model?.key, "Authorized model routing missing");
      fs.writeFileSync(path.join(context.paths.home, "config.toml"), [
        'model="gpt-6.1-sol"', 'model_reasoning_effort="medium"', 'model_provider="live_weixin"',
        `model_catalog_json=${JSON.stringify(catalogPath)}`, 'approval_policy="never"',
        'sandbox_mode="danger-full-access"', 'check_for_update_on_startup=false',
        '[features]', 'plugins=false', 'remote_plugin=false', 'recommended_plugins=false',
        'hooks=false', 'apps=false', 'shell_snapshot=false', 'api_key_model_discovery=false',
        '[model_providers.live_weixin]', 'name="Authorized Weixin acceptance"',
        `base_url=${JSON.stringify(source.model.base)}`, 'wire_api="responses"',
        'requires_openai_auth=true', 'supports_websockets=false', "",
      ].join("\n"));
      write(path.join(context.paths.home, "auth.json"), { OPENAI_API_KEY: source.model.key });
      manifest = {
        nonce: crypto.randomBytes(6).toString("hex"), outputDir: context.outputDir,
        runtime: context.paths.runtime, sandbox: settings.weixinConnectSandbox,
        defaults: { model: "gpt-6.1-sol", effort: "medium" },
        target: { model: "gpt-6-sol", effort: "low" }, realWeixin: true, realModel: true,
      };
      write(path.join(context.outputDir, "live-manifest.json"), manifest);
    } catch (error) {
      scrub("prepare-failed");
      throw error;
    }
  },
  async run(context) {
    assert.equal(current, context);
    try {
      await testing.installManagerUpdateHandling(context);
      await context.dialogs.manager.ensureClear();
      await context.managerPage.getByRole("button", { name: "微信连接", exact: true }).click();
      await context.managerPage.locator(".weixin-connect-page").waitFor();
      await context.dialogs.manager.ensureClear();
      const before = await context.invoke("weixin_connect_projects");
      assert.equal(before.projects.length, 0, "Reserved project must remain lazy before first message");
      context.check("real Weixin setup retains full access and lazy project has not been created");
      assert.equal((await context.invoke("weixin_connect_start")).status, "ok");
      phase("running", { outputDir: context.outputDir });
      const end = Date.now() + 480000;
      let count = -1;
      while (Date.now() < end) {
        context.signal.throwIfAborted();
        const status = await context.invoke("weixin_connect_status");
        assert.equal(status.status, "ok");
        if (status.processedMessages !== count) {
          count = status.processedMessages;
          write(path.join(context.outputDir, "native-evidence.json"), nativeEvidence());
          const projects = await context.invoke("weixin_connect_projects");
          write(path.join(context.outputDir, "live-projects.json"), projects);
          phase("running", { processed: count, state: status.state });
        }
        if (status.state === "retrying") phase("retrying", { message: status.message });
        if (fs.existsSync(path.join(context.outputDir, "finish-live.json"))) {
          assert(count >= 2, "Need actual remote change and subsequent Weixin message");
          const evidence = nativeEvidence();
          const turns = evidence.flatMap(e => e.turns);
          assert(turns.some(t => t.model === "gpt-6.1-sol" && t.effort === "medium"));
          assert(turns.some(t => t.model === "gpt-6-sol" && t.effort === "low"));
          assert(turns.every(t => t.sandbox === "danger-full-access"));
          write(path.join(context.outputDir, "native-evidence.json"), evidence);
          const projects = await context.invoke("weixin_connect_projects");
          assert(projects.reservedProjectId && projects.projects.length === 1);
          write(path.join(context.outputDir, "live-projects.json"), projects);
          await context.dialogs.manager.ensureClear();
          await context.managerPage.screenshot({ path: path.join(context.outputDir, "live-manager.png") });
          context.check("real Weixin messages changed the next native turn to gpt-6-sol low");
          return;
        }
        await pause(1000);
      }
      throw new Error("Live test deadline exceeded");
    } finally {
      try {
        const stopped = await testing.confirmStopped(context.invoke, context.signal);
        stopConfirmed = stopped.confirmed;
        phase("stopped");
        const files = fs.readdirSync(context.paths.state).filter(file => /^weixin-connect-state.*\.json$/.test(file));
        for (const file of files) {
          const value = read(path.join(context.paths.state, file));
          write(path.join(context.outputDir, "tested-message-keys.json"), value.processedMessageKeys);
        }
      } finally {
        // 取消时 invoke 可能失效；退出处理器会在 runner 收尾后再次脱敏。
        if (stopConfirmed) scrub("run-finally");
      }
    }
  },
};
