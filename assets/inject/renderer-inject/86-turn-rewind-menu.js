  // 仅接入消息菜单，不改原生模块、安装目录或对第三方拓展开放写 RPC。
  window.__codexPlusTurnRewindMenuCleanup?.();
  const turnRewindMenuMark = "builtin-turn-rewind-menu";
  let turnRewindObserver = null;
  let turnRewindRefreshTimer = 0;
  let turnRewindSaveServicePromise = null;

  function turnRewindIsBranchItem(item) {
    return /^(分支到新聊天|在新聊天中分支|从此处分支|Branch (?:in|into|to) (?:a )?new chat|Fork chat from here)$/i
      .test((item?.textContent || "").replace(/\s+/g, " ").trim());
  }

  function turnRewindManagerSupported(value) {
    const methods = ["getHostId", "getConversation", "getCompleteConversationTurns",
      "loadRemainingTurnItems", "sendFollowUpMessage", "addTurnCompletedListener",
      "addConversationStateCallback", "interruptConversation", "sendRequest",
      "applyRevertResponseToConversation"];
    try {
      return value && methods.every((name) => typeof value[name] === "function")
        && value.getHostId() === "local";
    } catch { return false; }
  }

  // React 的逻辑父链会穿过 portal。只读当前菜单的 props、hooks 和 context，
  // 不按页面顺序猜轮次，也不调用未知函数或创建新的会话管理器。
  function turnRewindContextFromNode(node) {
    const key = Object.getOwnPropertyNames(node || {}).find((name) => name.startsWith("__reactFiber$"));
    let fiber = key ? node[key] : null;
    const identities = new Map();
    const managers = new Set();
    const inspect = (root) => {
      const queue = [{ value: root, depth: 0 }];
      const seen = new Set();
      for (let count = 0; queue.length && count < 256; count += 1) {
        const { value, depth } = queue.shift();
        if (!value || typeof value !== "object" || seen.has(value)) continue;
        seen.add(value);
        if (turnRewindManagerSupported(value)) { managers.add(value); continue; }
        try {
          if (typeof value.getImplForHostId === "function") {
            const manager = value.getImplForHostId("local");
            if (turnRewindManagerSupported(manager)) managers.add(manager);
          }
          if (depth >= 4) continue;
          for (const name of Object.getOwnPropertyNames(value).slice(0, 32)) {
            if (["children", "_owner", "return", "child", "sibling", "stateNode", "alternate"].includes(name)) continue;
            const child = Object.getOwnPropertyDescriptor(value, name)?.value;
            if (child && typeof child === "object") queue.push({ value: child, depth: depth + 1 });
          }
        } catch { /* 原生 RPC stub 可能拒绝反射，保持不可用。 */ }
      }
    };
    for (let depth = 0; fiber && depth < 64; depth += 1, fiber = fiber.return) {
      // pendingProps 可能属于尚未提交的另一轮，不能用于破坏性操作的目标识别。
      const props = fiber.memoizedProps;
      const threadId = props?.threadId || props?.conversationId;
      const turnId = props?.turnId;
      if (typeof threadId === "string" && typeof turnId === "string" && threadId && turnId) {
        identities.set(`${threadId}:${turnId}`, { threadId, turnId, hostId: props.hostId });
      }
      inspect(props?.value);
      for (let hook = fiber.memoizedState, count = 0; hook && count < 48; hook = hook.next, count += 1) {
        inspect(hook.memoizedState);
      }
      for (let context = fiber.dependencies?.firstContext, count = 0; context && count < 32; context = context.next, count += 1) {
        inspect(context.memoizedValue);
      }
    }
    if (identities.size !== 1) return null;
    const target = [...identities.values()][0];
    if (target.hostId && target.hostId !== "local") return null;
    const matches = [...managers].filter((manager) => {
      try { return !!manager.getConversation(target.threadId); } catch { return false; }
    });
    return matches.length === 1 ? { ...target, manager: matches[0] } : null;
  }

  async function turnRewindNativeSaveService() {
    if (!turnRewindSaveServicePromise) {
      turnRewindSaveServicePromise = (async () => {
        for (const prefix of ["app-shared-", "app-initial-"]) {
          try {
            const module = await loadCodexAppModule(prefix);
            for (const candidate of Object.values(module || {})) {
              try {
                if (candidate && typeof candidate === "object"
                    && typeof candidate.workspaceFiles?.saveCopy === "function") return candidate.workspaceFiles;
              } catch { /* 跳过不支持此能力的 RPC stub。 */ }
            }
          } catch { /* 尝试下一个已知模块前缀。 */ }
        }
        throw Error("当前客户端没有可用的原生另存为接口，恢复记录仍保留在本机。");
      })().catch((error) => { turnRewindSaveServicePromise = null; throw error; });
    }
    return turnRewindSaveServicePromise;
  }

  codexPlusTurnRewind.setSaveCopy(async (request) => {
    const files = await turnRewindNativeSaveService();
    const operation = files.saveCopy(request);
    try { return await operation; }
    finally { operation?.[Symbol.dispose]?.(); }
  });

  function refreshTurnRewindMenus() {
    if (!codexPlusTurnRewindEnabled()) return;
    for (const menu of document.querySelectorAll('[role="menu"]')) {
      if (menu.hidden || menu.getAttribute("data-state") === "closed") continue;
      const branch = [...menu.querySelectorAll('[role="menuitem"]')].find(turnRewindIsBranchItem);
      if (!branch) continue;
      let button = menu.querySelector(`[data-codex-plus-ext="${turnRewindMenuMark}"]`);
      if (!button) {
        button = document.createElement("button");
        button.type = "button";
        button.className = `${branch.className || ""} codex-plus-turn-rewind-item`;
        button.setAttribute("data-codex-plus-ext", turnRewindMenuMark);
        button.setAttribute("role", "menuitem");
        button.textContent = "回退到本轮之前 · Codex++";
        button.style.cssText = "width:100%;text-align:start;cursor:pointer;";
        button.addEventListener("click", async (event) => {
          event.preventDefault();
          event.stopPropagation();
          // 点击时重读 React 身份；菜单节点可被复用，不能信任上次扫描的引用。
          const currentBranch = button.previousElementSibling;
          if (!turnRewindIsBranchItem(currentBranch)) return;
          const target = turnRewindContextFromNode(currentBranch);
          if (!target || !codexPlusTurnRewindEnabled()) return;
          codexPlusTurnRewind.registerManager(target.threadId, target.manager);
          if (!codexPlusTurnRewind.canRun(target.threadId, target.turnId)) return;
          button.disabled = true;
          try { await codexPlusTurnRewind.run(target.threadId, target.turnId); }
          catch (error) { showToast(String(error?.message || error), { type: "error" }); }
          finally { scheduleTurnRewindMenuRefresh(); }
        });
        // 原生菜单的键盘事件由其 roving-focus 管理器消费；自己的项显式支持。
        button.addEventListener("keydown", (event) => {
          if (!["Enter", " "].includes(event.key) || button.disabled) return;
          event.preventDefault();
          event.stopPropagation();
          button.click();
        });
        branch.insertAdjacentElement("afterend", button);
      }
      const target = turnRewindContextFromNode(branch);
      if (target) codexPlusTurnRewind.registerManager(target.threadId, target.manager);
      button.disabled = !target || !codexPlusTurnRewind.canRun(target.threadId, target.turnId);
      button.setAttribute("aria-disabled", String(button.disabled));
      button.title = button.disabled ? "仅支持已加载且空闲的本地 Codex 聊天；无法可靠识别轮次或接口时不可用。"
        : "撤销本轮及后续轮次的可恢复操作；AI 报告全部完成后截断会话。";
    }
  }

  function scheduleTurnRewindMenuRefresh() {
    if (turnRewindRefreshTimer) return;
    turnRewindRefreshTimer = setTimeout(() => {
      turnRewindRefreshTimer = 0;
      refreshTurnRewindMenus();
    }, 60);
  }

  function syncCodexPlusTurnRewind() {
    if (!codexPlusTurnRewindEnabled()) {
      turnRewindObserver?.disconnect();
      turnRewindObserver = null;
      clearTimeout(turnRewindRefreshTimer);
      turnRewindRefreshTimer = 0;
      document.querySelectorAll(`[data-codex-plus-ext="${turnRewindMenuMark}"]`).forEach((node) => node.remove());
      return;
    }
    if (!turnRewindObserver && document.body) {
      turnRewindObserver = new MutationObserver((records) => {
        // 自己的菜单项与状态面板都带归属标记，避免注入引起自喂扫描。
        const touchesMenu = records.some((record) => {
          if (record.target?.closest?.('[data-codex-plus-ext]')) return false;
          const added = [...(record.addedNodes || [])];
          if (added.length && added.every((node) => node.nodeType === 1 && node.hasAttribute("data-codex-plus-ext"))) return false;
          return !!record.target?.closest?.('[role="menu"]') || added.some((node) =>
            node.nodeType === 1 && (node.matches?.('[role="menu"]') || node.querySelector?.('[role="menu"]')));
        });
        if (touchesMenu) scheduleTurnRewindMenuRefresh();
      });
      turnRewindObserver.observe(document.body, { childList: true, subtree: true });
      refreshTurnRewindMenus();
    }
  }

  window.__codexPlusTurnRewindMenuCleanup = () => {
    turnRewindObserver?.disconnect();
    clearTimeout(turnRewindRefreshTimer);
    document.querySelectorAll(`[data-codex-plus-ext="${turnRewindMenuMark}"]`).forEach((node) => node.remove());
    // 在途回滚与恢复面板保留，不能因重注入失去停止/导出入口。
  };
