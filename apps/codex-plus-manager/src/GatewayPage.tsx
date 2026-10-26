/** 本地网关：让 Claude Code、Grok 经 Codex++ 使用统一的供应商池，支持多 Key 轮换与路由组。 */
import { useEffect, useState, type CSSProperties } from "react";
import { invoke } from "@tauri-apps/api/core";
import { AlertTriangle, RefreshCw, Route as RouteIcon } from "lucide-react";
import { Button } from "@/components/ui/button";
import { t, tf } from "./i18n";
import {
  agentName,
  bindingDisplay,
  groupSlug,
  memberStatus,
  parseTargetValue,
  routeCards,
  routeMembers,
  routeNarrative,
  strategyDescription,
  strategyLabel,
  targetValue,
  type AgentActivityView,
  type GatewayAgentId,
  type GatewayPayload,
  type GatewayResult,
  type MemberStatus,
  type RouteRecordView,
} from "./gateway-model";
import { AgentsView } from "./GatewayAgents";
import { ProvidersView } from "./GatewayProviders";
import { AGENT_ICONS, APP_LOGO_URL, Avatar, SectionLabel, Switch, type Notify, type Run } from "./gateway-ui";
import "./GatewayPage.css";

type Tab = "agents" | "providers" | "routing" | "gateway";

const memo: { payload: GatewayPayload | null; tab: Tab } = { payload: null, tab: "agents" };

/** 路由图里各 Agent 连线的色相。 */
const AGENT_HUES: Record<string, number> = { claude: 18, grok: 210, codex: 250 };
/** 路由页实时刷新间隔。 */
const LIVE_REFRESH_MS = 2500;

function hasPayload(result: GatewayResult): result is GatewayResult & GatewayPayload {
  return Boolean(result.gateway && Array.isArray(result.pool));
}

function statusText(status: MemberStatus): string {
  return status.args.length ? tf(status.text, status.args) : t(status.text);
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
        {tab === "agents" ? <AgentsView busy={busy} notify={notify} onManage={() => setTab("providers")} payload={payload} run={run} /> : null}
        {tab === "providers" ? <ProvidersView busy={busy} onOpenCodexProviders={onOpenCodexProviders} payload={payload} run={run} /> : null}
        {tab === "routing" ? <RoutingView onOpenCodexProviders={onOpenCodexProviders} payload={payload} /> : null}
        {tab === "gateway" ? <GatewayView busy={busy} payload={payload} run={run} /> : null}
      </div>
    </div>
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

  const isPool = parsed?.kind === "pool";
  const title = isPool ? t("供应商池") : group?.name ?? entry?.name ?? parsed?.id ?? "";
  const meta = isPool
    ? tf("{0} 个供应商 · {1} 个 Agent", [members.length, agents.length])
    : group
      ? tf("路由组 · {0} 个成员 · {1} 个 Agent", [group.members.length, agents.length])
      : tf("供应商 · {0} 个 Key · {1} 个 Agent", [entry?.keyCount ?? 1, agents.length]);
  const description = isPool
    ? t("按模型名选供应商：Agent 请求哪个模型，就发给池里提供它的供应商；同一个模型有多家时按池里的顺序依次尝试，每家再按自己的 Key 轮换。")
    : group
      ? t(strategyDescription(group.strategy))
      : payload?.gateway.keyPolicy.mode === "roundRobin"
        ? t("单个供应商：每个请求换下一个 Key，被限流或失效的 Key 自动跳过。")
        : t("单个供应商：同一会话固定用一个 Key 保住缓存，出错才换下一个 Key。");
  const summary = latest?.last
    ? (group || isPool) && latest.last.planned[0]
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
        hubPill={isPool ? t("按模型") : group ? t(strategyLabel(group.strategy)) : keyMode}
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
            <small>{agent.binding ? bindingDisplay(payload, targetValue(agent.binding.target), agent.binding.model, agent.binding.poolExclude).title : ""}</small>
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
