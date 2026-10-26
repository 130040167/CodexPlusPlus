/** 本地网关：让 Claude Code、Grok 经 Codex++ 使用统一的供应商池，支持多 Key 轮换与路由组。 */
import { useEffect, useMemo, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, ArrowRightLeft, Download, KeyRound, Power, RefreshCw, RotateCcw, Route as RouteIcon, ShieldCheck } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { PageHeader } from "./ManagementPages";
import { t, tf } from "./i18n";
import { providerAvatarText } from "./provider-name";
import {
  bindingFromDraft,
  draftFromBinding,
  failureLabel,
  formatRemaining,
  protocolLabel,
  targetModels,
  validateDraft,
  type BindingDraft,
  type GatewayAgentId,
  type GatewayAgentView,
  type GatewayPayload,
  type GatewayResult,
} from "./gateway-model";
import "./ClaudeProviderPage.css";
import "./GatewayPage.css";

type Status = "ok" | "failed" | "warning" | "info";
type Notify = (title: string, message: string, status?: Status) => Promise<void>;

const memo: { payload: GatewayPayload | null } = { payload: null };

function hasPayload(result: GatewayResult): result is GatewayResult & GatewayPayload {
  return Boolean(result.gateway && Array.isArray(result.pool));
}

export function GatewayPage({ notify, onOpenCodexProviders }: { notify: Notify; onOpenCodexProviders: () => void }) {
  const [payload, setPayload] = useState<GatewayPayload | null>(memo.payload);
  const [busy, setBusy] = useState<string | null>(memo.payload ? null : "load");

  const accept = (result: GatewayResult) => {
    if (hasPayload(result)) {
      memo.payload = result;
      setPayload(result);
    }
  };

  const run = async (key: string, command: string, args: Record<string, unknown> | undefined, title: string, silent = false) => {
    setBusy(key);
    try {
      const result = await invoke<GatewayResult>(command, args);
      accept(result);
      if (result.status !== "ok") {
        await notify(tf("{0}失败", [title]), result.message, "failed");
        return false;
      }
      if (!silent) await notify(title, result.message, "ok");
      return true;
    } catch (error) {
      await notify(t("调用失败"), String(error), "failed");
      return false;
    } finally {
      setBusy(null);
    }
  };

  const refresh = async (silent = true) => {
    setBusy("load");
    try {
      const result = await invoke<GatewayResult>("load_gateway");
      accept(result);
      if (result.status !== "ok") await notify(t("读取失败"), result.message, "failed");
      else if (!silent) await notify(t("已刷新"), result.message, "ok");
    } catch (error) {
      await notify(t("调用失败"), String(error), "failed");
    } finally {
      setBusy(null);
    }
  };

  useEffect(() => {
    void refresh();
    // 只在进入本页时读一次，之后由本页操作维护。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <div className="mg-page gateway-page">
      <PageHeader
        icon={<RouteIcon className="h-5 w-5" />}
        title={t("本地网关")}
        actions={(
          <Button disabled={busy !== null} onClick={() => void refresh(false)} size="sm" variant="ghost">
            <RefreshCw className={`h-4 w-4 ${busy === "load" ? "animate-spin" : ""}`} />
            {t("刷新")}
          </Button>
        )}
      />
      <div className="mg-content gateway-content">
        <p className="gateway-intro">
          {t("开启后，Agent 的请求先到 Codex++ 本地网关，再按绑定转发到供应商池里的上游：Claude Code 也能用 Chat / Responses 协议的供应商和 ChatGPT 订阅，并享受多 Key 轮换、故障切换和请求日志。不开启时保持原来的直连切换，互不影响。")}
        </p>
        {payload && !payload.helperRunning && payload.agents.some((agent) => agent.enabled) ? (
          <p className="claude-live-alert" role="alert">
            <AlertTriangle className="h-4 w-4" />
            <span>{t("本地网关当前没有运行，走网关的 Agent 会连不上。请重新保存一次绑定或重启 Codex++。")}</span>
          </p>
        ) : null}

        <section className="gateway-section">
          <h3>{t("Agent 绑定")}</h3>
          <div className="gateway-agent-grid">
            {(payload?.agents ?? []).map((agent) => (
              <AgentCard
                agent={agent}
                busy={busy}
                key={agent.agent}
                payload={payload}
                onDisable={async () => {
                  const ok = await confirm(tf("把 {0} 恢复成直连？会切回开启网关前的供应商。", [agent.name]), { title: t("恢复直连"), kind: "warning" });
                  if (ok) await run(`disable:${agent.agent}`, "disable_gateway_binding", { agent: agent.agent }, t("恢复直连"));
                }}
                onRotate={() => void run(`rotate:${agent.agent}`, "rotate_gateway_token", { agent: agent.agent }, t("更换 token"))}
                onSave={(binding) => void run(`save:${agent.agent}`, "save_gateway_binding", { agent: agent.agent, binding }, t("保存网关绑定"))}
              />
            ))}
          </div>
        </section>

        <PoolSection
          busy={busy}
          payload={payload}
          onImport={(command, id) => void run(`import:${id}`, command, { id }, t("导入供应商池"))}
          onOpenCodexProviders={onOpenCodexProviders}
        />

        <HealthSection busy={busy} payload={payload} onReset={(relayId) => void run(`reset:${relayId}`, "reset_gateway_key_health", { relayId }, t("重置 Key 状态"), true)} />

        <OptionsSection
          busy={busy}
          payload={payload}
          onSave={(keyRotation, restoreOnExit) => void run("options", "save_gateway_options", { keyRotation, restoreOnExit }, t("保存网关选项"), true)}
        />
      </div>
    </div>
  );
}

function AgentCard({
  agent,
  payload,
  busy,
  onSave,
  onDisable,
  onRotate,
}: {
  agent: GatewayAgentView;
  payload: GatewayPayload | null;
  busy: string | null;
  onSave: (binding: ReturnType<typeof bindingFromDraft>) => void;
  onDisable: () => void;
  onRotate: () => void;
}) {
  const binding = payload?.gateway.bindings[agent.agent as GatewayAgentId];
  const [draft, setDraft] = useState<BindingDraft>(() => draftFromBinding(binding));
  const [advanced, setAdvanced] = useState(false);
  // 后端保存后（或别处改动后）用最新绑定重置表单。
  const bindingKey = JSON.stringify(binding ?? null);
  useEffect(() => {
    setDraft(draftFromBinding(binding));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bindingKey]);

  const isClaude = agent.agent === "claude";
  const models = useMemo(() => targetModels(payload, draft.target), [payload, draft.target]);
  const validation = validateDraft(draft, payload);
  const update = (patch: Partial<BindingDraft>) => setDraft((previous) => ({ ...previous, ...patch }));
  const listId = `gateway-models-${agent.agent}`;
  const saving = busy === `save:${agent.agent}`;

  return (
    <div className={`gateway-agent-card ${agent.enabled ? "active" : ""}`}>
      <div className="gateway-agent-head">
        <strong>{agent.name}</strong>
        <span className="relay-tag" data-tone={agent.enabled ? "api" : "official"}>
          {agent.enabled ? t("经网关") : t("直连")}
        </span>
        {agent.enabled && agent.defaultModel ? <small>{agent.defaultModel}</small> : null}
      </div>
      {agent.enabled ? (
        <div className="gateway-agent-endpoint" title={agent.baseUrl}>
          <em>{t("网关地址")}</em>
          <code>{agent.baseUrl}</code>
        </div>
      ) : null}

      <div className="claude-editor-grid">
        <label className="claude-field span-2">
          <span>{t("转发到")}</span>
          <select className="claude-select gateway-select" onChange={(event) => update({ target: event.currentTarget.value })} value={draft.target}>
            <option value="">{t("选择供应商或路由组")}</option>
            {payload?.pool.length ? (
              <optgroup label={t("供应商")}>
                {payload.pool.map((entry) => (
                  <option key={entry.id} value={`provider:${entry.id}`}>
                    {`${entry.name} · ${entry.official ? t("官方登录") : protocolLabel(entry.protocol)}${entry.keyCount > 1 ? ` · ${tf("{0} 个 Key", [entry.keyCount])}` : ""}`}
                  </option>
                ))}
              </optgroup>
            ) : null}
            {payload?.groups.length ? (
              <optgroup label={t("路由组")}>
                {payload.groups.map((group) => (
                  <option key={group.id} value={`group:${group.id}`}>
                    {`${group.name} · ${tf("{0} 个成员", [group.members.length])}`}
                  </option>
                ))}
              </optgroup>
            ) : null}
          </select>
        </label>
        <label className="claude-field span-2">
          <span>{t("主模型")} <small>{t("留空用目标的默认模型")}</small></span>
          <Input list={listId} onChange={(event) => update({ model: event.currentTarget.value })} placeholder={models[0] ?? ""} value={draft.model} />
          <datalist id={listId}>
            {models.map((model) => <option key={model} value={model} />)}
          </datalist>
        </label>
        {isClaude ? (
          <>
            <label className="claude-field">
              <span>Haiku <small>{t("后台小任务")}</small></span>
              <Input list={listId} onChange={(event) => update({ haiku: event.currentTarget.value })} placeholder={t("同主模型")} value={draft.haiku} />
            </label>
            <label className="claude-field">
              <span>Opus <small>{t("/model 里的 Opus 档")}</small></span>
              <Input list={listId} onChange={(event) => update({ opus: event.currentTarget.value })} placeholder={t("同主模型")} value={draft.opus} />
            </label>
          </>
        ) : null}
      </div>

      <button className="gateway-advanced-toggle" onClick={() => setAdvanced(!advanced)} type="button">
        {advanced ? t("收起高级选项") : t("高级选项")}
      </button>
      {advanced ? (
        <div className="claude-editor-grid">
          {isClaude ? (
            <>
              <label className="claude-field">
                <span>Sonnet</span>
                <Input list={listId} onChange={(event) => update({ sonnet: event.currentTarget.value })} placeholder={t("同主模型")} value={draft.sonnet} />
              </label>
              <label className="claude-field">
                <span>{t("子代理")}</span>
                <Input list={listId} onChange={(event) => update({ subagent: event.currentTarget.value })} placeholder={t("不单独设置")} value={draft.subagent} />
              </label>
            </>
          ) : null}
          <label className="claude-field span-2">
            <span>{t("额外暴露的模型")} <small>{t("每行一个，Agent 里可以切换到这些模型")}</small></span>
            <Textarea onChange={(event) => update({ extraModels: event.currentTarget.value })} rows={3} spellCheck={false} value={draft.extraModels} />
          </label>
          <label className="claude-field">
            <span>{t("未知模型名")}</span>
            <select className="claude-select gateway-select" onChange={(event) => update({ unknownModel: event.currentTarget.value as BindingDraft["unknownModel"] })} value={draft.unknownModel}>
              <option value="mapToDefault">{t("改写成主模型")}</option>
              <option value="passthrough">{t("原样转发")}</option>
            </select>
          </label>
          <label className="claude-field">
            <span>{t("上下文窗口")}</span>
            <Input onChange={(event) => update({ contextWindow: event.currentTarget.value })} placeholder={t("例如 200k 或 1M")} value={draft.contextWindow} />
          </label>
          {isClaude ? (
            <label className="claude-compat span-2">
              <input checked={draft.claudeEnableEffort} onChange={(event) => update({ claudeEnableEffort: event.currentTarget.checked })} type="checkbox" />
              <span>
                <strong>{t("强制启用推理强度")}</strong>
                <small>{t("非 claude-* 模型名时 Claude Code 默认不发 effort，打开后照常发送。")}</small>
              </span>
            </label>
          ) : null}
        </div>
      ) : null}

      <div className="gateway-agent-actions">
        <span className="claude-editor-error" role={validation ? "alert" : undefined}>{validation ? t(validation) : ""}</span>
        {agent.enabled ? (
          <>
            <Button disabled={busy !== null} onClick={onRotate} size="sm" title={t("换一个新的本地 token，旧 token 立即失效")} variant="ghost">
              <RotateCcw className="h-4 w-4" />
              {t("更换 token")}
            </Button>
            <Button disabled={busy !== null} onClick={onDisable} size="sm" variant="outline">
              <ArrowRightLeft className="h-4 w-4" />
              {t("恢复直连")}
            </Button>
          </>
        ) : null}
        <Button disabled={busy !== null || Boolean(validation)} onClick={() => onSave(bindingFromDraft(draft, binding))} size="sm">
          <Power className="h-4 w-4" />
          {saving ? t("保存中") : agent.enabled ? t("保存") : t("开启网关模式")}
        </Button>
      </div>
    </div>
  );
}

function PoolSection({
  payload,
  busy,
  onImport,
  onOpenCodexProviders,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  onImport: (command: string, id: string) => void;
  onOpenCodexProviders: () => void;
}) {
  const [claudeId, setClaudeId] = useState("");
  const [grokId, setGrokId] = useState("");
  return (
    <section className="gateway-section">
      <div className="claude-section-head">
        <h3>{t("供应商池")}</h3>
        <Button onClick={onOpenCodexProviders} size="sm" variant="ghost">
          <KeyRound className="h-4 w-4" />
          {t("在 Codex 供应商配置里编辑")}
        </Button>
      </div>
      <p className="claude-field-hint">
        {t("供应商池就是 Codex 的供应商列表（含官方登录账号），路由组就是聚合供应商；在 Codex「供应商配置」里增删改，这里直接引用。多个 Key 在供应商的命名 Key 里添加。")}
      </p>
      <div className="relay-profile-list gateway-pool-list">
        {(payload?.pool ?? []).map((entry) => (
          <div className="relay-profile-card gateway-pool-card" key={entry.id}>
            <span className="relay-index" title={entry.name}>{providerAvatarText(entry.name, "P")}</span>
            <span className="relay-summary">
              <strong>
                <span className="relay-summary-name">{entry.name}</span>
                <span className="relay-tag" data-tone={entry.official ? "official" : "api"}>
                  {entry.official ? t("官方登录") : protocolLabel(entry.protocol)}
                </span>
                {entry.keyCount > 1 ? <span className="relay-tag">{tf("{0} 个 Key", [entry.keyCount])}</span> : null}
              </strong>
              <small>{entry.models.length ? entry.models.join(" · ") : t("未配置模型")}</small>
            </span>
          </div>
        ))}
        {payload && payload.pool.length === 0 ? (
          <div className="claude-provider-empty">
            <strong>{t("供应商池是空的")}</strong>
            <span>{t("先在 Codex「供应商配置」里添加供应商，或从下面导入 Claude / Grok 的供应商。")}</span>
          </div>
        ) : null}
      </div>
      <div className="gateway-import-row">
        <select className="claude-select gateway-select" onChange={(event) => setClaudeId(event.currentTarget.value)} value={claudeId}>
          <option value="">{t("选择 Claude 供应商")}</option>
          {(payload?.claudeProviders ?? []).map((item) => <option key={item.id} value={item.id}>{item.name}</option>)}
        </select>
        <Button disabled={!claudeId || busy !== null} onClick={() => onImport("import_claude_provider_to_gateway_pool", claudeId)} size="sm" variant="outline">
          <Download className="h-4 w-4" />
          {t("导入到池")}
        </Button>
        <select className="claude-select gateway-select" onChange={(event) => setGrokId(event.currentTarget.value)} value={grokId}>
          <option value="">{t("选择 Grok 供应商")}</option>
          {(payload?.grokProfiles ?? []).map((item) => <option key={item.id} value={item.id}>{item.name}</option>)}
        </select>
        <Button disabled={!grokId || busy !== null} onClick={() => onImport("import_grok_profile_to_gateway_pool", grokId)} size="sm" variant="outline">
          <Download className="h-4 w-4" />
          {t("导入到池")}
        </Button>
      </div>
    </section>
  );
}

function HealthSection({ payload, busy, onReset }: { payload: GatewayPayload | null; busy: string | null; onReset: (relayId: string) => void }) {
  const keys = payload?.health.keys.filter((key) => key.state !== "ok" || key.lastFailure) ?? [];
  const breakers = payload?.health.providers.filter((provider) => provider.breakerOpen) ?? [];
  const relayName = (id: string) => payload?.pool.find((entry) => entry.id === id)?.name ?? id;
  return (
    <section className="gateway-section">
      <h3>{t("Key 状态")}</h3>
      {keys.length === 0 && breakers.length === 0 ? (
        <p className="claude-field-hint">
          <ShieldCheck className="gateway-inline-icon h-4 w-4" />
          {t("所有 Key 都正常。被限流的 Key 会自动冷却，额度耗尽或失效的 Key 会被跳过，连续出错的供应商会被短暂熔断。")}
        </p>
      ) : (
        <div className="gateway-health-list">
          {breakers.map((provider) => (
            <div className="gateway-health-row" key={`breaker:${provider.relayId}`}>
              <strong>{relayName(provider.relayId)}</strong>
              <span className="relay-tag" data-tone="warning">{t("熔断中")}</span>
              <small>{formatRemaining(provider.remainingMs)}</small>
              <Button disabled={busy !== null} onClick={() => onReset(provider.relayId)} size="sm" variant="ghost">{t("重置")}</Button>
            </div>
          ))}
          {keys.map((key) => (
            <div className="gateway-health-row" key={`${key.relayId}:${key.keyId}`}>
              <strong>{relayName(key.relayId)}</strong>
              <code>{key.keyId}</code>
              <span className="relay-tag" data-tone={key.state === "ok" ? "api" : "warning"}>
                {key.state === "cooling" ? t("冷却中") : key.state === "disabled" ? t("已停用") : t("正常")}
              </span>
              <small>{[t(failureLabel(key.lastFailure)), formatRemaining(key.remainingMs)].filter(Boolean).join(" · ")}</small>
              <Button disabled={busy !== null} onClick={() => onReset(key.relayId)} size="sm" variant="ghost">{t("重置")}</Button>
            </div>
          ))}
        </div>
      )}
    </section>
  );
}

function OptionsSection({
  payload,
  busy,
  onSave,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  onSave: (keyRotation: string, restoreOnExit: boolean) => void;
}) {
  if (!payload) return null;
  const mode = payload.gateway.keyPolicy.mode;
  const restoreOnExit = payload.gateway.restoreOnExit;
  return (
    <section className="gateway-section">
      <h3>{t("网关选项")}</h3>
      <div className="claude-editor-grid">
        <label className="claude-field">
          <span>{t("多 Key 轮换")}</span>
          <select className="claude-select gateway-select" disabled={busy !== null} onChange={(event) => onSave(event.currentTarget.value, restoreOnExit)} value={mode}>
            <option value="sticky">{t("会话粘性：同一会话固定一个 Key，出错才换（推荐，保留缓存）")}</option>
            <option value="roundRobin">{t("逐请求轮换：每个请求换下一个 Key")}</option>
          </select>
        </label>
        <label className="claude-compat">
          <input checked={restoreOnExit} disabled={busy !== null} onChange={(event) => onSave(mode, event.currentTarget.checked)} type="checkbox" />
          <span>
            <strong>{t("退出 Codex++ 时恢复直连")}</strong>
            <small>{t("网关只在 Codex++ 运行时可用。打开后退出时把 Agent 切回直连，下次启动再自动指回网关。")}</small>
          </span>
        </label>
      </div>
    </section>
  );
}

/** 嵌在 Claude / Grok 供应商页顶部：显示是否经网关，并跳到网关页。 */
export function GatewayBanner({ active, agentName, onOpen }: { active: boolean; agentName: string; onOpen: () => void }) {
  return (
    <p className="gateway-banner" role="status">
      <RouteIcon className="h-4 w-4" />
      <span>
        {active
          ? tf("{0} 当前经 Codex++ 本地网关转发，供应商、模型与多 Key 在「本地网关」里管理；在下方直接切换供应商会恢复直连。", [agentName])
          : tf("想让 {0} 使用 Codex 的供应商池、ChatGPT 订阅、多 Key 轮换或路由组？可以开启本地网关模式。", [agentName])}
      </span>
      <Button onClick={onOpen} size="sm" variant="outline">{active ? t("管理网关") : t("了解网关模式")}</Button>
    </p>
  );
}
