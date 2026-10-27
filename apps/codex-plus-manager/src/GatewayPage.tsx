/** 本地网关：让 Claude Code、Grok 经 Codex++ 使用统一的供应商池，支持多 Key 轮换与模型组。 */
import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { invoke } from "@tauri-apps/api/core";
import { appConfirm } from "./app-dialog";
import { AlertTriangle, Asterisk, ChevronDown, ChevronRight, ChevronUp, Plus, RefreshCw, Search, Waypoints, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { t, tf } from "./i18n";
import {
  agentName,
  formatElapsed,
  groupModeDescription,
  groupModeLabel,
  groupModelId,
  groupStickyDescription,
  groupStickyLabel,
  isPattern,
  memberLabel,
  parseWeight,
  dispatchStatus,
  modelDispatch,
  modelDisplay,
  patternMatches,
  prettyModelName,
  speedOf,
  slugify,
  type GatewayGroupEntry,
  type GatewayPayload,
  type GatewayResult,
  type GroupMember,
  type GroupMode,
  type GroupSticky,
  agentPoolProviders,
  maskAccountName,
  type DispatchRow,
  type MemberStatus,
} from "./gateway-model";
import { AgentsView, Floating, ModelList } from "./GatewayAgents";
import { AppSelect } from "./AppSelect";
import { ProvidersView } from "./GatewayProviders";
import { SessionsView } from "./GatewaySessions";
import { UsageView } from "./GatewayUsage";
import { Avatar, MemberStatusLabel, SectionLabel, Switch, makeRun, type Notify, type Run } from "./gateway-ui";
import { TopbarActions } from "./TopbarActions";
import { GatewayNavContext, gatewayJump } from "./gateway-store";
import { useEscapeLayer } from "./use-escape-layer";
import { publishGatewayPayload } from "./gateway-store";
import "./GatewayPage.css";

type Tab = "agents" | "sessions" | "providers" | "routing" | "usage" | "gateway";
export type GatewayTab = Tab;

const memo: { payload: GatewayPayload | null; tab: Tab } = { payload: null, tab: "agents" };

/** 组编辑器的模型选择不排除任何供应商；模块级常量，避免每次渲染都换新数组让列表重算。 */
const NO_EXCLUDE: string[] = [];

/** 「模型」页刷新供应商状态的间隔。 */
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
      // 侧栏状态卡、总览共用的数据也立刻更新，不等它们下次轮询。
      publishGatewayPayload(result);
    }
  };

  const run: Run = makeRun(setBusy, accept, notify);

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

  // 「模型」页定时刷新供应商状态：静默读取，不占用 busy，也不弹错误。
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

  // 按数据流排：供应商 → 模型 → Agent。
  const tabs: Array<{ id: Tab; label: string }> = [
    { id: "providers", label: t("供应商") },
    { id: "routing", label: t("模型") },
    { id: "agents", label: t("智能体") },
    { id: "sessions", label: t("会话") },
    { id: "usage", label: t("用量") },
    { id: "gateway", label: t("网关") },
  ];

  // 跨页跳转：「模型」页列头 → 供应商池里展开这一家；模型下拉的「模型组」→「模型」页的模型组页签。
  const openProvider = (id: string) => {
    gatewayJump.provider = id;
    setTab("providers");
  };
  const nav = {
    openModelGroups: () => {
      gatewayJump.modelsTab = "groups";
      setTab("routing");
    },
  };

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
      <GatewayNavContext.Provider value={nav}>
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
        {tab === "routing" ? <RoutingView busy={busy} onManage={() => setTab("providers")} onOpenProvider={openProvider} payload={payload} run={run} /> : null}
        {tab === "usage" ? <UsageView busy={busy} notify={notify} payload={payload} run={run} /> : null}
        {tab === "gateway" ? <GatewayView busy={busy} payload={payload} run={run} /> : null}
        {tab === "gateway" && extraSettings ? extraSettings : null}
      </div>
      </GatewayNavContext.Provider>
    </div>
  );
}

/* —————————————————————— 模型 —————————————————————— */

/** 模型矩阵默认显示的行数，搜索、筛选或展开后显示全部。 */
const MATRIX_PREVIEW = 30;

type ModelsTab = "models" | "groups";
type MatrixFilter = "all" | "shared" | `provider:${string}`;

const modelsMemo: { tab: ModelsTab } = { tab: "models" };

/** 「模型」页：网关里能用的模型（供应商 × 模型矩阵）和模型组，两个页签。 */
function RoutingView({ payload, busy, run, onManage, onOpenProvider }: { payload: GatewayPayload | null; busy: string | null; run: Run; onManage: () => void; onOpenProvider: (id: string) => void }) {
  const [tab, setTabState] = useState<ModelsTab>(() => {
    // 从别的页跳过来指定了页签（如模型下拉里的「管理模型组」）：用它，读完清掉。
    const jumped = gatewayJump.modelsTab;
    gatewayJump.modelsTab = null;
    if (jumped) modelsMemo.tab = jumped;
    return modelsMemo.tab;
  });
  const setTab = (next: ModelsTab) => {
    modelsMemo.tab = next;
    setTabState(next);
  };
  const rows = useMemo(() => modelDispatch(payload), [payload]);
  const groups = payload?.groups ?? [];
  return (
    <>
      <div className="gw-models-tabs" role="tablist" aria-label={t("模型")}>
        <button aria-selected={tab === "models"} data-active={tab === "models"} onClick={() => setTab("models")} role="tab" type="button">
          {t("模型")}<em>{payload ? rows.length : "…"}</em>
        </button>
        <button aria-selected={tab === "groups"} data-active={tab === "groups"} onClick={() => setTab("groups")} role="tab" type="button">
          {t("模型组")}<em>{payload ? groups.length : "…"}</em>
        </button>
      </div>
      {tab === "models" ? <ModelMatrix busy={busy} onManage={onManage} onOpenProvider={onOpenProvider} payload={payload} rows={rows} run={run} /> : <GroupsPanel busy={busy} payload={payload} run={run} />}
    </>
  );
}

/** 模型组页签：列表 + 新建 / 编辑 / 删除。 */
function GroupsPanel({ payload, busy, run }: { payload: GatewayPayload | null; busy: string | null; run: Run }) {
  const [editing, setEditing] = useState<GatewayGroupEntry | "new" | null>(null);
  const remove = async (group: GatewayGroupEntry) => {
    const ok = await appConfirm(tf("删除模型组「{0}」？引用它的其它组会去掉这个成员。", [group.name]), { title: t("删除模型组"), confirmText: t("删除"), danger: true });
    if (ok) await run(`group:${group.id}`, "delete_gateway_group", { id: group.id }, t("删除模型组"));
  };
  return (
    <>
      <div className="gw-models-toolbar">
        <p className="gw-models-hint">{t("把几个模型编成一组，Agent 在模型列表里把它当一个模型来选：按顺序、轮换、按权重、额度或速度回答，组里也可以再放组。")}</p>
        <button className="gw-primary-button" disabled={busy !== null || !payload} onClick={() => setEditing("new")} type="button">
          <Plus className="h-4 w-4" />
          {t("新建模型组")}
        </button>
      </div>
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
              <span className="gw-pill gw-pill-outline" title={t(groupStickyDescription(group.sticky))}>{t(groupStickyLabel(group.sticky))}</span>
              <span className="gw-row-buttons">
                <button className="gw-text-button" disabled={busy !== null} onClick={() => setEditing(group)} type="button">{t("编辑")}</button>
                <button className="gw-text-button" disabled={busy !== null} onClick={() => void remove(group)} type="button">{t("删除")}</button>
              </span>
            </div>
          );
        })}
        {!payload ? <div className="gw-empty">{t("正在读取…")}</div> : null}
        {payload && payload.groups.length === 0 ? (
          <div className="gw-empty">
            <strong>{t("还没有模型组")}</strong>
            <span>{t("比如把两家的同一个模型编成「依次」组：第一家出错自动换第二家；或者把几个便宜模型编成「轮换」组分摊用量。")}</span>
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

/** 供应商名的短写法：`·` 前那段（「ChatGPT Pro · 邮箱」→「ChatGPT Pro」），完整名放在悬停提示里。 */
function shortProviderName(name: string): string {
  return name.split("·")[0].trim() || name;
}

/**
 * 供应商 × 模型矩阵：行是模型，列是池里打开的供应商（顺序就是池的顺序），格子里有点就是这家提供。
 * 打开「多家自动切换」时，多家都有的那几行点里标出「自动」的尝试顺序。
 */
function ModelMatrix({ payload, rows, onManage, onOpenProvider, busy, run }: { payload: GatewayPayload | null; rows: DispatchRow[]; onManage: () => void; onOpenProvider: (id: string) => void; busy: string | null; run: Run }) {
  const autoModels = payload?.gateway.autoModels === true;
  const [query, setQuery] = useState("");
  const [filter, setFilter] = useState<MatrixFilter>("all");
  const [expanded, setExpanded] = useState(false);
  const providers = useMemo(() => agentPoolProviders(payload), [payload]);
  const shared = rows.filter((row) => row.providers.length > 1).length;
  const needle = query.trim().toLowerCase();
  const matched = rows.filter((row) => {
    if (filter === "shared" && row.providers.length < 2) return false;
    if (filter.startsWith("provider:") && !row.providers.some((provider) => `provider:${provider.id}` === filter)) return false;
    return !needle || row.model.toLowerCase().includes(needle) || prettyModelName(row.model).toLowerCase().includes(needle);
  });
  const shown = needle || expanded || filter !== "all" ? matched : matched.slice(0, MATRIX_PREVIEW);
  const filters: Array<{ key: MatrixFilter; label: ReactNode; count: number; title?: string }> = [
    { key: "all", label: t("全部"), count: rows.length },
    { key: "shared", label: t("多家都有"), count: shared },
    ...providers.map((entry) => ({
      key: `provider:${entry.id}` as MatrixFilter,
      label: <><Avatar baseUrl={entry.baseUrl} name={entry.name} size="sm" />{maskAccountName(shortProviderName(entry.name))}</>,
      count: rows.filter((row) => row.providers.some((provider) => provider.id === entry.id)).length,
      title: maskAccountName(entry.name),
    })),
  ];
  return (
    <>
      <div className="gw-models-toolbar">
        <label className="gw-sheet-search gw-models-search">
          <Search className="h-4 w-4" />
          <input aria-label={t("搜索模型")} onChange={(event) => setQuery(event.target.value)} placeholder={t("搜索模型")} value={query} />
        </label>
        <div className="gw-models-auto" title={t("多家都有的模型额外列一份「自动」：按供应商池的顺序依次尝试，前一家出错、Key 都在冷却或被熔断就换下一家。关着时只能选具体某一家。")}>
          <span>{t("多家自动切换")}</span>
          <Switch
            disabled={busy !== null || !payload}
            label={t("多家自动切换")}
            on={autoModels}
            onToggle={() => void run("auto-models", "save_gateway_auto_models", { enabled: !autoModels }, t("多家自动切换"), true)}
          />
        </div>
      </div>
      <div className="gw-models-filters" role="group" aria-label={t("筛选")}>
        {filters.map((item) => (
          <button data-active={filter === item.key} key={item.key} onClick={() => setFilter(filter === item.key ? "all" : item.key)} title={item.title} type="button">
            {item.label}
            <em>{item.count}</em>
          </button>
        ))}
      </div>
      <p className="gw-models-hint">
        {autoModels ? (
          <>
            {t("多家都有的模型，Agent 里每家各一份（只发给那家），另有一份「自动」：按列从左到右依次尝试，数字是先后。")}
            <button className="gw-text-button" onClick={onManage} type="button">{t("调整顺序")}</button>
          </>
        ) : t("多家都有的模型，Agent 里每家各一份，选哪家就只发给哪家。打开「多家自动切换」可以再多一份按顺序自动换家的。")}
      </p>
      <div className="gw-card gw-matrix-card">
        {!payload ? <div className="gw-empty">{t("正在读取…")}</div> : null}
        {payload && rows.length === 0 ? (
          <div className="gw-empty">
            <strong>{payload.pool.length ? t("供应商都已关闭") : t("供应商池里还没有模型")}</strong>
            <span>{payload.pool.length ? t("打开供应商池里的供应商后，这里列出每个模型由哪几家提供。") : t("在「供应商池」添加并打开供应商后，这里列出每个模型由哪几家提供。")}</span>
          </div>
        ) : null}
        {payload && rows.length ? (
          <div className="gw-matrix-scroll">
            <table className="gw-matrix">
              <thead>
                <tr>
                  <th className="gw-matrix-model" scope="col">{t("模型")}</th>
                  {providers.map((entry) => (
                    <th key={entry.id} scope="col">
                      {/* 点列头去供应商池展开这一家，在那里改它的模型列表、Key、价格。 */}
                      <button className="gw-matrix-provider" onClick={() => onOpenProvider(entry.id)} title={tf("在供应商池里打开「{0}」", [maskAccountName(entry.name)])} type="button">
                        <Avatar baseUrl={entry.baseUrl} name={entry.name} size="sm" />
                        <span>{maskAccountName(shortProviderName(entry.name))}</span>
                        <ChevronRight aria-hidden className="h-3.5 w-3.5" />
                      </button>
                    </th>
                  ))}
                </tr>
              </thead>
              <tbody>
                {shown.map((row) => {
                  const multi = row.providers.length > 1;
                  return (
                    <tr data-multi={multi} key={row.model}>
                      <th className="gw-matrix-model" scope="row">
                        <strong title={row.model}>{prettyModelName(row.model)}</strong>
                        <code>{row.model}</code>
                      </th>
                      {providers.map((entry) => {
                        const index = row.providers.findIndex((provider) => provider.id === entry.id);
                        if (index < 0) return <td key={entry.id}><span className="gw-matrix-none" aria-label={t("不提供")}>·</span></td>;
                        const provider = row.providers[index];
                        const status = dispatchStatus(payload, provider.id, row.model);
                        const speed = speedOf(payload, provider.id, row.model);
                        const excluded = provider.excludedBy.length ? tf("{0} 不用这家", [provider.excludedBy.map(agentName).join(t("、"))]) : "";
                        const tips = [maskAccountName(entry.name), status ? statusText(status) : "", speed !== null ? tf("平均首包 {0}", [formatElapsed(speed)]) : "", excluded].filter(Boolean).join(" · ");
                        return (
                          <td key={entry.id} title={tips}>
                            <span className="gw-matrix-cell" data-excluded={provider.excludedBy.length > 0} data-state={status?.state ?? "idle"}>
                              <i>{autoModels && multi ? index + 1 : null}</i>
                              {speed !== null ? <small>{formatElapsed(speed)}</small> : null}
                            </span>
                          </td>
                        );
                      })}
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        ) : null}
        {payload && !needle && filter === "all" && matched.length > MATRIX_PREVIEW ? (
          <button className="gw-dispatch-more" onClick={() => setExpanded(!expanded)} type="button">
            {expanded ? t("收起") : tf("显示全部 {0} 个模型", [matched.length])}
          </button>
        ) : null}
        {payload && rows.length > 0 && matched.length === 0 ? <p className="gw-dispatch-none">{t("没有匹配的模型")}</p> : null}
      </div>
      <p className="gw-models-legend">
        <span><span className="gw-matrix-cell" data-state="idle"><i /></span>{t("提供")}</span>
        <span><span className="gw-matrix-cell" data-state="answered"><i /></span>{t("刚用过")}</span>
        <span><span className="gw-matrix-cell" data-state="cooling"><i /></span>{t("冷却或熔断中")}</span>
        <span><span className="gw-matrix-cell" data-excluded="true" data-state="idle"><i /></span>{t("有 Agent 不用这家")}</span>
      </p>
    </>
  );
}

/** 格子悬停提示里的状态文字。 */
function statusText(status: MemberStatus): string {
  return tf(status.text, status.args.map((arg) => (typeof arg === "string" ? t(arg) : arg)));
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
  onSave: (input: { id: string; name: string; mode: GroupMode; sticky: GroupSticky; members: GroupMember[] }) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(group?.name ?? "");
  const [mode, setMode] = useState<GroupMode>(group?.mode ?? "inOrder");
  const [sticky, setSticky] = useState<GroupSticky>(group?.sticky ?? "cache");
  const [pattern, setPattern] = useState("");
  const [members, setMembers] = useState<GroupMember[]>(group?.members.map((member) => ({ ...member })) ?? []);
  const [picking, setPicking] = useState(false);
  const addRef = useRef<HTMLButtonElement>(null);
  // Esc 栈：「添加成员」浮层、模式下拉开着时 Esc 先关它们。
  useEscapeLayer(true, onCancel);
  // 权重输入框正在编辑的原始文字（允许暂时为空），失焦时再规范成 ≥ 1 的整数。
  const [weightText, setWeightText] = useState<Record<number, string>>({});
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
    { value: "usage", label: t("额度优先") },
    { value: "fast", label: t("速度优先") },
  ];
  const stickies: GroupSticky[] = ["cache", "session", "off"];
  const patternText = pattern.trim();
  const patternHits = isPattern(patternText) ? patternMatches(payload, patternText) : [];
  const addPattern = () => {
    if (!isPattern(patternText)) return;
    if (!members.some((member) => member.model === patternText)) setMembers([...members, { model: patternText, weight: 1 }]);
    setPattern("");
  };
  const memberHint = mode === "inOrder"
    ? t("从上到下依次尝试")
    : mode === "usage" || mode === "fast"
      ? t("额度 / 速度相同时按列表顺序")
      : t("答不了时按列表顺序换下一个");
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onCancel(); }}>
      <form
        className="gw-modal gw-group-modal"
        onSubmit={(event) => {
          event.preventDefault();
          if (!error) onSave({ id: group?.id ?? "", name: name.trim(), mode, sticky, members });
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
            <span>{t("会话固定")}</span>
            <div className="gw-tabs gw-mode-tabs" role="radiogroup">
              {stickies.map((item) => (
                <button aria-checked={sticky === item} data-active={sticky === item} key={item} onClick={() => setSticky(item)} role="radio" type="button">
                  {t(groupStickyLabel(item))}
                </button>
              ))}
            </div>
            <small className="gw-muted">{t(groupStickyDescription(sticky))}</small>
          </div>
          <div className="gw-field">
            <span>{t("成员")} <small>{memberHint}</small></span>
            <div className="gw-member-list">
              {members.map((member, index) => {
                const view = modelDisplay(payload, member.model);
                const wildcard = isPattern(member.model);
                const hits = wildcard ? patternMatches(payload, member.model) : [];
                return (
                  <div className="gw-member-edit" data-pattern={wildcard} key={`${member.model}-${index}`}>
                    {wildcard ? <Asterisk className="gw-picker-glyph h-4 w-4" /> : view.group ? <Waypoints className="gw-picker-glyph h-4 w-4" /> : <Avatar name={view.sub || view.title} size="sm" />}
                    <span className="gw-member-edit-name">
                      <strong>{wildcard ? member.model : view.title}</strong>
                      {wildcard ? (
                        <small title={hits.join("\n")}>{hits.length ? tf("通配符 · 现在匹配 {0} 个模型", [hits.length]) : t("通配符 · 现在没有匹配的模型")}</small>
                      ) : (
                        <small>{view.group ? view.sub : view.sub || t("池里还没有提供它的供应商")}</small>
                      )}
                    </span>
                    {mode === "weighted" ? (
                      <label className="gw-inline-field">
                        <span>{t("权重")}</span>
                        <input
                          className="gw-inline-input gw-weight-input"
                          min={1}
                          onBlur={() => {
                            setWeightText((previous) => {
                              const next = { ...previous };
                              delete next[index];
                              return next;
                            });
                          }}
                          onChange={(event) => {
                            const text = event.currentTarget.value;
                            setWeightText((previous) => ({ ...previous, [index]: text }));
                            // 能读成数的就先记下；删空、输入到一半时保留原值，失焦后显示规范值。
                            const parsed = parseWeight(text);
                            if (parsed !== null) {
                              const next = [...members];
                              next[index] = { ...member, weight: parsed };
                              setMembers(next);
                            }
                          }}
                          type="number"
                          value={weightText[index] ?? String(member.weight)}
                        />
                      </label>
                    ) : null}
                    <button aria-label={t("上移")} className="gw-icon-button" disabled={index === 0} onClick={() => move(index, -1)} type="button"><ChevronUp className="h-4 w-4" /></button>
                    <button aria-label={t("下移")} className="gw-icon-button" disabled={index === members.length - 1} onClick={() => move(index, 1)} type="button"><ChevronDown className="h-4 w-4" /></button>
                    <button aria-label={t("移除")} className="gw-icon-button" onClick={() => setMembers(members.filter((_, position) => position !== index))} type="button"><X className="h-4 w-4" /></button>
                  </div>
                );
              })}
              <div className="gw-add-line">
                <button className="gw-add-row" onClick={() => setPicking(!picking)} ref={addRef} type="button">
                  <Plus className="h-4 w-4" />
                  {t("添加成员")}
                </button>
                <span className="gw-pattern-input">
                  <Asterisk aria-hidden className="h-3.5 w-3.5" />
                  <input
                    aria-label={t("通配符成员")}
                    onChange={(event) => setPattern(event.currentTarget.value)}
                    onKeyDown={(event) => {
                      if (event.key === "Enter") {
                        event.preventDefault();
                        addPattern();
                      }
                    }}
                    placeholder={t("通配符，如 openrouter/*:free")}
                    value={pattern}
                  />
                  {patternText ? (
                    <button className="gw-text-button strong" disabled={!isPattern(patternText)} onClick={addPattern} title={patternHits.slice(0, 20).join("\n")} type="button">
                      {isPattern(patternText) ? tf("添加（{0} 个）", [patternHits.length]) : t("要带 *")}
                    </button>
                  ) : null}
                </span>
              </div>
              <small className="gw-muted">{t("通配符按「供应商 id/模型」或模型名匹配，* 代表任意字符；上游新增的模型会自动算进来。")}</small>
              <Floating anchorRef={addRef} className="gw-model-popover" onClose={() => setPicking(false)} open={picking} width={520}>
                <ModelList
                  exclude={NO_EXCLUDE}
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
      </div>
    </>
  );
}
