/** 本地网关：让 Claude Code、Grok 经 Codex++ 使用统一的供应商池，支持多 Key 轮换与模型组。 */
import { useEffect, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { appConfirm } from "./app-dialog";
import { AlertTriangle, ChevronDown, ChevronRight, ChevronUp, Plus, RefreshCw, Search, Waypoints, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { t, tf } from "./i18n";
import {
  agentName,
  groupModeDescription,
  groupModeLabel,
  groupModelId,
  memberLabel,
  memberStatus,
  modelDispatch,
  modelDisplay,
  prettyModelName,
  slugify,
  type GatewayGroupEntry,
  type GatewayPayload,
  type GatewayResult,
  type GroupMember,
  type GroupMode,
} from "./gateway-model";
import { AgentsView, Floating, ModelList } from "./GatewayAgents";
import { AppSelect } from "./AppSelect";
import { ProvidersView } from "./GatewayProviders";
import { SessionsView } from "./GatewaySessions";
import { UsageView } from "./GatewayUsage";
import { Avatar, MemberStatusLabel, SectionLabel, Switch, type Notify, type Run } from "./gateway-ui";
import { TopbarActions } from "./TopbarActions";
import "./GatewayPage.css";

type Tab = "agents" | "sessions" | "providers" | "routing" | "usage" | "gateway";
export type GatewayTab = Tab;

const memo: { payload: GatewayPayload | null; tab: Tab } = { payload: null, tab: "agents" };

/** 模型调度页刷新供应商状态的间隔。 */
const LIVE_REFRESH_MS = 2500;

function hasPayload(result: GatewayResult): result is GatewayResult & GatewayPayload {
  return Boolean(result.gateway && Array.isArray(result.pool));
}

/**
 * 传入 `tab` 时由外层侧栏决定当前页签（不再显示页内标签条），页内跳转通过 `onTabChange` 交还外层；
 * 不传时保持原来的自管页签。
 */
export function GatewayPage({
  notify,
  tab: controlledTab,
  onTabChange,
  extraSettings,
}: {
  notify: Notify;
  tab?: Tab;
  onTabChange?: (tab: Tab) => void;
  /** 「网关设置」末尾附加的内容：主窗口传进来的新旧共用本地服务设置。 */
  extraSettings?: ReactNode;
}) {
  const [payload, setPayload] = useState<GatewayPayload | null>(memo.payload);
  const [busy, setBusy] = useState<string | null>(memo.payload ? null : "load");
  const [ownTab, setTabState] = useState<Tab>(memo.tab);
  const tab = controlledTab ?? ownTab;
  const setTab = (next: Tab) => {
    if (controlledTab) {
      onTabChange?.(next);
      return;
    }
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

  // 窗口回到前台时静默读一次：别的工具可能趁这时改了 Agent 的配置（被接管提示要跟上）。
  useEffect(() => {
    let cancelled = false;
    const onFocus = () => {
      void invoke<GatewayResult>("load_gateway")
        .then((result) => {
          if (!cancelled) accept(result);
        })
        .catch(() => undefined);
    };
    window.addEventListener("focus", onFocus);
    return () => {
      cancelled = true;
      window.removeEventListener("focus", onFocus);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // 模型调度页定时刷新供应商状态：静默读取，不占用 busy，也不弹错误。
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
    { id: "sessions", label: t("会话") },
    { id: "providers", label: t("供应商") },
    { id: "routing", label: t("模型调度") },
    { id: "usage", label: t("用量") },
    { id: "gateway", label: t("网关") },
  ];

  const refreshButton = (
    <button aria-label={t("刷新")} className="gw-icon-button" disabled={busy !== null} onClick={() => void refresh(false)} title={t("刷新")} type="button">
      <RefreshCw className={`h-4 w-4 ${busy === "load" ? "animate-spin" : ""}`} />
    </button>
  );

  return (
    <div className="mg-page gateway-page">
      {/* 页面标题由外层框架显示，这里只放分段标签和刷新。 */}
      {controlledTab ? (
        // 由外层侧栏控制时不显示页内工具条，刷新按钮放进外壳顶栏。
        <TopbarActions>{refreshButton}</TopbarActions>
      ) : (
        <div className="gw-toolbar">
          <nav aria-label={t("本地网关")} className="gw-tabs" role="tablist">
            {tabs.map((item) => (
              <button aria-selected={tab === item.id} data-active={tab === item.id} key={item.id} onClick={() => setTab(item.id)} role="tab" type="button">
                {item.label}
              </button>
            ))}
          </nav>
          {refreshButton}
        </div>
      )}
      <div className="mg-content gw-content">
        {payload && !payload.helperRunning && payload.agents.some((agent) => agent.enabled) ? (
          <p className="gw-alert" role="alert">
            <AlertTriangle className="h-4 w-4" />
            <span>{t("本地网关当前没有运行，走网关的 Agent 会连不上。请重新保存一次绑定或重启 Codex++。")}</span>
          </p>
        ) : null}
        {tab === "agents" ? <AgentsView busy={busy} notify={notify} onManage={() => setTab("providers")} payload={payload} run={run} /> : null}
        {tab === "sessions" ? <SessionsView notify={notify} /> : null}
        {tab === "providers" ? <ProvidersView busy={busy} notify={notify} payload={payload} run={run} /> : null}
        {tab === "routing" ? <RoutingView busy={busy} onManage={() => setTab("providers")} payload={payload} run={run} /> : null}
        {tab === "usage" ? <UsageView busy={busy} notify={notify} payload={payload} run={run} /> : null}
        {tab === "gateway" ? <GatewayView busy={busy} payload={payload} run={run} /> : null}
        {tab === "gateway" && extraSettings ? extraSettings : null}
      </div>
    </div>
  );
}

/* —————————————————————— 模型调度 —————————————————————— */

/** 调度表默认显示的行数，搜索或展开后显示全部。 */
const DISPATCH_PREVIEW = 12;

function RoutingView({ payload, busy, run, onManage }: { payload: GatewayPayload | null; busy: string | null; run: Run; onManage: () => void }) {
  const [editing, setEditing] = useState<GatewayGroupEntry | "new" | null>(null);
  const remove = async (group: GatewayGroupEntry) => {
    const ok = await appConfirm(tf("删除模型组「{0}」？引用它的其它组会去掉这个成员。", [group.name]), { title: t("删除模型组"), confirmText: t("删除"), danger: true });
    if (ok) await run(`group:${group.id}`, "delete_gateway_group", { id: group.id }, t("删除模型组"));
  };
  return (
    <>
      <DispatchTable onManage={onManage} payload={payload} />

      <SectionLabel
        aside={(
          <>
            <em>{t("Agent 可以把模型组当作一个模型来选")}</em>
            <button className="gw-text-button strong" disabled={busy !== null} onClick={() => setEditing("new")} type="button">{t("新建模型组")}</button>
          </>
        )}
      >
        {t("模型组")}
      </SectionLabel>
      <div className="gw-card">
        {(payload?.groups ?? []).map((group) => {
          const joiner = group.mode === "inOrder" ? " → " : " · ";
          const icons = group.members
            .map((member) => modelDisplay(payload, member.model))
            .filter((view) => !view.group && view.sub)
            .slice(0, 3);
          return (
            <div className="gw-group-row" key={group.id}>
              <span className="gw-avatar-stack">
                {icons.length ? icons.map((view, index) => <Avatar key={`${view.sub}-${index}`} name={view.sub} size="sm" />) : <Waypoints className="h-4 w-4" />}
              </span>
              <div className="gw-group-main">
                <div>
                  <strong>{group.name}</strong>
                  <code className="gw-chip">{groupModelId(group.id)}</code>
                </div>
                <small>{group.members.length ? group.members.map((member) => memberLabel(payload, member.model)).join(joiner) : t("没有成员")}</small>
              </div>
              <span className="gw-pill gw-pill-outline">{t(groupModeLabel(group.mode))}</span>
              <span className="gw-row-buttons">
                <button className="gw-text-button" disabled={busy !== null} onClick={() => setEditing(group)} type="button">{t("编辑")}</button>
                <button className="gw-text-button" disabled={busy !== null} onClick={() => void remove(group)} type="button">{t("删除")}</button>
              </span>
            </div>
          );
        })}
        {payload && payload.groups.length === 0 ? (
          <div className="gw-empty">
            <strong>{t("还没有模型组")}</strong>
            <span>{t("把几个模型编成一组，Agent 在模型列表里把它当一个模型来选：按顺序、轮换或按权重回答，组里也可以再放组。")}</span>
          </div>
        ) : null}
      </div>
      {editing ? (
        <GroupEditor
          busy={busy}
          group={editing === "new" ? null : editing}
          onCancel={() => setEditing(null)}
          onSave={async (input) => {
            if (await run("group:save", "save_gateway_group", { group: input }, t("保存模型组"))) setEditing(null);
          }}
          payload={payload}
        />
      ) : null}
    </>
  );
}

/** 每个模型名一行，列出提供它的供应商；从左到右就是网关的尝试顺序。 */
function DispatchTable({ payload, onManage }: { payload: GatewayPayload | null; onManage: () => void }) {
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState(false);
  const rows = modelDispatch(payload);
  const needle = query.trim().toLowerCase();
  const matched = needle
    ? rows.filter((row) => row.model.toLowerCase().includes(needle) || row.providers.some((provider) => provider.name.toLowerCase().includes(needle)))
    : rows;
  const shown = needle || expanded ? matched : matched.slice(0, DISPATCH_PREVIEW);
  const backed = rows.filter((row) => row.providers.length > 1).length;
  return (
    <>
      <SectionLabel
        aside={(
          <button className="gw-text-button strong" onClick={onManage} type="button">{t("调整顺序")}</button>
        )}
      >
        {t("模型调度")}
        <em>{tf("{0} 个模型 · {1} 个有备用", [rows.length, backed])}</em>
      </SectionLabel>
      <div className="gw-card gw-dispatch">
        <p className="gw-dispatch-lead">
          {t("Agent 请求一个模型名时，网关从左往右依次尝试提供它的供应商：前一家出错、Key 都在冷却或被熔断，就换下一家。顺序跟着供应商池走，在池里拖动即可调整。")}
        </p>
        {rows.length > DISPATCH_PREVIEW ? (
          <div className="gw-dispatch-search">
            <Search aria-hidden className="h-4 w-4" />
            <input aria-label={t("搜索模型或供应商")} onChange={(event) => setQuery(event.target.value)} placeholder={t("搜索模型或供应商")} value={query} />
          </div>
        ) : null}
        {rows.length === 0 ? (
          <div className="gw-empty">
            <strong>{t("供应商池里还没有模型")}</strong>
            <span>{t("在「供应商池」添加并打开供应商后，这里列出每个模型会发给谁。")}</span>
          </div>
        ) : null}
        {shown.map((row) => (
          <div className="gw-dispatch-row" key={row.model}>
            <div className="gw-dispatch-model">
              <strong title={row.model}>{prettyModelName(row.model)}</strong>
              <code>{row.model}</code>
            </div>
            <ol className="gw-dispatch-chain">
              {row.providers.map((provider, index) => {
                const status = memberStatus(payload, provider.id);
                const note = provider.excludedBy.length ? tf("{0} 不用这家", [provider.excludedBy.map(agentName).join("、")]) : undefined;
                return (
                  <li data-excluded={provider.excludedBy.length > 0} data-state={status.state} key={provider.id} title={note}>
                    {index > 0 ? <ChevronRight aria-hidden className="gw-dispatch-arrow h-3.5 w-3.5" /> : null}
                    <span className="gw-dispatch-step">
                      <i>{index + 1}</i>
                      <Avatar baseUrl={provider.baseUrl} name={provider.name} size="sm" />
                      <b>{provider.name}</b>
                      {status.state === "idle" ? null : <small><MemberStatusLabel status={status} /></small>}
                    </span>
                  </li>
                );
              })}
            </ol>
          </div>
        ))}
        {!needle && matched.length > DISPATCH_PREVIEW ? (
          <button className="gw-dispatch-more" onClick={() => setExpanded(!expanded)} type="button">
            {expanded ? t("收起") : tf("显示全部 {0} 个模型", [matched.length])}
          </button>
        ) : null}
        {needle && matched.length === 0 ? <p className="gw-dispatch-none">{t("没有匹配的模型")}</p> : null}
      </div>
    </>
  );
}

/** 新建 / 编辑模型组：名称、模式、成员（池里的模型或别的组）。 */
function GroupEditor({
  payload,
  group,
  busy,
  onSave,
  onCancel,
}: {
  payload: GatewayPayload | null;
  group: GatewayGroupEntry | null;
  busy: string | null;
  onSave: (input: { id: string; name: string; mode: GroupMode; members: GroupMember[] }) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(group?.name ?? "");
  const [mode, setMode] = useState<GroupMode>(group?.mode ?? "inOrder");
  const [members, setMembers] = useState<GroupMember[]>(group?.members.map((member) => ({ ...member })) ?? []);
  const [picking, setPicking] = useState(false);
  const addRef = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !picking) onCancel();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onCancel, picking]);
  const move = (index: number, delta: number) => {
    const next = [...members];
    const [item] = next.splice(index, 1);
    next.splice(Math.max(0, Math.min(next.length, index + delta)), 0, item);
    setMembers(next);
  };
  const error = !name.trim() ? t("请填写模型组名称") : members.length === 0 ? t("模型组至少要有一个成员") : "";
  const modes: Array<{ value: GroupMode; label: string }> = [
    { value: "inOrder", label: t("依次") },
    { value: "rotate", label: t("轮换") },
    { value: "weighted", label: t("按权重") },
  ];
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onCancel(); }}>
      <form
        className="gw-modal gw-group-modal"
        onSubmit={(event) => {
          event.preventDefault();
          if (!error) onSave({ id: group?.id ?? "", name: name.trim(), mode, members });
        }}
      >
        <header>
          <span className="gw-avatar gw-avatar-plus"><Waypoints className="h-4 w-4" /></span>
          <strong>{group ? tf("编辑 {0}", [group.name]) : t("新建模型组")}</strong>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onCancel} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body">
          <label className="gw-field">
            <span>{t("名称")} <small>{group ? groupModelId(group.id) : name.trim() ? groupModelId(slugify(name)) : ""}</small></span>
            <Input autoFocus onChange={(event) => setName(event.currentTarget.value)} placeholder={t("例如 日常编码")} value={name} />
          </label>
          <div className="gw-field">
            <span>{t("模式")}</span>
            <div className="gw-tabs gw-mode-tabs" role="radiogroup">
              {modes.map((item) => (
                <button aria-checked={mode === item.value} data-active={mode === item.value} key={item.value} onClick={() => setMode(item.value)} role="radio" type="button">
                  {item.label}
                </button>
              ))}
            </div>
            <small className="gw-muted">{t(groupModeDescription(mode))}</small>
          </div>
          <div className="gw-field">
            <span>{t("成员")} <small>{mode === "inOrder" ? t("从上到下依次尝试") : t("答不了时按列表顺序换下一个")}</small></span>
            <div className="gw-member-list">
              {members.map((member, index) => {
                const view = modelDisplay(payload, member.model);
                return (
                  <div className="gw-member-edit" key={`${member.model}-${index}`}>
                    {view.group ? <Waypoints className="gw-picker-glyph h-4 w-4" /> : <Avatar name={view.sub || view.title} size="sm" />}
                    <span className="gw-member-edit-name">
                      <strong>{view.title}</strong>
                      <small>{view.group ? view.sub : view.sub || t("池里还没有提供它的供应商")}</small>
                    </span>
                    {mode === "weighted" ? (
                      <label className="gw-inline-field">
                        <span>{t("权重")}</span>
                        <input
                          className="gw-inline-input gw-weight-input"
                          min={1}
                          onChange={(event) => {
                            const next = [...members];
                            next[index] = { ...member, weight: Math.max(1, Number(event.currentTarget.value) || 1) };
                            setMembers(next);
                          }}
                          type="number"
                          value={member.weight}
                        />
                      </label>
                    ) : null}
                    <button aria-label={t("上移")} className="gw-icon-button" disabled={index === 0} onClick={() => move(index, -1)} type="button"><ChevronUp className="h-4 w-4" /></button>
                    <button aria-label={t("下移")} className="gw-icon-button" disabled={index === members.length - 1} onClick={() => move(index, 1)} type="button"><ChevronDown className="h-4 w-4" /></button>
                    <button aria-label={t("移除")} className="gw-icon-button" onClick={() => setMembers(members.filter((_, position) => position !== index))} type="button"><X className="h-4 w-4" /></button>
                  </div>
                );
              })}
              <button className="gw-add-row" onClick={() => setPicking(!picking)} ref={addRef} type="button">
                <Plus className="h-4 w-4" />
                {t("添加成员")}
              </button>
              <Floating anchorRef={addRef} className="gw-model-popover" onClose={() => setPicking(false)} open={picking} width={520}>
                <ModelList
                  exclude={[]}
                  onPick={(item) => {
                    setPicking(false);
                    if (!members.some((member) => member.model === item.model)) setMembers([...members, { model: item.model, weight: 1 }]);
                  }}
                  payload={payload}
                  selectedModel=""
                  skipGroup={group?.id}
                />
              </Floating>
            </div>
          </div>
        </div>
        <footer>
          <span className="gw-error">{error}</span>
          <Button onClick={onCancel} size="sm" type="button" variant="ghost">{t("取消")}</Button>
          <Button disabled={busy !== null || Boolean(error)} size="sm" type="submit">{busy ? t("保存中") : t("保存")}</Button>
        </footer>
      </form>
    </div>,
    document.body,
  );
}

/* —————————————————————— 网关 —————————————————————— */

function GatewayView({ payload, busy, run }: { payload: GatewayPayload | null; busy: string | null; run: Run }) {
  if (!payload) return null;
  const mode = payload.gateway.keyPolicy.mode;
  const restoreOnExit = payload.gateway.restoreOnExit;
  const trayPanel = payload.gateway.trayPanel !== false;
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
          <span />
          <AppSelect
            disabled={busy !== null}
            label={t("多 Key 轮换")}
            onChange={(next) => saveOptions(next, restoreOnExit)}
            options={[
              { value: "sticky", label: t("会话粘性（推荐）"), hint: t("同一会话固定一个 Key，出错才换") },
              { value: "roundRobin", label: t("逐请求轮换"), hint: t("每个请求换下一个 Key") },
            ]}
            value={mode}
          />
        </div>
        <div className="gw-setting-row">
          <div>
            <strong>{t("退出 Codex++ 时恢复直连")}</strong>
            <small>{t("网关只在 Codex++ 运行时可用。打开后退出时把 Agent 切回直连，下次启动再自动指回网关。")}</small>
          </div>
          <span />
          <Switch disabled={busy !== null} label={t("退出 Codex++ 时恢复直连")} on={restoreOnExit} onToggle={() => saveOptions(mode, !restoreOnExit)} />
        </div>
        <div className="gw-setting-row">
          <div>
            <strong>{t("点击托盘图标弹出快捷面板")}</strong>
            <small>{t("面板里能直接给各 Agent 换模型、看最近的请求；右键仍是原来的菜单，双击打开主窗口。关掉恢复原来的点击行为。")}</small>
          </div>
          <span />
          <Switch
            disabled={busy !== null}
            label={t("点击托盘图标弹出快捷面板")}
            on={trayPanel}
            onToggle={() => void (async () => {
              if (await run("tray-panel", "save_gateway_tray_panel", { enabled: !trayPanel }, t("保存网关选项"), true)) await invoke("refresh_tray_mode");
            })()}
          />
        </div>
      </div>
    </>
  );
}
