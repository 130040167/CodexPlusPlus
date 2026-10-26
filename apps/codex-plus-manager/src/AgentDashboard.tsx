/**
 * Claude Code / Grok 的概览：一条接入状态栏、一排今日数据、最近会话与最近请求两栏、底部配置摘要。
 * 数据都来自现成接口（网关、今日用量、请求日志、会话、MCP、Skills），这里只读不写，
 * 唯一的操作是「在终端启动」「在终端继续」和跳到对应页面。
 */
import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { ArrowUpRight, CheckCircle2, Loader2, Play, TerminalSquare, XCircle } from "lucide-react";
import { t, tf } from "./i18n";
import { agentAccessState, agentUsageStats, recentAgentRequests, type DashboardRequest } from "./agent-dashboard-model";
import { AgentGlyph, useGatewayPayload } from "./HubOverview";
import { formatElapsed, formatTokens, modelDisplay, relativeTime, shortPath, type AgentSessionView, type UsageSummary } from "./gateway-model";
import "./AgentDashboard.css";

type Route = "gateway" | "sessions" | "requestLog" | "context" | "skills";
type Notify = (title: string, message: string, status?: string) => void;
type Result = { status: string; message: string };

export function AgentDashboard({ agent, name, onNavigate, notify }: {
  agent: "claude" | "grok";
  name: string;
  onNavigate: (route: Route) => void;
  notify: Notify;
}) {
  const payload = useGatewayPayload(10_000);
  const [usage, setUsage] = useState<UsageSummary | null>(null);
  const [requests, setRequests] = useState<DashboardRequest[] | null>(null);
  const [sessions, setSessions] = useState<AgentSessionView[] | null>(null);
  const [mcpCount, setMcpCount] = useState<number | null>(null);
  const [skillCount, setSkillCount] = useState<number | null>(null);
  const [launching, setLaunching] = useState(false);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    let cancelled = false;
    const done = <T,>(apply: (value: T) => void) => (value: T) => { if (!cancelled) apply(value); };
    invoke<Result & Partial<UsageSummary>>("load_gateway_usage", { range: "today" })
      .then(done((result) => setUsage(result.totals ? (result as UsageSummary) : null)))
      .catch(() => undefined);
    invoke<{ entries?: Parameters<typeof recentAgentRequests>[0] }>("load_request_logs", { limit: 300 })
      .then(done((result) => setRequests(recentAgentRequests(result.entries ?? [], agent))))
      .catch(done(() => setRequests([])));
    invoke<{ sessions?: AgentSessionView[] }>("list_agent_sessions", { agent })
      .then(done((result) => setSessions([...(result.sessions ?? [])].sort((a, b) => b.updatedAtMs - a.updatedAtMs).slice(0, 5))))
      .catch(done(() => setSessions([])));
    invoke<Result & { config?: { entries?: unknown[] } }>("load_agent_mcp", { agent })
      .then(done((result) => setMcpCount(result.config?.entries?.length ?? 0)))
      .catch(() => undefined);
    invoke<{ skills?: Array<{ installed?: boolean; agents?: string[] }> }>("list_installed_skills")
      .then(done((result) => setSkillCount((result.skills ?? []).filter((skill) => skill.installed && skill.agents?.includes(agent)).length)))
      .catch(() => undefined);
    const timer = window.setInterval(() => setNow(Date.now()), 30_000);
    return () => { cancelled = true; window.clearInterval(timer); };
  }, [agent]);

  const view = payload?.agents.find((item) => item.agent === agent);
  const access = agentAccessState(view);
  const binding = payload?.gateway.bindings[agent];
  const model = view?.enabled && binding ? modelDisplay(payload, binding.model, binding.poolExclude ?? []).title : "";
  const stats = agentUsageStats(usage, agent);

  const launch = async () => {
    setLaunching(true);
    try {
      const result = await invoke<Result>("open_agent_terminal", { agent });
      notify(tf("启动 {0}", [name]), result.message, result.status);
    } catch (error) {
      notify(tf("启动 {0}", [name]), String(error), "failed");
    } finally {
      setLaunching(false);
    }
  };
  const resume = async (session: AgentSessionView) => {
    try {
      const result = await invoke<Result>("resume_agent_session", { agent: session.agent, id: session.id, cwd: session.cwd });
      notify(t("在终端继续"), result.message, result.status);
    } catch (error) {
      notify(t("在终端继续"), String(error), "failed");
    }
  };

  const accessLabel: Record<typeof access, string> = {
    gateway: model ? tf("经 Codex++ 网关 · {0}", [model]) : t("经 Codex++ 网关"),
    drift: view?.drift?.by ? tf("配置被 {0} 改掉了，没有经过网关", [view.drift.by]) : t("配置被别的工具改掉了，没有经过网关"),
    direct: t("未接入网关 · 用它自己的设置"),
    missing: t("读取中…"),
  };

  return (
    <div className="agent-dash">
      <section className="agent-dash-hero">
        <span className="agent-dash-glyph"><AgentGlyph agent={agent} /></span>
        <div className="agent-dash-title">
          <strong>{name}{view?.version ? <small>{view.version}</small> : null}</strong>
          <span data-tone={access}><i aria-hidden="true" />{accessLabel[access]}</span>
        </div>
        <div className="agent-dash-actions">
          <button className="agent-dash-button" data-primary={access === "direct" || access === "drift" ? "true" : undefined} onClick={() => onNavigate("gateway")} type="button">
            {access === "gateway" ? t("管理接入") : access === "drift" ? t("重新接入") : t("接入网关")}
          </button>
          <button className="agent-dash-button" disabled={launching} onClick={() => void launch()} type="button">
            {launching ? <Loader2 aria-hidden="true" className="agent-dash-spin" size={13} /> : <Play aria-hidden="true" size={13} />}
            {t("在终端启动")}
          </button>
        </div>
      </section>

      <dl className="agent-dash-stats">
        <div><dt>{t("今日调用")}</dt><dd>{stats ? stats.calls : "—"}</dd></div>
        <div><dt>{t("成功率")}</dt><dd data-tone={stats?.successRate != null && stats.successRate < 0.9 ? "warn" : "ok"}>{stats?.successRate == null ? "—" : `${Math.round(stats.successRate * 1000) / 10}%`}</dd></div>
        <div><dt>Tokens</dt><dd>{stats ? formatTokens(stats.tokens) : "—"}</dd></div>
        <div><dt>{t("可用模型")}</dt><dd>{view?.enabled ? view.models.length : "—"}</dd></div>
      </dl>

      <div className="agent-dash-grid">
        <section className="agent-dash-card">
          <header>
            <h3>{t("最近会话")}</h3>
            <button onClick={() => onNavigate("sessions")} type="button">{t("全部")}<ArrowUpRight aria-hidden="true" size={13} /></button>
          </header>
          {sessions === null ? <p className="agent-dash-empty">{t("读取中…")}</p> : sessions.length === 0 ? (
            <p className="agent-dash-empty">{tf("还没有 {0} 的本地会话。", [name])}</p>
          ) : (
            <ul>
              {sessions.map((session) => {
                const time = relativeTime(session.updatedAtMs, now);
                return (
                  <li key={session.id}>
                    <span className="agent-dash-main">
                      <strong title={session.title}>{session.title || t("未命名会话")}</strong>
                      <small title={session.cwd}>{shortPath(session.cwd) || "~"}</small>
                    </span>
                    <time>{tf(time.text, time.args)}</time>
                    <button aria-label={t("在终端继续")} className="agent-dash-icon" onClick={() => void resume(session)} title={t("在终端继续")} type="button">
                      <TerminalSquare aria-hidden="true" size={14} />
                    </button>
                  </li>
                );
              })}
            </ul>
          )}
        </section>

        <section className="agent-dash-card">
          <header>
            <h3>{t("最近请求")}</h3>
            <button onClick={() => onNavigate("requestLog")} type="button">{t("请求日志")}<ArrowUpRight aria-hidden="true" size={13} /></button>
          </header>
          {requests === null ? <p className="agent-dash-empty">{t("读取中…")}</p> : requests.length === 0 ? (
            <p className="agent-dash-empty">{view?.enabled ? t("还没有经过网关的请求。在它里面发一条消息试试。") : t("接入网关后，这里显示它的每次请求走了哪家供应商。")}</p>
          ) : (
            <ul>
              {requests.map((request) => (
                <li key={request.id}>
                  <span className="agent-dash-main">
                    <strong title={request.model}>{request.model}</strong>
                    <small>{request.provider ? `→ ${request.provider}` : "—"}</small>
                  </span>
                  <time>{request.running ? t("进行中") : formatElapsed(request.durationMs)}</time>
                  {request.running ? <Loader2 aria-hidden="true" className="agent-dash-spin agent-dash-state" size={14} />
                    : request.success ? <CheckCircle2 aria-label={t("成功")} className="agent-dash-state" data-tone="ok" size={14} />
                      : <XCircle aria-label={t("失败")} className="agent-dash-state" data-tone="err" size={14} />}
                </li>
              ))}
            </ul>
          )}
        </section>
      </div>

      <footer className="agent-dash-foot">
        <button onClick={() => onNavigate("context")} type="button">MCP <b>{mcpCount ?? "—"}</b><ArrowUpRight aria-hidden="true" size={12} /></button>
        <button onClick={() => onNavigate("skills")} type="button">Skills <b>{skillCount ?? "—"}</b><ArrowUpRight aria-hidden="true" size={12} /></button>
        {view?.configPath ? <span title={view.configPath}>{t("配置文件")} <code>{shortPath(view.configPath)}</code></span> : null}
      </footer>
    </div>
  );
}
