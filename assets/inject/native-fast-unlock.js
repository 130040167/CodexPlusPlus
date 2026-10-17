(() => {
  const root = typeof window === "object" ? window : {};
  const id = "[A-Za-z_$][\\w$]*";
  const chatgpt = "`chatgpt`";
  const token = "`personalAccessToken`";
  const apiKey = "`apiKey`";
  const local = "`local`";
  const critical = "`critical`";

  const runtime = {
    update(settings) {
      root.__codexPlusSettings = settings && typeof settings === "object" ? settings : null;
      return runtime.status();
    },
    status() {
      const settings = root.__codexPlusSettings;
      const enhancementsEnabled = settings?.enhancementsEnabled === true;
      const optedIn = settings?.codexAppNativeFastUnlock === true;
      return {
        enabled: enhancementsEnabled && optedIn,
        enhancementsEnabled,
        optedIn,
        settingsLoaded: !!settings && typeof settings === "object",
      };
    },
    allow(hostId, authMethod) {
      return hostId === "local"
        && (authMethod === "apikey" || authMethod === "apiKey")
        && runtime.status().enabled;
    },
  };

  function allow(host, auth) {
    return `window.__codexPlusNativeFastUnlock?.runtime?.allow?.(${host},${auth})===true`;
  }

  // 只识别已核对的四条完整调用结构；压缩函数名和局部变量名都可变化。
  // 不修改 requirements、模型 serviceTiers 或 Standard 的解析规则。
  const gates = [
    {
      kind: "ui-access",
      pattern: [
        `function ${id}\\((?<argument>${id})\\)\\{let ${id}=\\(0,${id}\\.c\\)\\(\\d+\\),(?<fallback>${id})=${id}\\(${id}\\),`,
        `(?<host>${id})=\\k<argument>\\?\\.hostId\\?\\?\\k<fallback>,(?<identity>${id})=${id}\\(\\k<host>\\),`,
        `(?<variable>${id})=(?<gate>\\k<identity>\\?\\.authMethod===${chatgpt}\\|\\|\\k<identity>\\?\\.authMethod===${token}),`,
        `(?<method>${id})=\\k<identity>\\?\\.authMethod\\?\\?null,[^{}]{0,300}\\{authMethod:\\k<method>,hostId:\\k<host>\\}`,
        `[^{}]{0,300}\\{data:(?<data>${id}),isPending:(?<pending>${id})\\}=${id}\\(${id},${id}\\),`,
        `(?<loading>${id})=!!\\k<identity>\\?\\.isLoading\\|\\|\\k<variable>&&\\k<pending>,${id};`,
        `[^{}]{0,300}\\k<variable>&&!\\k<loading>&&\\k<data>!=null\\?${id}\\(\\k<data>\\):null`,
        `[^{}]{0,300}\\{serviceTierAccess:${id},isLoading:\\k<loading>\\}`,
      ].join(""),
      replacement: (groups) => `(${groups.gate}||${allow(groups.host, `${groups.identity}?.authMethod`)})`,
    },
    {
      kind: "request-access",
      pattern: [
        `async function ${id}\\((?<scope>${id}),(?<host>${id})\\)\\{let (?<auth>${id})=await ${id}\\(\\k<scope>,\\k<host>\\);`,
        `if\\((?<gate>\\k<auth>!==${chatgpt}&&\\k<auth>!==${token})\\)return null;`,
        `let (?<requirements>${id})=await ${id}\\(\\k<scope>,\\k<host>,\\{priority:${critical}\\}\\);`,
        `return \\k<scope>\\.query\\.setData\\(${id},\\{authMethod:\\k<auth>,hostId:\\k<host>\\},\\k<requirements>\\),${id}\\(\\k<requirements>\\)\\}`,
      ].join(""),
      replacement: (groups) => `${groups.gate}&&!(${allow(groups.host, groups.auth)})`,
    },
    {
      kind: "queue-default-access",
      pattern: [
        `function ${id}\\((?<manager>${id}),\\{runtime:(?<runtime>${id}),storage:(?<storage>${id})\\},(?<before>${id}),${id}\\)\\{let (?<pending>${id})=null;`,
        `return\\{async readServiceTier\\((?<model>${id})\\)\\{for\\(;\\k<pending>!=null;\\)await \\k<pending>\\.catch\\(\\(\\)=>void 0\\);`,
        `return \\k<before>\\(\\),${id}\\(\\k<manager>\\.requestClient,\\k<manager>\\.logger,\\k<model>,async\\(\\)=>`,
        `\\k<manager>\\.getHostId\\(\\)===${local}&&await \\k<storage>\\.readGlobalState\\(\`use-copilot-auth-if-available\`\\)===!0&&await \\k<runtime>\\.isCopilotApiAvailable\\(\\)\\?!1:`,
        `(?<gate>(?<account>\\(await \\k<manager>\\.getAccount\\(\\{priority:${critical}\\}\\)\\)\\.account\\?\\.type)===${chatgpt})\\)\\},async writeServiceTier\\(`,
      ].join(""),
      // includes 只消费一次 account/read；外层 Copilot 分支原样保留。
      replacement: (groups) => `[${chatgpt},...(${allow(`${groups.manager}.getHostId()`, apiKey)}?[${apiKey}]:[])].includes(${groups.account})`,
    },
    {
      kind: "queue-explicit-access",
      pattern: [
        `function ${id}\\((?<manager>${id}),\\{settings:(?<settings>${id}),storage:(?<storage>${id}),filesystem:${id},runtime:(?<runtime>${id})\\},(?<readDefault>${id}),${id},${id}\\)\\{`,
        `return async\\(${id},(?<message>${id})\\)=>\\{let (?<options>${id})=\\k<message>\\.submissionOptions,${id}=\\k<message>\\.context\\.workPermissions,${id}=${id}!=null,`,
        `(?<host>${id})=\\k<options>\\?\\.executionHostId\\?\\?\\k<manager>\\.getHostId\\(\\);`,
        `[\\s\\S]{0,1800}?let\\[${id},${id}\\]=await Promise\\.all\\(\\[\\(async\\(\\)=>\\{if\\(\\k<options>==null\\)return \\k<readDefault>\\(${id}\\?\\.latestModel\\|\\|null\\);`,
        `if\\(\\k<options>\\.serviceTier==null\\)return null;let\\{account:(?<account>${id})\\}=await \\k<manager>\\.getAccount\\(\\{priority:${critical}\\}\\);`,
        `return (?<gate>\\k<account>\\?\\.type===${chatgpt})\\?\\k<options>\\.serviceTier:null\\}\\)\\(\\),\\k<options>\\?\\.usePermissionSelection\\?\\?\\k<settings>\\.readPermissionRefreshEnabled\\(\\)\\]\\)`,
      ].join(""),
      // manager 和实际执行端都必须是本机；所选 Copilot 不能借底层 API 身份放行。
      replacement: (groups) => `(${groups.gate}||${groups.host}===${local}&&${allow(`${groups.manager}.getHostId()`, `${groups.account}?.type`)}&&!(await ${groups.storage}.readGlobalState(\`use-copilot-auth-if-available\`)===!0&&await ${groups.runtime}.isCopilotApiAvailable()))`,
    },
  ];

  function plan(source, url) {
    const diagnostics = { url, reason: "ready", matches: {} };
    const failed = (reason) => ({ ok: false, replacements: [], diagnostics: { ...diagnostics, reason } });
    if (typeof source !== "string" || !source || typeof url !== "string" || !url.trim()) {
      return failed("invalid-input");
    }
    const replacements = [];
    for (const definition of gates) {
      const matches = [...source.matchAll(new RegExp(definition.pattern, "g"))];
      diagnostics.matches[definition.kind] = matches.length;
      if (matches.length !== 1) continue;
      const match = matches[0];
      const gate = match.groups.gate;
      const offset = match[0].indexOf(gate);
      if (offset < 0 || offset !== match[0].lastIndexOf(gate) || /[\r\n]/.test(gate)) {
        return failed("unsupported-shape");
      }
      const start = match.index + offset;
      const text = definition.replacement(match.groups);
      if (/[\r\n]/.test(text)) return failed("unsupported-shape");
      replacements.push({ kind: definition.kind, start, end: start + gate.length, text });
    }
    if (Object.values(diagnostics.matches).some((count) => count > 1)) return failed("ambiguous-gates");
    if (replacements.length !== gates.length) return failed("incomplete-gates");
    replacements.sort((left, right) => left.start - right.start);
    if (replacements.some((replacement, index) => index > 0 && replacement.start < replacements[index - 1].end)) {
      return failed("overlapping-gates");
    }
    return { ok: true, replacements, diagnostics };
  }

  function transform(source, url) {
    const result = plan(source, url);
    let patchedSource = source;
    if (result.ok) {
      for (const replacement of [...result.replacements].reverse()) {
        patchedSource = patchedSource.slice(0, replacement.start) + replacement.text + patchedSource.slice(replacement.end);
      }
    }
    return { ...result, patchedSource };
  }

  const api = { plan, transform, runtime };
  if (typeof module === "object" && module.exports) module.exports = api;
  if (typeof window === "object") window.__codexPlusNativeFastUnlock = api;
})();
