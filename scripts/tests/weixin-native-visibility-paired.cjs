"use strict";

const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const { DatabaseSync, backup } = require("node:sqlite");
const { testing } = require("./weixin-media-isolated.cjs");

const read = (file) => JSON.parse(fs.readFileSync(file, "utf8").replace(/^\uFEFF/, ""));
const write = (file, data) => fs.writeFileSync(file, JSON.stringify(data, null, 2));
const contained = (root, target) => {
  const rel = path.relative(root, target);
  return rel && rel !== ".." && !rel.startsWith(".." + path.sep) && !path.isAbsolute(rel);
};

module.exports = {
  async prepare(context) {
    const { paths, report, outputDir } = context;
    const seed = path.resolve(process.env.CODEX_PLUS_NATIVE_VISIBILITY_SEED || "");
    const cli = process.env.CODEX_PLUS_NATIVE_VISIBILITY_CLI;
    assert(seed && cli && fs.existsSync(cli), "Explicit synthetic seed and CLI required");
    assert(contained(path.join(context.repo, "output/weixin-session-controls"), seed), "Seed must be a task output");
    const manifest = read(path.join(seed, "seed-report.json"));
    assert(manifest.success && manifest.syntheticOnly && manifest.credentialSource === "NONE");
    const evidence = read(path.join(seed, "evidence.json"));
    assert(evidence.localMocksOnly && evidence.nativeDbIndexed && evidence.nativeHistoryReadable);
    const root = path.join(paths.state, "codex-multi", report.instanceId);
    const home = path.join(root, "codex-home"), sqlite = path.join(root, "sqlite-home");
    const work = path.join(paths.runtime, "synthetic-weixin-workspace");
    for (const dir of [home, sqlite, work]) fs.mkdirSync(dir, { recursive: true });
    // 组内管理器和 paired Codex 共用合成 home/DB，以模拟正式共享数据布局；
    // 两个 junction 的目标都在本组 runtime，绝不连接正式目录。
    const bootstrapHome = paths.home + "-bootstrap", bootstrapSqlite = paths.sqlite + "-bootstrap";
    fs.renameSync(paths.home, bootstrapHome);
    fs.renameSync(paths.sqlite, bootstrapSqlite);
    for (const name of ["config.toml", "auth.json"]) fs.copyFileSync(path.join(bootstrapHome, name), path.join(home, name));
    fs.symlinkSync(home, paths.home, "junction");
    fs.symlinkSync(sqlite, paths.sqlite, "junction");
    assert(contained(paths.runtime, fs.realpathSync(paths.home)));
    assert(contained(paths.runtime, fs.realpathSync(paths.sqlite)));
    const source = new DatabaseSync(path.join(seed, "sqlite/state_5.sqlite"), { readOnly: true });
    const destination = path.join(sqlite, "state_5.sqlite");
    try { await backup(source, destination); } finally { source.close(); }
    const db = new DatabaseSync(destination);
    const rows = db.prepare("SELECT id,rollout_path,cwd FROM threads").all();
    const title = "Synthetic Weixin native visibility";
    try {
      db.exec("BEGIN");
      for (const row of rows) {
        const sourceFile = path.resolve(row.rollout_path);
        assert(contained(seed, sourceFile), "Rollout must be synthetic");
        const relative = path.relative(path.join(seed, "home"), sourceFile);
        const target = path.join(home, relative);
        assert(contained(home, target));
        fs.mkdirSync(path.dirname(target), { recursive: true });
        const lines = fs.readFileSync(sourceFile, "utf8").split("\n").filter(Boolean).map((line) => {
          const value = JSON.parse(line);
          const replacePaths = (item) => {
            if (!item || typeof item !== "object") return;
            for (const [key, val] of Object.entries(item)) {
              if (typeof val === "string" && /^(cwd|path)$/.test(key) &&
                val.replace(/^\\\\\?\\/, "").startsWith(path.join(seed, "work"))) item[key] = work;
              else if (typeof val === "object") replacePaths(val);
              else if (typeof val === "string" && key === "runtime_workspace_roots") item[key] = work;
            }
            for (const key of ["runtime_workspace_roots", "runtimeWorkspaceRoots"]) {
              if (Array.isArray(item[key])) item[key] = item[key].map(() => work);
            }
          };
          replacePaths(value);
          return JSON.stringify(value);
        });
        fs.writeFileSync(target, lines.join("\n") + "\n");
        db.prepare("UPDATE threads SET rollout_path=?,cwd=? WHERE id=?").run(target, work, row.id);
      }
      db.prepare("UPDATE project_roots SET path=? WHERE project_id=?").run(work, evidence.projectId);
      db.prepare("UPDATE threads SET title=?,name=? WHERE id=?").run(title, title, evidence.threadId);
      db.exec("COMMIT");
    } catch (e) { db.exec("ROLLBACK"); throw e; }
    finally { db.close(); }
    write(path.join(home, ".codex-global-state.json"), {
      "electron-saved-workspace-roots": [],
      "active-workspace-roots": [],
      // 桌面版自己的项目缓存不自动收录外部 app-server 创建的项目。
      // 仅为原生重命名 UI 场景预置缓存，ID 对应真实 seed 项目，不导入旧路径项目。
      "local-projects": {
        [evidence.projectId]: {
          id: evidence.projectId, name: evidence.projectName, rootPaths: [work],
          createdAt: Date.now(), updatedAt: Date.now(),
        },
      },
      "project-order": [evidence.projectId],
    });
    fs.writeFileSync(path.join(home, "session_index.jsonl"), JSON.stringify({
      id: evidence.threadId, thread_name: title, updated_at: new Date().toISOString(),
    }) + "\n");
    const settingsFile = path.join(paths.state, "settings.json");
    const settings = read(settingsFile);
    const features = "\n[features]\nplugins=false\nremote_plugin=false\nhooks=false\napps=false\nshell_snapshot=false\napi_key_model_discovery=false\n";
    // 保留 seed 的历史 provider ID；仅把本组假配置的名称对齐，端点仍是本地拒绝生成的 mock。
    settings.relayProfiles[0].configContents = settings.relayProfiles[0].configContents
      .replace('model_provider="isolated"', 'model_provider="local_weixin_test"')
      .replace("[model_providers.isolated]", "[model_providers.local_weixin_test]");
    settings.relayProfiles[0].configContents += features;
    fs.writeFileSync(path.join(home, "config.toml"), settings.relayProfiles[0].configContents);
    settings.relayProfiles[0].modelList = "isolated-model\ngpt-6.1-sol\ngpt-6-sol";
    Object.assign(settings, {
      weixinConnectEnabled: false, weixinConnectBaseUrl: "http://127.0.0.1:9",
      weixinConnectToken: "", weixinConnectAccountId: "", weixinConnectAllowFrom: "",
      weixinConnectCodexPath: cli, weixinConnectWorkDir: work,
      weixinConnectProjectId: "", weixinConnectModel: "isolated-model",
      weixinConnectReasoningEffort: "", localConfigProviderImported: true,
    });
    write(settingsFile, settings);
    context.nativeSeed = { seed, ...evidence, title, work, home, sqlite, root };
    write(path.join(outputDir, "native-visibility-prepare.json"), {
      ...context.nativeSeed, source: "SYNTHETIC_LOCAL_FIXTURE",
      databaseRebased: true, sharedWithinThisTestGroupOnly: true,
      copiedProductionCredentials: false, weixinServiceEnabled: false,
      nativeProjectCacheSeededForRenameUi: true,
    });
  },
  async run(context) {
    const { managerPage, codexPage, invoke, dialogs, outputDir, check, signal, nativeSeed } = context;
    managerPage.setDefaultTimeout(15000);
    codexPage.setDefaultTimeout(15000);
    await testing.installManagerUpdateHandling(context);
    await dialogs.manager.ensureClear();
    await dialogs.codex.ensureClear({ settleMs: 2000 });
    const evidence = { syntheticOnly: true, realWeixinE2E: false,
      threadId: nativeSeed.threadId, projectId: nativeSeed.projectId, checks: [] };
    const save = () => write(path.join(outputDir, "native-visibility-evidence.json"), evidence);
    const screenshot = async (page, which, name) => {
      await dialogs[which].ensureClear();
      await page.screenshot({ path: path.join(outputDir, name) });
      await dialogs[which].ensureClear();
    };
    try {
      signal.throwIfAborted();
      evidence.initialNative = await codexPage.evaluate(() => ({
        url: location.href, text: document.body.innerText.slice(0, 18000),
        anchors: [...document.querySelectorAll("a[href]")].map((a) => ({ text: a.textContent?.trim(), href: a.getAttribute("href") })).slice(0, 100),
        buttons: [...document.querySelectorAll("button")].map((b) => ({ text: b.textContent?.trim(), label: b.getAttribute("aria-label") })).slice(0, 100),
      }));
      save();
      await screenshot(codexPage, "codex", "native-initial.png");
      const thread = codexPage.getByText(nativeSeed.title, { exact: true }).first();
      await thread.waitFor({ state: "visible", timeout: 20000 });
      evidence.sidebarVisible = true;
      evidence.sidebarTitle = await thread.innerText();
      await thread.click();
      await dialogs.codex.ensureClear();
      await codexPage.getByText("LOCAL_WEIXIN_REPLY_2", { exact: true }).waitFor({ state: "visible", timeout: 15000 });
      evidence.historyVisible = true;
      evidence.threadOpenedUrl = codexPage.url();
      evidence.nativeThreadRows = await codexPage.locator("[data-app-action-sidebar-thread-id]")
        .evaluateAll((rows) => rows.map((row) => ({
          attributes: Object.fromEntries([...row.attributes].filter((a) => a.name.startsWith("data-app-action")).map((a) => [a.name, a.value])),
          text: row.innerText,
        })));
      await codexPage.locator("main").getByRole("button", { name: nativeSeed.title, exact: true }).waitFor();
      check("synthetic Weixin thread visible in native sidebar and historical mock reply opened");
      evidence.checks.push("native thread visible and openable");
      save();
      await screenshot(codexPage, "codex", "native-thread-opened.png");
      await managerPage.getByRole("button", { name: "微信连接", exact: true }).click();
      await managerPage.locator(".weixin-connect-page").waitFor();
      await dialogs.manager.ensureClear();
      const project = managerPage.locator("label.field")
        .filter({ has: managerPage.locator("span").filter({ hasText: /^项目$/ }) }).locator("select");
      const listed = await invoke("weixin_connect_projects");
      assert.equal(listed.reservedProjectId, nativeSeed.projectId);
      evidence.managerBefore = listed;
      const first = project.locator("option").first();
      await managerPage.waitForFunction((name) => [...document.querySelectorAll(".weixin-connect-page select option")]
        .some((o) => o.value === "" && o.textContent === name), nativeSeed.projectName, { timeout: 15000 });
      assert.equal(await first.getAttribute("value"), "");
      assert.equal(await first.innerText(), nativeSeed.projectName);
      evidence.managerFirstOptionBefore = await first.innerText();
      save();
      await screenshot(managerPage, "manager", "manager-project-before.png");
      await managerPage.getByRole("button", { name: "会话管理", exact: true }).click();
      await dialogs.manager.ensureClear();
      const sessionSearch = managerPage.getByPlaceholder("搜索标题、目录或会话 ID", { exact: true });
      if (await sessionSearch.isVisible()) await sessionSearch.fill(nativeSeed.threadId);
      const managerSession = managerPage.locator(".session-row, .mg-session-row").first();
      await managerSession.waitFor();
      if (await sessionSearch.isVisible()) {
        assert.equal(await managerPage.locator(".mg-session-row").count(), 1);
        await managerSession.locator(".mg-session-main").click();
        const details = managerPage.getByRole("dialog");
        await details.getByText(nativeSeed.threadId, { exact: true }).waitFor();
        evidence.managerSessionDetail = await details.innerText();
        await managerPage.screenshot({ path: path.join(outputDir, "manager-session-detail.png") });
        await details.getByRole("button", { name: "关闭窗口", exact: true }).click();
        await details.waitFor({ state: "hidden" });
      } else {
        assert((await managerSession.innerText()).includes(nativeSeed.threadId));
      }
      evidence.managerSessionVisible = true;
      evidence.managerSessionRow = await managerSession.innerText();
      check("synthetic Weixin thread visible in Codex++ local session management");
      save();
      await screenshot(managerPage, "manager", "manager-session-visible.png");
      await managerPage.getByRole("button", { name: "微信连接", exact: true }).click();
      await managerPage.locator(".weixin-connect-page").waitFor();
      await dialogs.manager.ensureClear();
      await dialogs.codex.ensureClear();
      const projectActions = codexPage.getByRole("button", {
        name: `${nativeSeed.projectName} 的项目操作`, exact: true,
      }).or(codexPage.getByRole("button", {
        name: `Project actions for ${nativeSeed.projectName}`, exact: true,
      }));
      await projectActions.focus();
      assert(await projectActions.evaluate((button) => document.activeElement === button));
      await projectActions.press("Enter");
      evidence.projectMenu = await codexPage.locator('[role="menu"]').allTextContents();
      save();
      const projectMenu = codexPage.getByRole("menu", {
        name: `${nativeSeed.projectName} 的项目操作`, exact: true,
      }).or(codexPage.getByRole("menu", {
        name: `Project actions for ${nativeSeed.projectName}`, exact: true,
      }));
      const rename = projectMenu.getByRole("menuitem", { name: /^(重命名|Rename|编辑|Edit)/i });
      await rename.waitFor({ state: "visible", timeout: 5000 });
      await rename.click();
      // 项目改名是预期对话框，先完成该交互再调用通用弹窗检查。
      await codexPage.getByRole("dialog").waitFor({ timeout: 10000 });
      evidence.projectEditDialog = await codexPage.locator("body").ariaSnapshot();
      save();
      await codexPage.screenshot({ path: path.join(outputDir, "native-project-edit.png") });
      const input = codexPage.getByRole("textbox").filter({ visible: true });
      const matched = await input.all();
      let selected;
      for (const item of matched) if (await item.inputValue().catch(() => "") === nativeSeed.projectName) selected = item;
      assert(selected, "Native rename input matching original project name required");
      const newName = "Weixin Native Renamed Sync";
      await selected.fill(newName);
      const saveProject = codexPage.getByRole("button", { name: /^(保存|Save)(更改| changes)?$/i });
      if (await saveProject.isVisible()) await saveProject.click();
      else await selected.press("Enter");
      await dialogs.codex.ensureClear();
      await codexPage.getByText(newName, { exact: true }).first().waitFor();
      evidence.nativeProjectRenamed = true;
      await screenshot(codexPage, "codex", "native-project-renamed.png");
      await managerPage.getByRole("button", { name: "刷新项目", exact: true }).click();
      await managerPage.waitForFunction((name) => {
        const labels = [...document.querySelectorAll(".weixin-connect-page label.field")];
        const select = labels.find((l) => l.querySelector("span")?.textContent?.trim() === "项目")?.querySelector("select");
        return select?.options[0]?.text === name;
      }, newName, { timeout: 15000 });
      evidence.managerAfter = await invoke("weixin_connect_projects");
      assert.equal(evidence.managerAfter.reservedProjectId, nativeSeed.projectId);
      assert.equal(evidence.managerAfter.projects.find((p) => p.id === nativeSeed.projectId)?.name, newName);
      evidence.managerFirstOptionAfter = await first.innerText();
      evidence.stableProjectIdRenameSynced = true;
      check("native project rename preserves reserved ID and synchronizes manager first option");
      await screenshot(managerPage, "manager", "manager-project-after.png");
      evidence.success = true;
    } catch (e) {
      evidence.error = e.message;
      evidence.nativeAtFailure = await codexPage.locator("body").innerText().catch(() => "");
      evidence.nativeStructureAtFailure = await codexPage.locator("body").ariaSnapshot().catch(() => "");
      throw e;
    } finally { save(); }
  },
};
