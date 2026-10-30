(() => {
  const root = window;
  if (root.__codexPlusNativeThreadGoal?.version === 1) return;
  const patches = new WeakMap();
  const requests = new Map();
  let modulePromise = null;
  let adapters = null;

  async function nativeModule() {
    if (!modulePromise) {
      modulePromise = Promise.resolve().then(async () => {
        if (adapters?.loadModule) return adapters.loadModule();
        const urls = [
          ...Array.from(document.querySelectorAll("script[src],link[href]"), node => node.src || node.href),
          ...performance.getEntriesByType("resource").map(entry => entry.name),
        ];
        const shared = urls.find(value => {
          try {
            const url = new URL(value, root.location.href);
            return url.protocol === "app:" && url.hostname === "-"
              && /^\/assets\/app-shared-[\w-]+\.js$/.test(url.pathname);
          } catch { return false; }
        });
        if (!shared) throw new Error("native-module-unavailable");
        return import(shared);
      }).then(module => {
        if (!module) throw new Error("native-module-unavailable");
        return module;
      }).catch(error => {
        modulePromise = null;
        throw error;
      });
    }
    return modulePromise;
  }

  function appScopes(module) {
    const scopes = new Set(Object.values(module).filter(value =>
      value && typeof value === "object" && value.kind === "signal-family").map(value => value.scope));
    const found = new Set(), seen = new Set(), stack = [];
    for (const node of [document.getElementById("root"), document.body, document.documentElement]) {
      if (!node) continue;
      const key = Object.keys(node).find(key => /^__react(Container|Fiber)\$/.test(key));
      const fiber = key && node[key];
      if (fiber) stack.push(fiber.stateNode?.current || fiber);
    }
    for (let count = 0; stack.length && count < 4000; count++) {
      const fiber = stack.pop();
      if (!fiber || typeof fiber !== "object" || seen.has(fiber)) continue;
      seen.add(fiber);
      for (let hook = fiber.memoizedState, index = 0; hook && index < 64; hook = hook.next, index++) {
        const scope = hook.memoizedState?.current;
        if (scope && typeof scope.get === "function" && scopes.has(scope.scope)) found.add(scope);
      }
      if (fiber.child) stack.push(fiber.child);
      if (fiber.sibling) stack.push(fiber.sibling);
    }
    return found;
  }

  async function managerFor(threadId, hostId) {
    const module = await nativeModule();
    // 已验证的原生 getter：按 thread 查 host，再从该 AppScope 的 managers 中查找。
    const getters = Object.values(module).filter(value => {
      if (typeof value !== "function") return false;
      try {
        const source = Function.prototype.toString.call(value);
        return source.includes(".getConversation(") && source.includes(".find(")
          && source.includes(".get(") && source.includes("return null");
      } catch { return false; }
    });
    for (const scope of appScopes(module)) for (const getter of getters) {
      let manager;
      try { manager = getter(scope, threadId); } catch { continue; }
      if (manager && manager.disposed !== true && typeof manager.setThreadGoal === "function"
          && typeof manager.getStreamRole === "function" && typeof manager.getHostId === "function"
          && typeof manager.getConversation === "function" && manager.getHostId() === hostId
          && manager.getConversation(threadId)) return manager;
    }
    return null;
  }

  async function probe({ threadId, hostId }) {
    const manager = await managerFor(threadId, hostId);
    return { status: manager?.getStreamRole(threadId)?.role || "unavailable" };
  }

  async function execute(request) {
    const { threadId, hostId, requestId, params } = request;
    const key = JSON.stringify([hostId, threadId, params]);
    const previous = requests.get(requestId);
    if (previous) {
      if (previous.key !== key) throw new Error("goal-request-id-conflict");
      return previous.promise;
    }
    const manager = await managerFor(threadId, hostId);
    if (!manager || manager.getStreamRole(threadId)?.role !== "owner") {
      throw new Error("goal-owner-changed");
    }
    // 不驱逐未完成请求，也不在未知结果后重试另一窗口。
    const duplicate = requests.get(requestId);
    if (duplicate) {
      if (duplicate.key !== key) throw new Error("goal-request-id-conflict");
      return duplicate.promise;
    }
    for (const [id, entry] of requests) {
      if (entry.finishedAt && Date.now() - entry.finishedAt > 300_000) requests.delete(id);
    }
    if (requests.size >= 128) throw new Error("goal-request-limit");
    const original = patches.get(Object.getPrototypeOf(manager))?.original || manager.setThreadGoal;
    const entry = { key, promise: null, finishedAt: 0 };
    entry.promise = Promise.resolve().then(() => {
      if (manager.disposed || manager.getHostId() !== hostId || !manager.getConversation(threadId)
          || manager.getStreamRole(threadId)?.role !== "owner") throw new Error("goal-owner-changed");
      // 原生实现会在 resume 后再次校验 owner，并维护 settings、transcript 和广播。
      return original.call(manager, threadId, params);
    }).then(goal => ({ status: "ok", goal })).finally(() => { entry.finishedAt = Date.now(); });
    requests.set(requestId, entry);
    return entry.promise;
  }

  async function install(threadId, options) {
    adapters = options;
    const manager = await managerFor(threadId, options.hostId);
    if (!manager) return false;
    const proto = Object.getPrototypeOf(manager);
    if (patches.has(proto)) return true;
    const descriptor = Object.getOwnPropertyDescriptor(proto, "setThreadGoal");
    if (!descriptor?.writable || typeof descriptor.value !== "function") return false;
    const original = descriptor.value;
    proto.setThreadGoal = async function (id, params) {
      if (!adapters?.enabled()) return original.call(this, id, params);
      const forward = async () => {
        const result = await adapters.forward({
          threadId: id, hostId: this.getHostId(), requestId: crypto.randomUUID(), params,
        });
        if (result?.status !== "ok" || !result.goal) {
          adapters.report(result?.code || "goal-forward-failed");
          throw new Error(result?.message || "会话所在窗口暂不可用，请稍后重试设置目标");
        }
        return result.goal;
      };
      if (this.getStreamRole(id)?.role === "follower") return forward();
      try { return await original.call(this, id, params); }
      catch (error) {
        // 仅补偿 resume 期间转为 follower 的已知断言；它发生在任何目标写入之前。
        if (error?.message === "Please continue this conversation on the window where it was started."
            && this.getStreamRole(id)?.role === "follower") return forward();
        throw error;
      }
    };
    patches.set(proto, { original });
    return true;
  }

  root.__codexPlusNativeThreadGoal = { version: 1, probe, execute, install };
})();
