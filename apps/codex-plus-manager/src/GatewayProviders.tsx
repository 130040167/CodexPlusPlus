/** 本地网关 · 供应商：网关自己的供应商池（可开关、排序、编辑、删除），右上角「添加供应商」打开目录弹窗。
 *
 * 与直连模式完全独立：这里的增删改只动 gateway.json；「导入…」是把直连模式的供应商复制一份过来。
 */
import { useEffect, useMemo, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { createPortal } from "react-dom";
import { appConfirm } from "./app-dialog";
import { Check, ChevronRight, Copy, GripVertical, Plus, Search, X } from "lucide-react";
import { t, tf } from "./i18n";
import {
  defaultProviderAgents,
  failureLabel,
  formatRemaining,
  gatewayPresets,
  memberStatus,
  presetAdded,
  presetHost,
  protocolLabel,
  providerEnabled,
  providerInput,
  skippedAgents,
  type GatewayAgentId,
  type GatewayPayload,
  type GatewayPoolEntry,
  type GatewayPreset,
  type PresetSection,
  type ProviderDraft,
  type ProviderPrice,
  maskAccountName,
} from "./gateway-model";
import { AgentIcon, Avatar, KeyCount, MemberStatusLabel, Switch, type Notify, type Run } from "./gateway-ui";
import { ProviderFormDialog, type ProviderFormState } from "./ProviderFormDialog";
import { ProviderExtrasDialog, priceSummary } from "./ProviderExtrasDialog";
import { ProviderModelsDialog } from "./ProviderModelsDialog";

/** 能在网关里直接登录的订阅。 */
type AccountKind = "chatgpt" | "chatgpt-api" | "copilot" | "grok";
type DeviceCode = { deviceCode: string; userCode: string; verificationUri: string; interval: number; expiresIn: number };

const ACCOUNT_LABELS: Record<AccountKind, string> = { chatgpt: "ChatGPT", "chatgpt-api": "ChatGPT API", copilot: "GitHub Copilot", grok: "SuperGrok" };

/** 浏览器登录的订阅各自的命令。 */
const BROWSER_LOGIN_COMMANDS: Record<Exclude<AccountKind, "copilot">, string> = {
  chatgpt: "login_gateway_chatgpt",
  "chatgpt-api": "login_gateway_chatgpt_api",
  grok: "login_gateway_grok",
};

/** 表单弹窗的用途：按预设新建、自定义新建、编辑已有供应商。 */
type FormState = ProviderFormState;

export function ProvidersView({
  payload,
  busy,
  run,
  notify,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  notify: Notify;
}) {
  const pool = payload?.pool ?? [];
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);
  const [dragging, setDragging] = useState<string | null>(null);
  // 编辑已有供应商的表单；新添加走 ProviderAdder。
  const [form, setForm] = useState<FormState | null>(null);
  const openCatalog = () => setAdding(true);
  // 拖动排序中的临时顺序；松手后提交，等新数据回来再清掉。
  const [order, setOrder] = useState<string[] | null>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const needle = query.trim().toLowerCase();
  const ordered = order ? order.map((id) => pool.find((entry) => entry.id === id)).filter((entry): entry is GatewayPoolEntry => Boolean(entry)) : pool;
  const visible = ordered.filter((entry) => !needle || entry.name.toLowerCase().includes(needle) || entry.models.some((model) => model.toLowerCase().includes(needle)));

  /** 按住把手上下拖动排序。用指针事件实现：桌面端 webview 会拦截 HTML5 拖放，原生 draggable 拖不动。 */
  const startDrag = (id: string, event: ReactPointerEvent) => {
    if (event.button !== 0) return;
    event.preventDefault();
    const initial = pool.map((entry) => entry.id);
    let current = initial;
    setExpanded(null);
    setDragging(id);
    setOrder(initial);
    // 落点按其它行的中线算：指针越过哪几行的中线，就排在它们后面。行高不同、列表正在重排时也不会来回跳。
    const move = (moveEvent: PointerEvent) => {
      const others = [...(listRef.current?.querySelectorAll<HTMLElement>("[data-pool-id]") ?? [])]
        .filter((row) => row.dataset.poolId !== id)
        .map((row) => {
          const rect = row.getBoundingClientRect();
          return { id: row.dataset.poolId ?? "", middle: rect.top + rect.height / 2 };
        })
        .sort((left, right) => left.middle - right.middle);
      if (!others.length) return;
      const above = others.filter((row) => row.middle < moveEvent.clientY);
      const next = current.filter((item) => item !== id);
      const index = above.length ? next.indexOf(above[above.length - 1].id) + 1 : next.indexOf(others[0].id);
      next.splice(Math.max(index, 0), 0, id);
      if (next.join("\n") === current.join("\n")) return;
      current = next;
      setOrder(next);
    };
    const finish = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", finish);
      window.removeEventListener("pointercancel", finish);
      document.body.classList.remove("gw-sorting");
      setDragging(null);
      if (current.join("\n") === initial.join("\n")) {
        setOrder(null);
        return;
      }
      void run("reorder", "reorder_gateway_providers", { ids: current }, t("调整顺序"), true).finally(() => setOrder(null));
    };
    document.body.classList.add("gw-sorting");
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", finish);
    window.addEventListener("pointercancel", finish);
  };

  const { login, dialog: loginDialog } = useAccountLogin(run, notify);
  const saveEdit = async (draft: ProviderDraft) => {
    if (form?.mode !== "edit") return;
    if (await run(`edit:${form.entry.id}`, "update_gateway_provider", { id: form.entry.id, input: providerInput(draft) }, t("保存供应商"))) setForm(null);
  };

  return (
    <>
      <div className="gw-provider-search-row">
        <label className="gw-sheet-search">
          <Search className="h-4 w-4" />
          <input onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("查找供应商")} value={query} />
        </label>
        {pool.length ? (
          <button className="gw-primary-button" onClick={openCatalog} type="button">
            <Plus className="h-4 w-4" />
            {t("添加供应商")}
          </button>
        ) : null}
      </div>
      {pool.length ? (
        <div className="gw-card gw-pool-card" ref={listRef}>
          {visible.map((entry) => (
            <PoolRow
              busy={busy}
              dragging={dragging === entry.id}
              entry={entry}
              expanded={expanded === entry.id}
              key={entry.id}
              onGripDown={(event) => startDrag(entry.id, event)}
              onEdit={() => setForm({ mode: "edit", entry })}
              onRelogin={() => void login(entry.account as AccountKind)}
              onToggleExpand={() => setExpanded(expanded === entry.id ? null : entry.id)}
              payload={payload}
              run={run}
            />
          ))}
          {visible.length === 0 ? <div className="gw-empty">{t("没有匹配的供应商")}</div> : null}
        </div>
      ) : null}
      {payload !== null && pool.length === 0 ? (
        <div className="gw-card gw-pool-empty">
          <strong>{t("供应商池还是空的")}</strong>
          <span>{t("登录订阅，或添加厂商 API、中转站的 Key。所有接入的 Agent 共用这里的供应商。")}</span>
          <button className="gw-primary-button" onClick={openCatalog} type="button">
            <Plus className="h-4 w-4" />
            {t("添加供应商")}
          </button>
        </div>
      ) : null}
      {adding ? <ProviderAdder busy={busy} notify={notify} onClose={() => setAdding(false)} payload={payload} run={run} /> : null}
      {loginDialog}
      {form ? <ProviderFormDialog busy={busy} form={form} onCancel={() => setForm(null)} onSubmit={(draft) => void saveEdit(draft)} /> : null}
    </>
  );
}

/** 订阅账号登录（新添加和「重新登录」共用）：返回登录函数和 Copilot 验证码弹窗。 */
function useAccountLogin(run: Run, notify: Notify) {
  const [deviceCode, setDeviceCode] = useState<DeviceCode | null>(null);
  /** 登录订阅账号：ChatGPT / ChatGPT API / SuperGrok 走浏览器登录；Copilot 先显示验证码，再等你在 GitHub 上授权。
   * `skip` 是新添加时取消勾选的 Agent；重新登录已有账号时不传。 */
  const login = async (kind: AccountKind, skip?: GatewayAgentId[]) => {
    const title = tf("登录 {0}", [ACCOUNT_LABELS[kind]]);
    if (kind === "copilot") {
      let code: DeviceCode;
      try {
        const result = await invoke<{ status: string; message: string } & Partial<DeviceCode>>("start_gateway_copilot_login");
        if (result.status !== "ok" || !result.userCode || !result.deviceCode) {
          await notify(tf("{0}失败", [title]), result.message, "failed");
          return;
        }
        code = { deviceCode: result.deviceCode, userCode: result.userCode, verificationUri: result.verificationUri ?? "https://github.com/login/device", interval: result.interval ?? 5, expiresIn: result.expiresIn ?? 900 };
      } catch (error) {
        await notify(t("调用失败"), String(error), "failed");
        return;
      }
      setDeviceCode(code);
      await run("login:copilot", "finish_gateway_copilot_login", { code, skipAgents: skip }, title);
      setDeviceCode(null);
      return;
    }
    void notify(title, t("已在浏览器打开登录页，登录完成后回到这里。5 分钟内有效。"), "info");
    await run(`login:${kind}`, BROWSER_LOGIN_COMMANDS[kind], skip ? { skipAgents: skip } : undefined, title);
  };

  const dialog = deviceCode ? <DeviceCodeDialog code={deviceCode} notify={notify} onClose={() => setDeviceCode(null)} /> : null;
  return { login, dialog };
}

/** 「添加供应商」整套流程：目录弹窗 → 填写表单或订阅登录，顶部可选给哪些 Agent 用。
 * 供应商池和总览都用它；流程结束（添加成功、取消或登录结束）时调 `onClose`。 */
export function ProviderAdder({
  payload,
  busy,
  run,
  notify,
  onClose,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  notify: Notify;
  onClose: () => void;
}) {
  // 这次添加给哪些 Agent 用：按各 Agent 的「添加供应商时自动加入」预选。
  const [picked, setPicked] = useState<GatewayAgentId[]>(() => defaultProviderAgents(payload));
  const skipAgents = skippedAgents(payload, picked);
  const [catalogOpen, setCatalogOpen] = useState(true);
  const [form, setForm] = useState<FormState | null>(null);
  const [loggingIn, setLoggingIn] = useState(false);
  const { login, dialog } = useAccountLogin(run, notify);
  const agentPicks = <AgentPicks onChange={setPicked} payload={payload} picked={picked} />;
  const submit = async (draft: ProviderDraft) => {
    if (!form || form.mode === "edit") return;
    const input = providerInput(draft);
    if (await run("add:create", "create_gateway_provider", { input, source: form.mode === "preset" ? `preset:${form.preset.key}` : "custom", skipAgents }, t("添加供应商"))) onClose();
  };
  return (
    <>
      {catalogOpen ? (
        <ProviderCatalog
          agentPicks={agentPicks}
          busy={busy}
          notify={notify}
          onClose={onClose}
          onCustom={() => { setCatalogOpen(false); setForm({ mode: "custom" }); }}
          onLogin={(kind) => {
            setCatalogOpen(false);
            setLoggingIn(true);
            void login(kind, skipAgents).finally(onClose);
          }}
          onPreset={(preset) => { setCatalogOpen(false); setForm({ mode: "preset", preset }); }}
          payload={payload}
          run={run}
          skipAgents={skipAgents}
        />
      ) : null}
      {loggingIn ? dialog : null}
      {form ? <ProviderFormDialog agentPicks={agentPicks} busy={busy} form={form} onCancel={onClose} onSubmit={(draft) => void submit(draft)} /> : null}
    </>
  );
}

function PoolRow({
  entry,
  payload,
  busy,
  run,
  expanded,
  dragging,
  onToggleExpand,
  onGripDown,
  onEdit,
  onRelogin,
}: {
  entry: GatewayPoolEntry;
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  expanded: boolean;
  dragging: boolean;
  onToggleExpand: () => void;
  onGripDown: (event: ReactPointerEvent) => void;
  onEdit: () => void;
  onRelogin: () => void;
}) {
  const enabled = providerEnabled(entry);
  const status = memberStatus(payload, entry.id);
  const failures = (payload?.health.keys ?? []).filter((key) => key.relayId === entry.id && key.state !== "ok");
  const breaker = payload?.health.providers.some((item) => item.relayId === entry.id && item.breakerOpen);
  const tone = !enabled ? "off" : status.state === "cooling" ? "error" : "on";
  const sub = (entry.account
    ? [t("订阅登录"), entry.lentToCodex ? t("Codex 正用它登录") : "", tf("{0} 个模型", [entry.models.length])]
    : [protocolLabel(entry.protocol), presetHost(entry.baseUrl), tf("{0} 个模型", [entry.models.length])]
  ).filter(Boolean).join(" · ");
  const [extrasOpen, setExtrasOpen] = useState(false);
  const [modelsOpen, setModelsOpen] = useState(false);
  const newModels = entry.upstreamNew?.length ?? 0;
  const saveModels = async (models: string[], defaultModel: string) => {
    if (await run(`models:${entry.id}`, "save_gateway_provider_models", { id: entry.id, models, defaultModel }, t("保存模型列表"))) setModelsOpen(false);
  };
  const price = priceSummary(entry);
  const saveExtras = async (nextPrice: ProviderPrice | null, modelPrices: Record<string, ProviderPrice>, proxy: string) => {
    if (await run(`extras:${entry.id}`, "save_gateway_provider_extras", { id: entry.id, price: nextPrice, modelPrices, proxy }, t("保存价格与代理"))) setExtrasOpen(false);
  };
  const remove = async () => {
    const ok = await appConfirm(tf("从网关删除「{0}」？直连模式里的供应商不受影响。", [maskAccountName(entry.name)]), { title: t("删除供应商"), confirmText: t("删除"), danger: true });
    if (ok) await run(`remove:${entry.id}`, "delete_gateway_provider", { id: entry.id }, t("删除供应商"));
  };
  return (
    <div className="gw-pool-item" data-dragging={dragging} data-off={!enabled} data-pool-id={entry.id}>
      {/* 整行点击展开；把手、开关、按钮各管各的。 */}
      <div
        className="gw-pool-row"
        onClick={(event) => {
          if ((event.target as HTMLElement).closest("button, a, input, .gw-grip")) return;
          onToggleExpand();
        }}
      >
        <span
          aria-label={t("拖动排序")}
          className="gw-grip"
          onPointerDown={onGripDown}
          title={t("拖动排序：同一个模型有多家时按这个顺序尝试")}
        >
          <GripVertical className="h-4 w-4" />
        </span>
        <Avatar baseUrl={entry.baseUrl} name={entry.name} size="lg" />
        <div className="gw-pool-main">
          <strong>{maskAccountName(entry.name)}</strong>
          <small>{sub}</small>
        </div>
        <span className="gw-pool-status">
          {newModels ? (
            <button className="gw-new-models" onClick={() => setModelsOpen(true)} title={t("后台检查发现上游有新模型，点开查看")} type="button">
              {tf("+{0} 新模型", [newModels])}
            </button>
          ) : null}
          <i className="gw-dot" data-tone={tone} />
          {/* 订阅账号空闲时显示套餐，其它供应商显示 Key 数。 */}
          {!enabled ? t("已关闭") : status.state === "idle" && entry.account && entry.plan ? entry.plan : <MemberStatusLabel status={status} />}
        </span>
        <Switch
          disabled={busy !== null}
          label={tf("{0} 参与路由", [entry.name])}
          on={enabled}
          onToggle={() => void run(`toggle:${entry.id}`, "set_gateway_provider_enabled", { id: entry.id, enabled: !enabled }, t("切换供应商"), true)}
        />
        <button aria-expanded={expanded} aria-label={t("详情")} className="gw-chevron" data-open={expanded} onClick={onToggleExpand} type="button">
          <ChevronRight className="h-4 w-4" />
        </button>
      </div>
      {expanded ? (
        <div className="gw-agent-detail gw-pool-detail">
          <div className="gw-detail-row">
            <span className="gw-detail-label">{t("模型")}</span>
            <div className="gw-detail-value gw-chip-row">
              {entry.models.map((model) => <code className="gw-chip" key={model}>{`${entry.id}/${model}`}</code>)}
              {entry.models.length === 0 ? <span className="gw-muted">{t("未配置模型")}</span> : null}
              <button className="gw-text-button" disabled={busy !== null} onClick={() => setModelsOpen(true)} type="button">{t("编辑")}</button>
            </div>
          </div>
          <div className="gw-detail-row">
            <span className="gw-detail-label">{entry.account ? t("套餐") : "Key"}</span>
            <div className="gw-detail-value gw-chip-row">
              {entry.account ? <span>{entry.plan || t("未知")}</span> : <KeyCount count={entry.keyCount} />}
              {failures.map((key) => (
                <span className="gw-key-failure" key={key.keyId}>
                  {`${key.keyId}：${[t(failureLabel(key.lastFailure)) || t(key.state === "disabled" ? "已停用" : "冷却中"), formatRemaining(key.remainingMs)].filter(Boolean).join(" · ")}`}
                </span>
              ))}
              {failures.length || breaker ? (
                <button className="gw-text-button" disabled={busy !== null} onClick={() => void run(`reset:${entry.id}`, "reset_gateway_key_health", { relayId: entry.id }, t("重置 Key 状态"), true)} type="button">
                  {t("重置")}
                </button>
              ) : null}
            </div>
          </div>
          <div className="gw-detail-row">
            <span className="gw-detail-label">{t("价格")}</span>
            <div className="gw-detail-value gw-chip-row">
              <span className={price ? undefined : "gw-muted"}>{price || t("未填写，不估算费用")}</span>
              <button className="gw-text-button" disabled={busy !== null} onClick={() => setExtrasOpen(true)} type="button">{t("编辑")}</button>
            </div>
          </div>
          <div className="gw-detail-row">
            <span className="gw-detail-label">{t("代理")}</span>
            <div className="gw-detail-value gw-chip-row">
              {entry.proxy ? <code className="gw-chip">{entry.proxy}</code> : <span className="gw-muted">{t("沿用系统代理")}</span>}
              <button className="gw-text-button" disabled={busy !== null} onClick={() => setExtrasOpen(true)} type="button">{t("编辑")}</button>
            </div>
          </div>
          <div className="gw-detail-row gw-detail-foot">
            <span className="gw-detail-label" />
            <div className="gw-detail-value">
              {entry.account ? (
                <button className="gw-text-button" disabled={busy !== null} onClick={onRelogin} type="button">
                  {t("重新登录")}
                  <ChevronRight className="h-3.5 w-3.5" />
                </button>
              ) : (
                <button className="gw-text-button" onClick={onEdit} type="button">
                  {t("编辑")}
                  <ChevronRight className="h-3.5 w-3.5" />
                </button>
              )}
              <button className="gw-danger-link" disabled={busy !== null} onClick={() => void remove()} type="button">
                {t("删除")}
              </button>
            </div>
          </div>
        </div>
      ) : null}
      {modelsOpen ? (
        <ProviderModelsDialog busy={busy !== null} entry={entry} onCancel={() => setModelsOpen(false)} onSave={(models, defaultModel) => void saveModels(models, defaultModel)} />
      ) : null}
      {extrasOpen ? (
        <ProviderExtrasDialog
          busy={busy !== null}
          entry={entry}
          onCancel={() => setExtrasOpen(false)}
          onSave={(nextPrice, modelPrices, proxy) => void saveExtras(nextPrice, modelPrices, proxy)}
        />
      ) : null}
    </div>
  );
}

/** 添加时给哪些 Agent 用：一排可切换的 Agent，默认按各自的「添加供应商时自动加入」。 */
function AgentPicks({ payload, picked, onChange }: { payload: GatewayPayload | null; picked: GatewayAgentId[]; onChange: (next: GatewayAgentId[]) => void }) {
  const agents = payload?.agents ?? [];
  if (!agents.length) return null;
  return (
    <div className="gw-agent-picks">
      <span>{t("添加后给这些 Agent 用")}</span>
      {agents.map((agent) => {
        const on = picked.includes(agent.agent);
        return (
          <button
            aria-pressed={on}
            data-on={on}
            key={agent.agent}
            onClick={() => onChange(on ? picked.filter((item) => item !== agent.agent) : [...picked, agent.agent])}
            type="button"
          >
            <AgentIcon agent={agent.agent} className="gw-agent-pick-icon" />
            {agent.name}
            {on ? <Check className="h-3.5 w-3.5" /> : null}
          </button>
        );
      })}
      {picked.length === 0 ? <em>{t("都不选也能添加，之后在 Agent 的「挑选」里再打开")}</em> : null}
    </div>
  );
}

type CatalogItem = { key: string; name: string; added?: boolean; icon?: ReactNode; onClick: () => void };

/** 「添加供应商」目录：订阅、供应商（厂商 API）、中转、本机；「导入…」从直连模式复制。 */
function ProviderCatalog({
  payload,
  busy,
  run,
  notify,
  onClose,
  onPreset,
  onCustom,
  onLogin,
  agentPicks,
  skipAgents,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  notify: Notify;
  onClose: () => void;
  onPreset: (preset: GatewayPreset) => void;
  onCustom: () => void;
  onLogin: (kind: AccountKind) => void;
  agentPicks: ReactNode;
  skipAgents: GatewayAgentId[];
}) {
  const [query, setQuery] = useState("");
  const [importing, setImporting] = useState(false);
  const presets = useMemo(() => gatewayPresets(), []);
  const needle = query.trim().toLowerCase();
  const hit = (...values: string[]) => !needle || values.some((value) => value.toLowerCase().includes(needle));
  const presetItems = (section: PresetSection): CatalogItem[] => presets
    .filter((preset) => preset.section === section && hit(preset.name, preset.baseUrl))
    .map((preset) => ({ key: preset.key, name: preset.name, added: presetAdded(payload, preset), onClick: () => onPreset(preset) }));
  const loggedIn = (kind: AccountKind) => Boolean(payload?.pool.some((entry) => entry.account === kind));
  const subscriptions: CatalogItem[] = (
    [
      { kind: "chatgpt", name: "ChatGPT", icon: "openai" },
      { kind: "chatgpt-api", name: "ChatGPT API", icon: "openai" },
      { kind: "copilot", name: "GitHub Copilot", icon: "githubcopilot" },
      { kind: "grok", name: "SuperGrok", icon: "grok" },
    ] as Array<{ kind: AccountKind; name: string; icon: string }>
  )
    .filter((item) => hit(item.name, t("订阅")))
    .map((item) => ({
      key: `login:${item.kind}`,
      name: item.name,
      added: loggedIn(item.kind),
      onClick: () => onLogin(item.kind),
    }));
  const local: CatalogItem[] = [
    ...presetItems("local"),
    ...(hit(t("自定义")) ? [{ key: "custom", name: t("自定义"), icon: <span className="gw-avatar gw-avatar-plus" data-size="sm"><Plus className="h-3.5 w-3.5" /></span>, onClick: onCustom }] : []),
  ];
  const sections: Array<{ key: string; title: string; hint: string; items: CatalogItem[] }> = [
    { key: "sponsor", title: t("赞助商"), hint: t("支持 Codex++ 的服务商"), items: presetItems("sponsor") },
    { key: "subscription", title: t("订阅"), hint: t("登录即可，无需密钥；与 Codex 自己的登录互不影响"), items: subscriptions },
    { key: "vendor", title: t("供应商"), hint: t("模型厂商的 API"), items: presetItems("vendor") },
    { key: "relay", title: t("中转"), hint: t("一个密钥用多家模型"), items: presetItems("relay") },
    { key: "local", title: t("本机"), hint: t("本机运行的模型服务，或任意兼容地址"), items: local },
  ].filter((section) => section.items.length);

  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape" && !importing) onClose();
    };
    window.addEventListener("keydown", escape);
    return () => window.removeEventListener("keydown", escape);
  }, [importing, onClose]);

  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="gw-modal gw-catalog-modal" role="dialog" aria-label={t("添加供应商")}>
        <header>
          <strong>{t("添加供应商")}</strong>
          <label className="gw-sheet-search">
            <Search className="h-4 w-4" />
            <input autoFocus onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("查找供应商…")} value={query} />
          </label>
          <button className="gw-text-button" onClick={() => setImporting(true)} type="button">{t("导入…")}</button>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onClose} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body gw-catalog">
          {sections.map((section) => (
            <section className="gw-catalog-section" key={section.key}>
              <h4>{section.title} <span>{section.hint}</span></h4>
              <div className="gw-catalog-grid">
                {section.items.map((item) => (
                  <button className="gw-catalog-item" disabled={busy !== null} key={item.key} onClick={item.onClick} type="button">
                    {item.icon ?? <Avatar name={item.name} size="sm" />}
                    <span>{item.name}</span>
                    {item.added ? <i className="gw-dot" data-tone="added" title={t("已在供应商池里")} /> : null}
                  </button>
                ))}
              </div>
            </section>
          ))}
          {sections.length === 0 ? <div className="gw-empty">{t("没有匹配的供应商")}</div> : null}
        </div>
        <footer className="gw-catalog-foot">{agentPicks}</footer>
      </div>
      {importing ? <ImportDialog busy={busy} onClose={() => setImporting(false)} payload={payload} run={run} skipAgents={skipAgents} /> : null}
    </div>,
    document.body,
  );
}

/** 导入：把直连模式里 Codex、Claude Code、Grok 的供应商复制一份进网关（之后两边互不影响），或用 OpenCode 里保存的 Key。 */
function ImportDialog({ payload, busy, run, onClose, skipAgents }: { payload: GatewayPayload | null; busy: string | null; run: Run; onClose: () => void; skipAgents: GatewayAgentId[] }) {
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onClose]);
  const imported = (source: string, id: string) => Boolean(payload?.pool.some((entry) => entry.source === `${source}:${id}`));
  const finish = async (source: string, id: string) => {
    const done = source === "opencode"
      ? await run(`import:${source}:${id}`, "import_gateway_opencode_key", { id, skipAgents }, t("导入供应商"))
      : await run(`import:${source}:${id}`, "import_gateway_provider", { source, id, skipAgents }, t("导入供应商"));
    if (done) onClose();
  };
  const groups: Array<{ key: string; title: string; items: Array<{ id: string; name: string; sub: string }> }> = [
    {
      key: "codex",
      title: t("Codex 的供应商"),
      items: (payload?.codexProviders ?? []).map((entry) => ({ id: entry.id, name: entry.name, sub: `${protocolLabel(entry.protocol)} · ${presetHost(entry.baseUrl)}` })),
    },
    {
      key: "claude",
      title: t("Claude Code 的供应商"),
      items: (payload?.claudeProviders ?? []).map((item) => ({ id: item.id, name: item.name, sub: t("Anthropic 协议，原样转发") })),
    },
    {
      key: "grok",
      title: t("Grok 的供应商"),
      items: (payload?.grokProfiles ?? []).map((item) => ({ id: item.id, name: item.name, sub: t("Grok 页里配置的供应商") })),
    },
    {
      key: "opencode",
      title: t("OpenCode 里保存的 Key"),
      items: (payload?.opencodeKeys ?? []).map((item) => ({ id: item.id, name: item.name, sub: t("opencode auth login 保存的 Key，按模型自动选协议") })),
    },
  ];
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="gw-modal gw-import-modal">
        <header>
          <strong>{t("导入供应商")}</strong>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onClose} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body">
          <p className="gw-muted gw-import-note">{t("复制一份进网关，之后两边各改各的，互不影响。官方登录账号不能复制，需要在网关里单独登录。")}</p>
          {groups.map((group) => (
            <section key={group.key}>
              <h4 className="gw-import-title">{group.title}</h4>
              {group.items.length ? group.items.map((item) => {
                const added = imported(group.key, item.id);
                return (
                  <button className="gw-import-item" disabled={busy !== null} key={item.id} onClick={() => void finish(group.key, item.id)} type="button">
                    <Avatar name={item.name} size="sm" />
                    <span>
                      <strong>{item.name}</strong>
                      <small>{added ? t("已导入过，再点会再复制一份") : item.sub}</small>
                    </span>
                    <Plus className="h-4 w-4" />
                  </button>
                );
              }) : <p className="gw-muted gw-import-empty">{t("这里没有可导入的供应商")}</p>}
            </section>
          ))}
        </div>
      </div>
    </div>,
    document.body,
  );
}

/** 新建 / 编辑供应商：名称、Key（每行一个，多个自动轮换）、地址、协议、模型。 */
/** Copilot 设备码登录：显示验证码，等你在 GitHub 页面里输入并授权。 */
function DeviceCodeDialog({ code, notify, onClose }: { code: DeviceCode; notify: Notify; onClose: () => void }) {
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(code.userCode);
      await notify(t("已复制"), code.userCode, "ok");
    } catch (error) {
      await notify(t("复制失败"), String(error), "failed");
    }
  };
  return createPortal(
    <div className="gw-modal-backdrop gw-portal">
      <div className="gw-modal gw-device-modal">
        <header>
          <strong>{t("登录 GitHub Copilot")}</strong>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onClose} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body">
          <p className="gw-muted">{tf("已在浏览器打开 {0}，输入下面的验证码并授权。授权完成后这里会自动继续。", [code.verificationUri])}</p>
          <button className="gw-device-code" onClick={() => void copy()} title={t("复制验证码")} type="button">
            <span>{code.userCode}</span>
            <Copy className="h-4 w-4" />
          </button>
          <p className="gw-muted">{tf("验证码 {0} 分钟内有效。需要这个 GitHub 账号有 Copilot 订阅。", [Math.round(code.expiresIn / 60)])}</p>
        </div>
      </div>
    </div>,
    document.body,
  );
}
