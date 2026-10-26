/** 本地网关：让 Claude Code、Grok 经 Codex++ 使用统一的供应商池，支持多 Key 轮换与路由组。 */
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { confirm } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, Check, ChevronDown, ChevronRight, LayoutGrid, Plus, RefreshCw, RotateCcw, Route as RouteIcon, Search, Waypoints, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { t, tf } from "./i18n";
import { providerAvatarText } from "./provider-name";
import {
  agentName,
  bindingDisplay,
  bindingFromDraft,
  draftFromBinding,
  failureLabel,
  formatRemaining,
  gatewayPresets,
  groupSlug,
  memberStatus,
  parseTargetValue,
  pickerSections,
  presetAdded,
  presetHost,
  protocolLabel,
  providerDraftFromPreset,
  routeCards,
  routeMembers,
  routeNarrative,
  strategyDescription,
  strategyLabel,
  targetModels,
  validateDraft,
  validateProviderDraft,
  type AgentActivityView,
  type BindingDraft,
  type GatewayAgentId,
  type GatewayAgentView,
  type GatewayPayload,
  type GatewayPreset,
  type GatewayProtocol,
  type GatewayResult,
  type MemberStatus,
  type PresetSection,
  type ProviderDraft,
  type RouteRecordView,
} from "./gateway-model";
import "./GatewayPage.css";

type Status = "ok" | "failed" | "warning" | "info";
type Notify = (title: string, message: string, status?: Status) => Promise<void>;
type Tab = "agents" | "providers" | "routing" | "gateway";
type Run = (key: string, command: string, args: Record<string, unknown> | undefined, title: string, silent?: boolean) => Promise<boolean>;

const memo: { payload: GatewayPayload | null; tab: Tab } = { payload: null, tab: "agents" };

/** 品牌图标随应用打包，来源和许可见 assets/agents/LICENSE.txt。 */
const AGENT_ICONS: Record<string, string> = {
  claude: new URL("./assets/agents/claude.svg", import.meta.url).href,
  grok: new URL("./assets/agents/grok.svg", import.meta.url).href,
  codex: new URL("./assets/agents/chatgpt.svg", import.meta.url).href,
};
const APP_LOGO_URL = new URL("./assets/codex-plus-logo.png", import.meta.url).href;
/** 路由图里各 Agent 连线的色相。 */
const AGENT_HUES: Record<string, number> = { claude: 18, grok: 210, codex: 250 };
/** 路由页实时刷新间隔。 */
const LIVE_REFRESH_MS = 2500;

function hasPayload(result: GatewayResult): result is GatewayResult & GatewayPayload {
  return Boolean(result.gateway && Array.isArray(result.pool));
}

function hueOf(name: string): number {
  let hash = 0;
  for (const char of name) hash = (hash * 31 + (char.codePointAt(0) ?? 0)) % 360;
  return hash;
}

function avatarOf(name: string): string {
  return providerAvatarText(name, "P");
}

/** 供应商的字母头像：颜色按名字固定。 */
function Avatar({ name, size = "md" }: { name: string; size?: "sm" | "md" }) {
  return (
    <span className="gw-avatar" data-size={size} style={{ "--gw-hue": hueOf(name) } as CSSProperties} title={name}>
      {avatarOf(name)}
    </span>
  );
}

function Switch({ on, disabled, label, onToggle }: { on: boolean; disabled?: boolean; label: string; onToggle: () => void }) {
  return (
    <button aria-checked={on} aria-label={label} className={`gw-switch ${on ? "is-on" : ""}`} disabled={disabled} onClick={onToggle} role="switch" type="button">
      <span />
    </button>
  );
}

function SectionLabel({ children, aside }: { children: ReactNode; aside?: ReactNode }) {
  return (
    <div className="gw-section-label">
      <span>{children}</span>
      {aside ? <div className="gw-section-aside">{aside}</div> : null}
    </div>
  );
}

export function GatewayPage({ notify, onOpenCodexProviders }: { notify: Notify; onOpenCodexProviders: () => void }) {
  const [payload, setPayload] = useState<GatewayPayload | null>(memo.payload);
  const [busy, setBusy] = useState<string | null>(memo.payload ? null : "load");
  const [tab, setTabState] = useState<Tab>(memo.tab);
  const setTab = (next: Tab) => {
    memo.tab = next;
    setTabState(next);
  };

  const accept = (result: GatewayResult) => {
    if (hasPayload(result)) {
      memo.payload = result;
      setPayload(result);
    }
  };

  const run: Run = async (key, command, args, title, silent = false) => {
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

  // 路由页实时刷新：静默读取，不占用 busy，也不弹错误。
  useEffect(() => {
    if (tab !== "routing") return;
    let cancelled = false;
    const timer = window.setInterval(() => {
      void invoke<GatewayResult>("load_gateway")
        .then((result) => {
          if (!cancelled) accept(result);
        })
        .catch(() => undefined);
    }, LIVE_REFRESH_MS);
    return () => {
      cancelled = true;
      window.clearInterval(timer);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tab]);

  const tabs: Array<{ id: Tab; label: string }> = [
    { id: "agents", label: t("智能体") },
    { id: "providers", label: t("供应商") },
    { id: "routing", label: t("路由") },
    { id: "gateway", label: t("网关") },
  ];

  return (
    <div className="mg-page gateway-page">
      {/* 页面标题由外层框架显示，这里只放分段标签和刷新。 */}
      <div className="gw-toolbar">
        <nav aria-label={t("本地网关")} className="gw-tabs" role="tablist">
          {tabs.map((item) => (
            <button aria-selected={tab === item.id} data-active={tab === item.id} key={item.id} onClick={() => setTab(item.id)} role="tab" type="button">
              {item.label}
            </button>
          ))}
        </nav>
        <button aria-label={t("刷新")} className="gw-icon-button" disabled={busy !== null} onClick={() => void refresh(false)} title={t("刷新")} type="button">
          <RefreshCw className={`h-4 w-4 ${busy === "load" ? "animate-spin" : ""}`} />
        </button>
      </div>
      <div className="mg-content gw-content">
        {payload && !payload.helperRunning && payload.agents.some((agent) => agent.enabled) ? (
          <p className="gw-alert" role="alert">
            <AlertTriangle className="h-4 w-4" />
            <span>{t("本地网关当前没有运行，走网关的 Agent 会连不上。请重新保存一次绑定或重启 Codex++。")}</span>
          </p>
        ) : null}
        {tab === "agents" ? <AgentsView busy={busy} notify={notify} payload={payload} run={run} /> : null}
        {tab === "providers" ? <ProvidersView busy={busy} onOpenCodexProviders={onOpenCodexProviders} payload={payload} run={run} /> : null}
        {tab === "routing" ? <RoutingView onOpenCodexProviders={onOpenCodexProviders} payload={payload} /> : null}
        {tab === "gateway" ? <GatewayView busy={busy} payload={payload} run={run} /> : null}
      </div>
    </div>
  );
}

/* —————————————————————— 智能体 —————————————————————— */

function AgentsView({ payload, busy, run, notify }: { payload: GatewayPayload | null; busy: string | null; run: Run; notify: Notify }) {
  const [expanded, setExpanded] = useState<string | null>(null);
  return (
    <>
      <p className="gw-lead">
        {t("打开一个 Agent 的开关，它的请求就经")} <strong>{t("Codex++ 网关")}</strong> {t("转发到供应商池；模型也可以直接在这里选。关掉开关恢复原来的直连。")}
      </p>
      <div className="gw-card">
        {(payload?.agents ?? []).map((agent) => (
          <AgentRow
            agent={agent}
            busy={busy}
            expanded={expanded === agent.agent}
            key={agent.agent}
            notify={notify}
            onToggleExpand={() => setExpanded(expanded === agent.agent ? null : agent.agent)}
            payload={payload}
            run={run}
          />
        ))}
        {!payload ? <div className="gw-empty">{t("正在读取…")}</div> : null}
      </div>
    </>
  );
}

function AgentRow({
  agent,
  payload,
  busy,
  expanded,
  onToggleExpand,
  run,
  notify,
}: {
  agent: GatewayAgentView;
  payload: GatewayPayload | null;
  busy: string | null;
  expanded: boolean;
  onToggleExpand: () => void;
  run: Run;
  notify: Notify;
}) {
  const binding = payload?.gateway.bindings[agent.agent as GatewayAgentId];
  const [draft, setDraft] = useState<BindingDraft>(() => draftFromBinding(binding));
  const [pickerOpen, setPickerOpen] = useState(false);
  // 后端保存后（或别处改动后）用最新绑定重置表单。
  const bindingKey = JSON.stringify(binding ?? null);
  useEffect(() => {
    setDraft(draftFromBinding(binding));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [bindingKey]);

  const isClaude = agent.agent === "claude";
  const save = (next: BindingDraft) => run(`save:${agent.agent}`, "save_gateway_binding", { agent: agent.agent, binding: bindingFromDraft(next, binding) }, t("保存网关绑定"));
  const disable = async () => {
    const ok = await confirm(tf("把 {0} 恢复成直连？会切回开启网关前的供应商。", [agent.name]), { title: t("恢复直连"), kind: "warning" });
    if (ok) await run(`disable:${agent.agent}`, "disable_gateway_binding", { agent: agent.agent }, t("恢复直连"));
  };
  const pick = (target: string, model: string) => {
    const next = { ...draft, target, model };
    setDraft(next);
    setPickerOpen(false);
    if (agent.enabled) void save(next);
  };
  const toggle = () => {
    if (agent.enabled) {
      void disable();
      return;
    }
    const error = validateDraft(draft, payload);
    if (error) {
      setPickerOpen(true);
      void notify(t("还不能开启"), t(error), "warning");
      return;
    }
    void save(draft);
  };

  const running = Boolean(payload?.helperRunning);
  const tone = agent.enabled ? (running ? "on" : "error") : "off";
  const status = agent.enabled
    ? running
      ? isClaude
        ? t("已接入 · 在 /model 里切换 opus · sonnet · haiku 档")
        : tf("已接入 · {0} 个模型在 Grok 的模型列表里", [agent.models.length])
      : t("已接入 · 网关没有运行")
    : t("直连 · 选好模型后打开开关即可接入");

  return (
    <div className="gw-agent" data-expanded={expanded}>
      <div className="gw-agent-row">
        <img alt="" className="gw-agent-icon" src={AGENT_ICONS[agent.agent]} />
        <div className="gw-agent-main">
          <strong>{agent.name}</strong>
          <small>
            <i className="gw-dot" data-tone={tone} />
            {status}
          </small>
        </div>
        <button aria-expanded={expanded} className="gw-link" onClick={onToggleExpand} type="button">
          {isClaude ? t("档位") : tf("模型 {0}", [agent.models.length])}
          <ChevronRight className="h-3.5 w-3.5" />
        </button>
        <ModelPicker
          model={draft.model}
          onOpenChange={setPickerOpen}
          onPick={pick}
          open={pickerOpen}
          payload={payload}
          target={draft.target}
        />
        <Switch disabled={busy !== null} label={tf("{0} 经网关", [agent.name])} on={agent.enabled} onToggle={toggle} />
      </div>
      {expanded ? (
        <AgentDetail
          agent={agent}
          busy={busy}
          draft={draft}
          onChange={(patch) => setDraft((previous) => ({ ...previous, ...patch }))}
          onRotate={() => void run(`rotate:${agent.agent}`, "rotate_gateway_token", { agent: agent.agent }, t("更换 token"))}
          onSave={() => void save(draft)}
          payload={payload}
        />
      ) : null}
    </div>
  );
}

/** 模型选择器：左侧按供应商跳转，右侧列出路由组和每个供应商的模型，可直接输入模型 id。 */
function ModelPicker({
  payload,
  target,
  model,
  open,
  onOpenChange,
  onPick,
}: {
  payload: GatewayPayload | null;
  target: string;
  model: string;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onPick: (target: string, model: string) => void;
}) {
  const [query, setQuery] = useState("");
  const [position, setPosition] = useState<CSSProperties | null>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const popoverRef = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const sections = useMemo(() => pickerSections(payload, query, avatarOf), [payload, query]);
  const display = bindingDisplay(payload, target, model);

  // 弹层用 fixed 定位挂到 body：按钮下方放不下时翻到上方，高度不超出窗口。
  useLayoutEffect(() => {
    if (!open) return;
    const place = () => {
      const rect = wrapRef.current?.getBoundingClientRect();
      if (!rect) return;
      const width = Math.min(440, window.innerWidth - 24);
      const left = Math.max(12, Math.min(rect.right - width, window.innerWidth - width - 12));
      const below = window.innerHeight - rect.bottom - 18;
      const above = rect.top - 18;
      const upward = below < 320 && above > below;
      const maxHeight = Math.min(460, upward ? above : below);
      setPosition(upward
        ? { left, width, bottom: window.innerHeight - rect.top + 6, maxHeight }
        : { left, width, top: rect.bottom + 6, maxHeight });
    };
    place();
    window.addEventListener("resize", place);
    window.addEventListener("scroll", place, true);
    return () => {
      window.removeEventListener("resize", place);
      window.removeEventListener("scroll", place, true);
    };
  }, [open]);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    const close = (event: MouseEvent) => {
      const node = event.target as Node;
      if (!wrapRef.current?.contains(node) && !popoverRef.current?.contains(node)) onOpenChange(false);
    };
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onOpenChange(false);
    };
    document.addEventListener("mousedown", close);
    document.addEventListener("keydown", escape);
    return () => {
      document.removeEventListener("mousedown", close);
      document.removeEventListener("keydown", escape);
    };
  }, [open, onOpenChange]);

  const jump = (key: string) => {
    const node = listRef.current?.querySelector<HTMLElement>(`[data-section="${CSS.escape(key)}"]`);
    if (node && listRef.current) listRef.current.scrollTop = node.offsetTop - 6;
  };
  const isSelected = (item: { target: string; model: string }) => item.target === target && (display.kind === "group" || item.model === display.title);

  return (
    <div className="gw-picker" ref={wrapRef}>
      <button aria-expanded={open} aria-haspopup="listbox" className="gw-picker-button" data-empty={!display.kind} onClick={() => onOpenChange(!open)} type="button">
        {display.kind === "group" ? <Waypoints className="gw-picker-glyph h-4 w-4" /> : display.kind ? <Avatar name={display.sub} size="sm" /> : null}
        <span>{display.kind ? display.title : t("选择模型")}</span>
        <ChevronDown className="h-3.5 w-3.5" />
      </button>
      {open && position ? createPortal(
        <div className="gw-popover gw-portal" ref={popoverRef} role="listbox" style={position}>
          <label className="gw-popover-search">
            <Search className="h-4 w-4" />
            <input autoFocus onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("筛选，或输入任意模型 id…")} value={query} />
          </label>
          <div className="gw-popover-body">
            <div className="gw-popover-rail">
              <button aria-label={t("全部")} data-active={!query} onClick={() => { setQuery(""); if (listRef.current) listRef.current.scrollTop = 0; }} type="button">
                <LayoutGrid className="h-4 w-4" />
              </button>
              <span className="gw-rail-divider" />
              {sections.filter((section) => section.key !== "custom").map((section) => (
                <button aria-label={t(section.title)} key={section.key} onClick={() => jump(section.key)} title={section.key === "group" ? t("路由组") : section.title} type="button">
                  {section.key === "group" ? <Waypoints className="h-4 w-4" /> : <Avatar name={section.title} size="sm" />}
                </button>
              ))}
            </div>
            <div className="gw-popover-list" ref={listRef}>
              {sections.length === 0 ? <div className="gw-empty">{payload?.pool.length ? t("没有匹配的模型") : t("供应商池是空的，先到「供应商」里添加。")}</div> : null}
              {sections.map((section) => (
                <div data-section={section.key} key={section.key}>
                  <div className="gw-popover-heading">{section.key === "group" || section.key === "custom" ? t(section.title) : section.title}</div>
                  {section.items.map((item) => (
                    <button
                      className="gw-popover-item"
                      data-selected={isSelected(item)}
                      key={`${item.target}|${item.model}`}
                      onClick={() => onPick(item.target, item.model)}
                      role="option"
                      aria-selected={isSelected(item)}
                      type="button"
                    >
                      {section.key === "group" ? <Waypoints className="gw-picker-glyph h-4 w-4" /> : <Avatar name={item.sub} size="sm" />}
                      <span className="gw-popover-item-label">{item.label}</span>
                      <small>{section.key === "group" ? item.sub : item.custom ? tf("{0} · 自定义模型", [item.sub]) : tf("{0} · 经网关", [item.sub])}</small>
                      {isSelected(item) ? <Check className="gw-popover-check h-4 w-4" /> : null}
                    </button>
                  ))}
                </div>
              ))}
            </div>
          </div>
        </div>,
        document.body,
      ) : null}
    </div>
  );
}

function AgentDetail({
  agent,
  payload,
  draft,
  busy,
  onChange,
  onSave,
  onRotate,
}: {
  agent: GatewayAgentView;
  payload: GatewayPayload | null;
  draft: BindingDraft;
  busy: string | null;
  onChange: (patch: Partial<BindingDraft>) => void;
  onSave: () => void;
  onRotate: () => void;
}) {
  const isClaude = agent.agent === "claude";
  const models = useMemo(() => targetModels(payload, draft.target), [payload, draft.target]);
  const validation = validateDraft(draft, payload);
  const listId = `gateway-models-${agent.agent}`;
  const binding = payload?.gateway.bindings[agent.agent as GatewayAgentId];
  const placeholder = t("同主模型");
  return (
    <div className="gw-agent-detail">
      <datalist id={listId}>
        {models.map((model) => <option key={model} value={model} />)}
      </datalist>
      {isClaude ? (
        <div className="gw-tier-grid">
          {([
            ["opus", "Opus", t("/model 里的 Opus 档")],
            ["sonnet", "Sonnet", t("/model 里的 Sonnet 档")],
            ["haiku", "Haiku", t("后台小任务")],
            ["subagent", t("子代理"), t("不单独设置")],
          ] as Array<[keyof BindingDraft, string, string]>).map(([key, label, hint]) => (
            <label className="gw-field" key={key}>
              <span>{label} <small>{hint}</small></span>
              <Input list={listId} onChange={(event) => onChange({ [key]: event.currentTarget.value })} placeholder={key === "subagent" ? t("不单独设置") : placeholder} value={String(draft[key])} />
            </label>
          ))}
        </div>
      ) : null}
      <div className="gw-detail-grid">
        <label className="gw-field">
          <span>{t("主模型")} <small>{t("留空用目标的默认模型")}</small></span>
          <Input list={listId} onChange={(event) => onChange({ model: event.currentTarget.value })} placeholder={models[0] ?? ""} value={draft.model} />
        </label>
        <label className="gw-field">
          <span>{t("未知模型名")}</span>
          <select className="gw-select" onChange={(event) => onChange({ unknownModel: event.currentTarget.value as BindingDraft["unknownModel"] })} value={draft.unknownModel}>
            <option value="mapToDefault">{t("改写成主模型")}</option>
            <option value="passthrough">{t("原样转发")}</option>
          </select>
        </label>
        <label className="gw-field">
          <span>{t("上下文窗口")}</span>
          <Input onChange={(event) => onChange({ contextWindow: event.currentTarget.value })} placeholder={t("例如 200k 或 1M")} value={draft.contextWindow} />
        </label>
        <label className="gw-field gw-span-3">
          <span>{t("额外暴露的模型")} <small>{t("每行一个，Agent 里可以切换到这些模型")}</small></span>
          <Textarea onChange={(event) => onChange({ extraModels: event.currentTarget.value })} rows={2} spellCheck={false} value={draft.extraModels} />
        </label>
        {isClaude ? (
          <label className="gw-check gw-span-3">
            <input checked={draft.claudeEnableEffort} onChange={(event) => onChange({ claudeEnableEffort: event.currentTarget.checked })} type="checkbox" />
            <span>
              <strong>{t("强制启用推理强度")}</strong>
              <small>{t("非 claude-* 模型名时 Claude Code 默认不发 effort，打开后照常发送。")}</small>
            </span>
          </label>
        ) : null}
      </div>
      <div className="gw-detail-footer">
        {agent.enabled ? <code title={agent.baseUrl}>{agent.baseUrl}</code> : <span />}
        <span className="gw-error" role={validation ? "alert" : undefined}>{validation ? t(validation) : ""}</span>
        {binding?.token ? (
          <Button disabled={busy !== null} onClick={onRotate} size="sm" title={t("换一个新的本地 token，旧 token 立即失效")} variant="ghost">
            <RotateCcw className="h-4 w-4" />
            {t("更换 token")}
          </Button>
        ) : null}
        <Button disabled={busy !== null || Boolean(validation)} onClick={onSave} size="sm">
          {busy === `save:${agent.agent}` ? t("保存中") : agent.enabled ? t("保存") : t("保存并开启")}
        </Button>
      </div>
    </div>
  );
}

/* —————————————————————— 供应商 —————————————————————— */

function statusText(status: MemberStatus): string {
  return status.args.length ? tf(status.text, status.args) : t(status.text);
}

function ProvidersView({
  payload,
  busy,
  run,
  onOpenCodexProviders,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  onOpenCodexProviders: () => void;
}) {
  const [adding, setAdding] = useState(false);
  if (adding) {
    return <AddProviderSheet busy={busy} onClose={() => setAdding(false)} onOpenCodexProviders={onOpenCodexProviders} payload={payload} run={run} />;
  }
  const pool = payload?.pool ?? [];
  const boundIds = new Set(Object.values(payload?.gateway.bindings ?? {}).filter((binding) => binding?.target.kind === "provider").map((binding) => binding!.target.id));
  const remove = async (id: string, name: string) => {
    const ok = await confirm(tf("把「{0}」移出供应商池？Codex 供应商列表里的这一项会保留。", [name]), { title: t("移出供应商池"), kind: "warning" });
    if (ok) await run(`remove:${id}`, "remove_gateway_pool_provider", { id }, t("移出供应商池"));
  };
  return (
    <>
      <SectionLabel
        aside={(
          <button className="gw-text-button strong" onClick={() => setAdding(true)} type="button">
            <Plus className="h-3.5 w-3.5" />
            {t("添加供应商")}
          </button>
        )}
      >
        {t("供应商池")}
      </SectionLabel>
      <div className="gw-card">
        {pool.map((entry) => {
          const status = memberStatus(payload, entry.id);
          const failures = (payload?.health.keys ?? []).filter((key) => key.relayId === entry.id && key.state !== "ok");
          const unhealthy = failures.length > 0 || Boolean(payload?.health.providers.some((item) => item.relayId === entry.id && item.breakerOpen));
          const bound = boundIds.has(entry.id);
          return (
            <div className="gw-provider-row" key={entry.id}>
              <Avatar name={entry.name} />
              <div className="gw-provider-main">
                <div>
                  <strong>{entry.name}</strong>
                  <span className="gw-pill">{entry.official ? t("官方登录") : protocolLabel(entry.protocol)}</span>
                  {bound ? <span className="gw-pill" data-tone="ok">{t("已绑定")}</span> : null}
                </div>
                <small>{entry.models.length ? entry.models.join(" · ") : t("未配置模型")}</small>
                {failures.length ? (
                  <small className="gw-provider-keys">
                    {failures.map((key) => `${key.keyId}：${[t(failureLabel(key.lastFailure)) || t(key.state === "disabled" ? "已停用" : "冷却中"), formatRemaining(key.remainingMs)].filter(Boolean).join(" · ")}`).join("；")}
                  </small>
                ) : null}
              </div>
              <span className="gw-status" data-state={status.state}>{statusText(status)}</span>
              <span className="gw-row-actions">
                {unhealthy ? (
                  <button className="gw-text-button" disabled={busy !== null} onClick={() => void run(`reset:${entry.id}`, "reset_gateway_key_health", { relayId: entry.id }, t("重置 Key 状态"), true)} type="button">
                    {t("重置")}
                  </button>
                ) : null}
                <button
                  className="gw-text-button"
                  disabled={busy !== null || bound}
                  onClick={() => void remove(entry.id, entry.name)}
                  title={bound ? t("正有 Agent 绑定在这个供应商上") : t("移出供应商池，Codex 里的供应商保留")}
                  type="button"
                >
                  {t("移出")}
                </button>
              </span>
            </div>
          );
        })}
        {payload && pool.length === 0 ? (
          <div className="gw-empty">
            <strong>{t("供应商池是空的")}</strong>
            <span>{t("点「添加供应商」：从内置厂商预设新建、从 Codex 已有的供应商里挑选，或从 Claude Code / Grok 导入。")}</span>
            <button className="gw-text-button strong gw-empty-action" onClick={() => setAdding(true)} type="button">
              <Plus className="h-3.5 w-3.5" />
              {t("添加供应商")}
            </button>
          </div>
        ) : null}
      </div>
      <p className="gw-footnote">
        {t("池里只放加入网关的供应商，数据仍存在 Codex 供应商列表里，移出池不会删除它。多个 Key 在供应商的命名 Key 里添加；被限流的 Key 会自动冷却，额度耗尽或失效的 Key 会被跳过，连续出错的供应商会被短暂熔断。")}
        {" "}
        <button className="gw-text-button gw-inline" onClick={onOpenCodexProviders} type="button">{t("在 Codex 供应商配置里编辑")}</button>
      </p>
    </>
  );
}

type Tile = { key: string; name: string; sub: string; added?: boolean; dashed?: boolean; onClick: () => void; icon?: ReactNode };

function TileGrid({ tiles, empty, disabled }: { tiles: Tile[]; empty?: string; disabled: boolean }) {
  return (
    <div className="gw-tile-grid">
      {tiles.map((tile) => (
        <button className={`gw-tile ${tile.dashed ? "gw-tile-custom" : ""}`} data-added={Boolean(tile.added)} disabled={disabled} key={tile.key} onClick={tile.onClick} type="button">
          {tile.icon ?? <Avatar name={tile.name} />}
          <span>
            <strong>{tile.name}</strong>
            <small>{tile.sub}</small>
          </span>
          {tile.added ? <Check className="gw-tile-check h-4 w-4" /> : null}
        </button>
      ))}
      {tiles.length === 0 && empty ? <span className="gw-tile-empty">{empty}</span> : null}
    </div>
  );
}

/** 添加供应商：内置厂商预设、Codex 已有供应商、Claude Code / Grok 导入、自定义地址。 */
function AddProviderSheet({
  payload,
  busy,
  run,
  onClose,
  onOpenCodexProviders,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  onClose: () => void;
  onOpenCodexProviders: () => void;
}) {
  const [query, setQuery] = useState("");
  const [form, setForm] = useState<{ preset: GatewayPreset | null } | null>(null);
  const needle = query.trim().toLowerCase();
  const hit = (...values: string[]) => !needle || values.some((value) => value.toLowerCase().includes(needle));
  const presets = useMemo(() => gatewayPresets(), []);
  const disabled = busy !== null;

  const finish = async (command: string, args: Record<string, unknown>, title: string) => {
    if (await run(`add:${command}`, command, args, title)) onClose();
  };
  const presetTiles = (section: PresetSection): Tile[] => presets
    .filter((preset) => preset.section === section && hit(preset.name, preset.baseUrl))
    .map((preset) => ({
      key: preset.key,
      name: preset.name,
      sub: presetAdded(payload, preset) ? t("已在供应商池里") : presetHost(preset.baseUrl),
      added: presetAdded(payload, preset),
      onClick: () => setForm({ preset }),
    }));
  const inPool = (name: string) => Boolean(payload?.pool.some((entry) => entry.name === name));
  const importTiles = (items: Array<{ id: string; name: string }>, command: string, sub: string): Tile[] => items
    .filter((item) => hit(item.name))
    .map((item) => ({
      key: item.id,
      name: item.name,
      sub: inPool(item.name) ? t("已在供应商池里") : sub,
      added: inPool(item.name),
      onClick: () => void finish(command, { id: item.id }, t("导入供应商池")),
    }));
  const codexTiles: Tile[] = (payload?.codexProviders ?? [])
    .filter((entry) => hit(entry.name, entry.baseUrl))
    .map((entry) => ({
      key: entry.id,
      name: entry.name,
      sub: entry.official ? t("官方登录") : `${protocolLabel(entry.protocol)} · ${presetHost(entry.baseUrl)}`,
      onClick: () => void finish("add_gateway_pool_providers", { ids: [entry.id] }, t("加入供应商池")),
    }));
  const localTiles: Tile[] = [
    ...presetTiles("local"),
    ...(hit(t("自定义")) ? [{ key: "custom", name: t("自定义"), sub: t("任意兼容地址"), dashed: true, icon: <span className="gw-avatar gw-avatar-plus"><Plus className="h-4 w-4" /></span>, onClick: () => setForm({ preset: null }) }] : []),
  ];
  const subscriptionTiles: Tile[] = hit("ChatGPT", t("ChatGPT 订阅"))
    ? [{ key: "chatgpt", name: t("ChatGPT 订阅"), sub: t("在 Codex 里登录官方账号"), icon: <img alt="" className="gw-tile-logo" src={AGENT_ICONS.codex} />, onClick: onOpenCodexProviders }]
    : [];

  const sections: Array<{ key: string; title: string; hint: string; tiles: Tile[]; empty?: string }> = [
    { key: "subscription", title: t("订阅"), hint: t("登录即可，无需 Key；登录后从下面「Codex 里已有的」加入"), tiles: subscriptionTiles },
    { key: "vendor", title: t("厂商"), hint: t("填 Key 即可"), tiles: presetTiles("vendor") },
    { key: "relay", title: t("中转"), hint: t("一个 Key 用多家模型"), tiles: presetTiles("relay") },
    { key: "local", title: t("本机"), hint: t("本机运行的模型服务，或任意兼容地址"), tiles: localTiles },
    { key: "codex", title: t("Codex 里已有的"), hint: t("直接加入，不复制数据"), tiles: codexTiles, empty: needle ? undefined : t("Codex 的供应商都已在池里") },
    { key: "claude", title: t("从 Claude Code 导入"), hint: t("Anthropic 协议，Claude Code 经网关时原样转发"), tiles: importTiles(payload?.claudeProviders ?? [], "import_claude_provider_to_gateway_pool", t("Claude Code 供应商")) },
    { key: "grok", title: t("从 Grok 导入"), hint: t("Grok 页里配置的供应商"), tiles: importTiles(payload?.grokProfiles ?? [], "import_grok_profile_to_gateway_pool", t("Grok 供应商")) },
  ];
  const visible = sections.filter((section) => section.tiles.length || (!needle && section.empty));

  return (
    <div className="gw-sheet">
      <div className="gw-sheet-head">
        <span className="gw-section-title">{t("添加供应商")}</span>
        <label className="gw-sheet-search">
          <Search className="h-4 w-4" />
          <input autoFocus onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("查找供应商…")} value={query} />
        </label>
        <button className="gw-text-button" onClick={onClose} type="button">{t("关闭")}</button>
      </div>
      {visible.map((section) => (
        <section key={section.key}>
          <h4>{section.title} <span>· {section.hint}</span></h4>
          <TileGrid disabled={disabled} empty={section.empty} tiles={section.tiles} />
        </section>
      ))}
      {visible.length === 0 ? <div className="gw-empty">{t("没有匹配的供应商")}</div> : null}
      {form ? (
        <ProviderFormDialog
          busy={busy}
          onCancel={() => setForm(null)}
          onSubmit={(draft) => void finish("create_gateway_pool_provider", { ...draft }, t("添加供应商"))}
          preset={form.preset}
        />
      ) : null}
    </div>
  );
}

/** 预设或自定义供应商的表单：名称、地址、Key、协议、模型。 */
function ProviderFormDialog({
  preset,
  busy,
  onSubmit,
  onCancel,
}: {
  preset: GatewayPreset | null;
  busy: string | null;
  onSubmit: (draft: ProviderDraft) => void;
  onCancel: () => void;
}) {
  const [draft, setDraft] = useState<ProviderDraft>(() => providerDraftFromPreset(preset));
  const update = (patch: Partial<ProviderDraft>) => setDraft((previous) => ({ ...previous, ...patch }));
  const validation = validateProviderDraft(draft);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onCancel]);
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onCancel(); }}>
      <form
        className="gw-modal"
        onSubmit={(event) => {
          event.preventDefault();
          if (!validation) onSubmit({ ...draft, model: draft.model.trim() });
        }}
      >
        <header>
          {preset ? <Avatar name={preset.name} /> : <span className="gw-avatar gw-avatar-plus"><Plus className="h-4 w-4" /></span>}
          <strong>{preset ? tf("添加 {0}", [preset.name]) : t("添加自定义供应商")}</strong>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onCancel} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body">
          <label className="gw-field">
            <span>{t("名称")}</span>
            <Input onChange={(event) => update({ name: event.currentTarget.value })} value={draft.name} />
          </label>
          <label className="gw-field">
            <span>{t("API Key")} {preset?.apiKeyUrl ? <small>{tf("在 {0} 获取", [presetHost(preset.apiKeyUrl)])}</small> : preset?.defaultKey ? <small>{t("本机服务不校验 Key，保留默认值即可")}</small> : null}</span>
            <Input autoComplete="off" autoFocus={Boolean(preset && !preset.defaultKey)} onChange={(event) => update({ apiKey: event.currentTarget.value })} placeholder="sk-..." type="password" value={draft.apiKey} />
          </label>
          <div className="gw-modal-grid">
            <label className="gw-field">
              <span>{t("地址")}</span>
              <Input onChange={(event) => update({ baseUrl: event.currentTarget.value })} placeholder="https://api.example.com/v1" value={draft.baseUrl} />
            </label>
            <label className="gw-field">
              <span>{t("协议")}</span>
              <select className="gw-select" onChange={(event) => update({ protocol: event.currentTarget.value as GatewayProtocol })} value={draft.protocol}>
                <option value="chatCompletions">Chat Completions</option>
                <option value="responses">Responses</option>
                <option value="anthropicMessages">Anthropic Messages</option>
              </select>
            </label>
          </div>
          <label className="gw-field">
            <span>{t("默认模型")} <small>{t("留空用模型列表的第一个")}</small></span>
            <Input onChange={(event) => update({ model: event.currentTarget.value })} value={draft.model} />
          </label>
          <label className="gw-field">
            <span>{t("模型列表")} <small>{t("每行一个，会出现在智能体的模型选择器里")}</small></span>
            <Textarea onChange={(event) => update({ modelList: event.currentTarget.value })} rows={4} spellCheck={false} value={draft.modelList} />
          </label>
        </div>
        <footer>
          <span className="gw-error">{validation ? t(validation) : ""}</span>
          <Button onClick={onCancel} size="sm" type="button" variant="ghost">{t("取消")}</Button>
          <Button disabled={busy !== null || Boolean(validation)} size="sm" type="submit">{busy ? t("保存中") : t("添加到供应商池")}</Button>
        </footer>
      </form>
    </div>,
    document.body,
  );
}

/* —————————————————————— 路由 —————————————————————— */

function RoutingView({ payload, onOpenCodexProviders }: { payload: GatewayPayload | null; onOpenCodexProviders: () => void }) {
  const cards = routeCards(payload);
  return (
    <>
      <SectionLabel>
        {t("路由")}
        <i className="gw-dot gw-dot-live" data-tone={payload?.helperRunning ? "on" : "off"} />
        <em>{payload?.helperRunning ? t("实时，按网关的实际决策") : t("网关没有运行")}</em>
      </SectionLabel>
      {cards.length === 0 ? (
        <div className="gw-card gw-empty">
          <strong>{t("还没有 Agent 经网关")}</strong>
          <span>{t("在「智能体」里为 Claude Code 或 Grok 选好模型并打开开关，这里会实时显示每个请求怎么走。")}</span>
        </div>
      ) : null}
      {cards.map((card) => <RouteCard agents={card.agents} key={card.target} payload={payload} target={card.target} />)}

      <SectionLabel
        aside={(
          <>
            <em>{t("Agent 可以把路由组当作一个模型来选")}</em>
            <button className="gw-text-button strong" onClick={onOpenCodexProviders} type="button">{t("新建路由组")}</button>
          </>
        )}
      >
        {t("路由组")}
      </SectionLabel>
      <div className="gw-card">
        {(payload?.groups ?? []).map((group) => {
          const members = group.members.map((id) => payload?.pool.find((entry) => entry.id === id)).filter((entry): entry is NonNullable<typeof entry> => Boolean(entry));
          const joiner = group.strategy === "failover" ? " → " : " · ";
          return (
            <div className="gw-group-row" key={group.id}>
              <span className="gw-avatar-stack">
                {members.slice(0, 3).map((entry) => <Avatar key={entry.id} name={entry.name} size="sm" />)}
              </span>
              <div className="gw-group-main">
                <div>
                  <strong>{group.name}</strong>
                  <code className="gw-chip">{groupSlug(group.name)}</code>
                </div>
                <small>{members.length ? members.map((entry) => `${entry.name} · ${entry.models[0] ?? "—"}`).join(joiner) : t("没有成员")}</small>
              </div>
              <span className="gw-pill gw-pill-outline">{t(strategyLabel(group.strategy))}</span>
              <button className="gw-text-button" onClick={onOpenCodexProviders} type="button">{t("编辑")}</button>
            </div>
          );
        })}
        {payload && payload.groups.length === 0 ? (
          <div className="gw-empty">
            <strong>{t("还没有路由组")}</strong>
            <span>{t("在 Codex「供应商配置」里新建聚合供应商，它就是一个路由组：多个供应商按顺序或轮转回答。")}</span>
          </div>
        ) : null}
      </div>
    </>
  );
}

function RouteCard({ payload, target, agents }: { payload: GatewayPayload | null; target: string; agents: GatewayAgentId[] }) {
  const parsed = parseTargetValue(target);
  const group = parsed?.kind === "group" ? payload?.groups.find((item) => item.id === parsed.id) : undefined;
  const entry = parsed?.kind === "provider" ? payload?.pool.find((item) => item.id === parsed.id) : undefined;
  const activity = (payload?.activity?.agents ?? []).filter((item): item is AgentActivityView => agents.includes(item.agent as GatewayAgentId));
  const latest = activity
    .filter((item) => item.last)
    .sort((left, right) => (right.last?.finishedAtMs ?? 0) - (left.last?.finishedAtMs ?? 0))[0];
  const records = activity.map((item) => item.last).filter((record): record is RouteRecordView => Boolean(record));
  const members = routeMembers(payload, target, records);
  const totals = activity.reduce((sum, item) => ({ requests: sum.requests + item.requests, rerouted: sum.rerouted + item.rerouted, errors: sum.errors + item.errors }), { requests: 0, rerouted: 0, errors: 0 });
  const keyMode = payload?.gateway.keyPolicy.mode === "roundRobin" ? t("逐请求换 Key") : t("会话粘性");

  const title = group?.name ?? entry?.name ?? parsed?.id ?? "";
  const meta = group
    ? tf("路由组 · {0} 个成员 · {1} 个 Agent", [group.members.length, agents.length])
    : tf("供应商 · {0} 个 Key · {1} 个 Agent", [entry?.keyCount ?? 1, agents.length]);
  const description = group
    ? t(strategyDescription(group.strategy))
    : payload?.gateway.keyPolicy.mode === "roundRobin"
      ? t("单个供应商：每个请求换下一个 Key，被限流或失效的 Key 自动跳过。")
      : t("单个供应商：同一会话固定用一个 Key 保住缓存，出错才换下一个 Key。");
  const summary = latest?.last
    ? group && latest.last.planned[0]
      ? tf("{0} 排在第一个。", [latest.last.planned[0].relayName])
      : tf("全部请求发到 {0}。", [title])
    : t("还没有请求经过这里。在 Agent 里发一条消息试试。");

  return (
    <div className="gw-card gw-route-card">
      <div className="gw-route-head">
        <strong>{title}</strong>
        <span> · {meta}</span>
        <p>{description}</p>
      </div>
      <RouteDiagram
        activeAgent={latest?.agent ?? null}
        agents={agents.map((agent) => ({ id: agent, binding: payload?.gateway.bindings[agent] }))}
        hubPill={group ? t(strategyLabel(group.strategy)) : keyMode}
        members={members.map((member) => ({ ...member, status: memberStatus(payload, member.relayId) }))}
        payload={payload}
      />
      <div className="gw-route-summary">
        <p>{summary}</p>
        <dl>
          <div><dt>{totals.requests}</dt><dd>{t("个请求")}</dd></div>
          <div><dt>{totals.rerouted}</dt><dd>{t("次换上游")}</dd></div>
          <div><dt>{totals.errors}</dt><dd>{t("个错误交给了 Agent")}</dd></div>
        </dl>
      </div>
      {latest?.last ? (
        <div className="gw-route-trace">
          <h4>{t("上一个请求是怎么路由的")}</h4>
          <ul>
            {routeNarrative(latest.agent, latest.last, payload).map((line, index) => (
              <li data-tone={line.tone} key={index}>{tf(line.text, line.args.map((arg) => (typeof arg === "string" && /^[一-龥]/.test(arg) ? t(arg) : arg)))}</li>
            ))}
          </ul>
        </div>
      ) : null}
    </div>
  );
}

const AGENT_NODE_H = 76;
const AGENT_NODE_GAP = 12;
const MEMBER_ROW_H = 46;
const MEMBER_ROW_GAP = 8;
const HUB_H = 118;
const LINK_W = 64;

/** 路由图：左边 Agent，中间网关，右边成员；最近一次请求走过的线用虚线高亮。 */
function RouteDiagram({
  payload,
  agents,
  members,
  activeAgent,
  hubPill,
}: {
  payload: GatewayPayload | null;
  agents: Array<{ id: GatewayAgentId; binding: GatewayPayload["gateway"]["bindings"][GatewayAgentId] }>;
  members: Array<{ relayId: string; name: string; model: string; status: MemberStatus }>;
  activeAgent: string | null;
  hubPill: string;
}) {
  const agentsH = agents.length * AGENT_NODE_H + Math.max(0, agents.length - 1) * AGENT_NODE_GAP;
  const membersH = members.length * MEMBER_ROW_H + Math.max(0, members.length - 1) * MEMBER_ROW_GAP;
  const height = Math.max(agentsH, membersH, HUB_H);
  const agentTop = (height - agentsH) / 2;
  const memberTop = (height - membersH) / 2;
  const middle = height / 2;
  const agentY = (index: number) => agentTop + index * (AGENT_NODE_H + AGENT_NODE_GAP) + AGENT_NODE_H / 2;
  const memberY = (index: number) => memberTop + index * (MEMBER_ROW_H + MEMBER_ROW_GAP) + MEMBER_ROW_H / 2;
  const curve = (fromY: number, toY: number) => `M0 ${fromY} C${LINK_W / 2} ${fromY}, ${LINK_W / 2} ${toY}, ${LINK_W} ${toY}`;
  const hue = AGENT_HUES[activeAgent ?? ""] ?? 232;
  const activeStyle = { "--gw-hue": hue } as CSSProperties;

  return (
    <div className="gw-diagram" style={{ height }}>
      <div className="gw-diagram-col" style={{ paddingTop: agentTop, gap: AGENT_NODE_GAP }}>
        {agents.map((agent) => (
          <div className="gw-node-agent" key={agent.id} style={{ height: AGENT_NODE_H, "--gw-hue": AGENT_HUES[agent.id] ?? 232 } as CSSProperties}>
            <img alt="" src={AGENT_ICONS[agent.id]} />
            <strong><i className="gw-dot" data-tone="agent" />{agentName(agent.id)}</strong>
            <small>{agent.binding ? bindingDisplay(payload, `${agent.binding.target.kind}:${agent.binding.target.id}`, agent.binding.model).title : ""}</small>
          </div>
        ))}
      </div>
      <svg aria-hidden className="gw-links" height={height} width={LINK_W}>
        {agents.map((agent, index) => (
          <path className={agent.id === activeAgent ? "active" : ""} d={curve(agentY(index), middle)} key={agent.id} style={agent.id === activeAgent ? activeStyle : undefined} />
        ))}
      </svg>
      <div className="gw-diagram-col" style={{ paddingTop: (height - HUB_H) / 2 }}>
        <div className="gw-hub" style={{ height: HUB_H }}>
          <img alt="" src={APP_LOGO_URL} />
          <strong>{t("Codex++ 网关")}</strong>
          <small>{`127.0.0.1:${payload?.port ?? ""}`}</small>
          <span className="gw-pill gw-pill-solid">{hubPill}</span>
        </div>
      </div>
      <svg aria-hidden className="gw-links" height={height} width={LINK_W}>
        {members.map((member, index) => {
          const active = member.status.state === "answered" || member.status.state === "busy";
          return <path className={active ? "active" : ""} d={curve(middle, memberY(index))} key={member.relayId} style={active ? activeStyle : undefined} />;
        })}
      </svg>
      <div className="gw-diagram-col" style={{ paddingTop: memberTop, gap: MEMBER_ROW_GAP }}>
        {members.map((member) => (
          <div className="gw-member" data-state={member.status.state} key={member.relayId} style={{ height: MEMBER_ROW_H }}>
            <i className="gw-radio" />
            <Avatar name={member.name} size="sm" />
            <strong>{member.name}</strong>
            {member.model ? <code className="gw-chip">{member.model}</code> : null}
            <span className="gw-status" data-state={member.status.state}>{statusText(member.status)}</span>
          </div>
        ))}
      </div>
    </div>
  );
}

/* —————————————————————— 网关 —————————————————————— */

function GatewayView({ payload, busy, run }: { payload: GatewayPayload | null; busy: string | null; run: Run }) {
  if (!payload) return null;
  const mode = payload.gateway.keyPolicy.mode;
  const restoreOnExit = payload.gateway.restoreOnExit;
  const saveOptions = (keyRotation: string, restore: boolean) => void run("options", "save_gateway_options", { keyRotation, restoreOnExit: restore }, t("保存网关选项"), true);
  return (
    <>
      <SectionLabel>{t("网关")}</SectionLabel>
      <div className="gw-card">
        <div className="gw-setting-row">
          <div>
            <strong>{t("监听地址")}</strong>
            <small>{t("只监听本机；各 Agent 用自己的路径和 token 接入。")}</small>
          </div>
          <code>{`http://127.0.0.1:${payload.port}`}</code>
          <span className="gw-pill" data-tone={payload.helperRunning ? "ok" : "warning"}>{payload.helperRunning ? t("运行中") : t("未运行")}</span>
        </div>
        {payload.agents.map((agent) => {
          const binding = payload.gateway.bindings[agent.agent];
          return (
            <div className="gw-setting-row" key={agent.agent}>
              <div>
                <strong>{tf("{0} 接入地址", [agent.name])}</strong>
                <small>{agent.enabled ? t("已写入该 Agent 的配置") : t("开启后写入该 Agent 的配置")}</small>
              </div>
              <code title={agent.baseUrl}>{agent.baseUrl}</code>
              {binding?.token ? (
                <button className="gw-text-button" disabled={busy !== null} onClick={() => void run(`rotate:${agent.agent}`, "rotate_gateway_token", { agent: agent.agent }, t("更换 token"))} title={t("换一个新的本地 token，旧 token 立即失效")} type="button">
                  {t("更换 token")}
                </button>
              ) : <span />}
            </div>
          );
        })}
      </div>

      <SectionLabel>{t("网关选项")}</SectionLabel>
      <div className="gw-card">
        <div className="gw-setting-row">
          <div>
            <strong>{t("多 Key 轮换")}</strong>
            <small>{mode === "roundRobin" ? t("每个请求换下一个 Key，用量更平均，但上游缓存命中会变低。") : t("同一会话固定一个 Key，出错才换；保住上游的提示词缓存。")}</small>
          </div>
          <select className="gw-select" disabled={busy !== null} onChange={(event) => saveOptions(event.currentTarget.value, restoreOnExit)} value={mode}>
            <option value="sticky">{t("会话粘性（推荐）")}</option>
            <option value="roundRobin">{t("逐请求轮换")}</option>
          </select>
        </div>
        <div className="gw-setting-row">
          <div>
            <strong>{t("退出 Codex++ 时恢复直连")}</strong>
            <small>{t("网关只在 Codex++ 运行时可用。打开后退出时把 Agent 切回直连，下次启动再自动指回网关。")}</small>
          </div>
          <span />
          <Switch disabled={busy !== null} label={t("退出 Codex++ 时恢复直连")} on={restoreOnExit} onToggle={() => saveOptions(mode, !restoreOnExit)} />
        </div>
      </div>
    </>
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
