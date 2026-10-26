/** 本地网关 · 供应商：已加入网关的供应商（可开关、排序、展开详情），下面是「添加供应商」目录。 */
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { confirm } from "@tauri-apps/plugin-dialog";
import { ChevronRight, GripVertical, Plus, Search, X } from "lucide-react";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Textarea } from "@/components/ui/textarea";
import { t, tf } from "./i18n";
import {
  failureLabel,
  formatRemaining,
  gatewayPresets,
  memberStatus,
  poolProviderEnabled,
  presetAdded,
  presetHost,
  protocolLabel,
  providerDraftFromPreset,
  validateProviderDraft,
  type GatewayPayload,
  type GatewayPoolEntry,
  type GatewayPreset,
  type GatewayProtocol,
  type PresetSection,
  type ProviderDraft,
} from "./gateway-model";
import { AGENT_ICONS, Avatar, Switch, type Run } from "./gateway-ui";

export function ProvidersView({
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
  const pool = payload?.pool ?? [];
  const [query, setQuery] = useState("");
  const [expanded, setExpanded] = useState<string | null>(null);
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [dragging, setDragging] = useState<string | null>(null);
  const showCatalog = catalogOpen || (payload !== null && pool.length === 0);
  const needle = query.trim().toLowerCase();
  const visible = pool.filter((entry) => !needle || entry.name.toLowerCase().includes(needle) || entry.models.some((model) => model.toLowerCase().includes(needle)));

  const drop = (targetId: string) => {
    if (!dragging || dragging === targetId) return;
    const ids = pool.map((entry) => entry.id).filter((id) => id !== dragging);
    ids.splice(ids.indexOf(targetId), 0, dragging);
    setDragging(null);
    void run("reorder", "reorder_gateway_pool", { ids }, t("调整顺序"), true);
  };

  return (
    <>
      <div className="gw-provider-search-row">
        <label className="gw-sheet-search">
          <Search className="h-4 w-4" />
          <input onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("查找供应商")} value={query} />
        </label>
      </div>
      {pool.length ? (
        <div className="gw-card gw-pool-card">
          {visible.map((entry) => (
            <PoolRow
              busy={busy}
              dragging={dragging === entry.id}
              entry={entry}
              expanded={expanded === entry.id}
              key={entry.id}
              onDragEnd={() => setDragging(null)}
              onDragStart={() => setDragging(entry.id)}
              onDrop={() => drop(entry.id)}
              onOpenCodexProviders={onOpenCodexProviders}
              onToggleExpand={() => setExpanded(expanded === entry.id ? null : entry.id)}
              payload={payload}
              run={run}
            />
          ))}
          {visible.length === 0 ? <div className="gw-empty">{t("没有匹配的供应商")}</div> : null}
        </div>
      ) : null}
      {!showCatalog ? (
        <button className="gw-add-row" onClick={() => setCatalogOpen(true)} type="button">
          <Plus className="h-4 w-4" />
          {t("添加供应商")}
        </button>
      ) : null}
      {showCatalog ? (
        <ProviderCatalog
          busy={busy}
          closable={pool.length > 0}
          onClose={() => setCatalogOpen(false)}
          onOpenCodexProviders={onOpenCodexProviders}
          payload={payload}
          run={run}
        />
      ) : null}
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
  onDragStart,
  onDragEnd,
  onDrop,
  onOpenCodexProviders,
}: {
  entry: GatewayPoolEntry;
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  expanded: boolean;
  dragging: boolean;
  onToggleExpand: () => void;
  onDragStart: () => void;
  onDragEnd: () => void;
  onDrop: () => void;
  onOpenCodexProviders: () => void;
}) {
  const enabled = poolProviderEnabled(payload, entry.id);
  const status = memberStatus(payload, entry.id);
  const failures = (payload?.health.keys ?? []).filter((key) => key.relayId === entry.id && key.state !== "ok");
  const breaker = payload?.health.providers.some((item) => item.relayId === entry.id && item.breakerOpen);
  const bound = Object.values(payload?.gateway.bindings ?? {}).some((binding) => binding?.target.kind === "provider" && binding.target.id === entry.id);
  const statusText = !enabled ? t("已关闭") : status.args.length ? tf(status.text, status.args) : t(status.text);
  const tone = !enabled ? "off" : status.state === "cooling" ? "error" : "on";
  const sub = [entry.official ? t("官方登录") : protocolLabel(entry.protocol), entry.official ? "" : presetHost(entry.baseUrl), tf("{0} 个模型", [entry.models.length])].filter(Boolean).join(" · ");
  const remove = async () => {
    const ok = await confirm(tf("把「{0}」移出供应商池？Codex 供应商列表里的这一项会保留。", [entry.name]), { title: t("移出供应商池"), kind: "warning" });
    if (ok) await run(`remove:${entry.id}`, "remove_gateway_pool_provider", { id: entry.id }, t("移出供应商池"));
  };
  return (
    <div
      className="gw-pool-item"
      data-dragging={dragging}
      data-off={!enabled}
      onDragOver={(event) => event.preventDefault()}
      onDrop={(event) => { event.preventDefault(); onDrop(); }}
    >
      <div className="gw-pool-row">
        <span
          aria-label={t("拖动排序")}
          className="gw-grip"
          draggable
          onDragEnd={onDragEnd}
          onDragStart={(event) => { event.dataTransfer.effectAllowed = "move"; onDragStart(); }}
          title={t("拖动排序：同一个模型有多家时按这个顺序尝试")}
        >
          <GripVertical className="h-4 w-4" />
        </span>
        <Avatar name={entry.official ? "ChatGPT" : entry.name} size="lg" />
        <div className="gw-pool-main">
          <strong>{entry.name}</strong>
          <small>{sub}</small>
        </div>
        <span className="gw-pool-status"><i className="gw-dot" data-tone={tone} />{statusText}</span>
        <Switch
          disabled={busy !== null}
          label={tf("{0} 参与路由", [entry.name])}
          on={enabled}
          onToggle={() => void run(`toggle:${entry.id}`, "set_gateway_pool_provider_enabled", { id: entry.id, enabled: !enabled }, t("切换供应商"), true)}
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
              {entry.models.map((model) => <code className="gw-chip" key={model}>{model}</code>)}
              {entry.models.length === 0 ? <span className="gw-muted">{t("未配置模型")}</span> : null}
            </div>
          </div>
          <div className="gw-detail-row">
            <span className="gw-detail-label">Key</span>
            <div className="gw-detail-value gw-chip-row">
              <span>{entry.keyCount > 1 ? tf("{0} 个 Key 轮换", [entry.keyCount]) : t("一个 Key")}</span>
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
          <div className="gw-detail-row gw-detail-foot">
            <span className="gw-detail-label gw-detail-file">{t("数据存在 Codex 供应商列表里")}</span>
            <div className="gw-detail-value">
              <button className="gw-text-button" onClick={onOpenCodexProviders} type="button">
                {t("在 Codex 里编辑")}
                <ChevronRight className="h-3.5 w-3.5" />
              </button>
              <button className="gw-danger-link" disabled={busy !== null || bound} onClick={() => void remove()} title={bound ? t("正有 Agent 绑定在这个供应商上") : undefined} type="button">
                {t("移出供应商池")}
              </button>
            </div>
          </div>
        </div>
      ) : null}
    </div>
  );
}

type CatalogItem = { key: string; name: string; added?: boolean; icon?: ReactNode; onClick: () => void };

/** 「添加供应商」目录：订阅、厂商、中转、本机；「导入…」从 Codex / Claude Code / Grok 已有的供应商加入。 */
function ProviderCatalog({
  payload,
  busy,
  run,
  closable,
  onClose,
  onOpenCodexProviders,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  closable: boolean;
  onClose: () => void;
  onOpenCodexProviders: () => void;
}) {
  const [query, setQuery] = useState("");
  const [form, setForm] = useState<{ preset: GatewayPreset | null } | null>(null);
  const [importing, setImporting] = useState(false);
  const presets = useMemo(() => gatewayPresets(), []);
  const needle = query.trim().toLowerCase();
  const hit = (...values: string[]) => !needle || values.some((value) => value.toLowerCase().includes(needle));
  const presetItems = (section: PresetSection): CatalogItem[] => presets
    .filter((preset) => preset.section === section && hit(preset.name, preset.baseUrl))
    .map((preset) => ({ key: preset.key, name: preset.name, added: presetAdded(payload, preset), onClick: () => setForm({ preset }) }));
  const officialOutside = (payload?.codexProviders ?? []).filter((entry) => entry.official);
  const officialInPool = (payload?.pool ?? []).some((entry) => entry.official);
  const subscriptions: CatalogItem[] = hit("ChatGPT", t("订阅"))
    ? [{
        key: "chatgpt",
        name: "ChatGPT",
        added: officialInPool,
        icon: <img alt="" className="gw-catalog-logo" src={AGENT_ICONS.codex} />,
        onClick: () => {
          if (officialOutside.length) void run("add:official", "add_gateway_pool_providers", { ids: officialOutside.map((entry) => entry.id) }, t("加入供应商池"));
          else onOpenCodexProviders();
        },
      }]
    : [];
  const local: CatalogItem[] = [
    ...presetItems("local"),
    ...(hit(t("自定义")) ? [{ key: "custom", name: t("自定义"), icon: <span className="gw-avatar gw-avatar-plus" data-size="sm"><Plus className="h-3.5 w-3.5" /></span>, onClick: () => setForm({ preset: null }) }] : []),
  ];
  const sections: Array<{ key: string; title: string; hint: string; items: CatalogItem[] }> = [
    { key: "subscription", title: t("订阅"), hint: officialOutside.length ? t("登录即可，无需密钥；点一下加入 Codex 里已登录的账号") : t("登录即可，无需密钥；先在 Codex 里登录官方账号"), items: subscriptions },
    { key: "vendor", title: t("供应商"), hint: t("模型厂商的 API"), items: presetItems("vendor") },
    { key: "relay", title: t("中转"), hint: t("一个密钥用多家模型"), items: presetItems("relay") },
    { key: "local", title: t("本机"), hint: t("本机运行的模型服务，或任意兼容地址"), items: local },
  ].filter((section) => section.items.length);

  return (
    <div className="gw-catalog">
      <div className="gw-sheet-head">
        <span className="gw-section-title">{t("添加供应商")}</span>
        <label className="gw-sheet-search">
          <Search className="h-4 w-4" />
          <input onChange={(event) => setQuery(event.currentTarget.value)} placeholder={t("查找供应商…")} value={query} />
        </label>
        <button className="gw-text-button" onClick={() => setImporting(true)} type="button">{t("导入…")}</button>
        {closable ? <button className="gw-text-button" onClick={onClose} type="button">{t("关闭")}</button> : null}
      </div>
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
      {form ? (
        <ProviderFormDialog
          busy={busy}
          onCancel={() => setForm(null)}
          onSubmit={async (draft) => {
            if (await run("add:create", "create_gateway_pool_provider", { ...draft }, t("添加供应商"))) {
              setForm(null);
              if (closable) onClose();
            }
          }}
          preset={form.preset}
        />
      ) : null}
      {importing ? <ImportDialog busy={busy} onClose={() => setImporting(false)} payload={payload} run={run} /> : null}
    </div>
  );
}

/** 导入：从 Codex 已有的供应商、Claude Code、Grok 的供应商加入池。 */
function ImportDialog({ payload, busy, run, onClose }: { payload: GatewayPayload | null; busy: string | null; run: Run; onClose: () => void }) {
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onClose]);
  const inPool = (name: string) => Boolean(payload?.pool.some((entry) => entry.name === name));
  const finish = async (command: string, args: Record<string, unknown>, title: string) => {
    if (await run(`import:${command}`, command, args, title)) onClose();
  };
  const groups: Array<{ key: string; title: string; items: Array<{ id: string; name: string; sub: string; added: boolean; onClick: () => void }> }> = [
    {
      key: "codex",
      title: t("Codex 里已有的"),
      items: (payload?.codexProviders ?? []).map((entry) => ({
        id: entry.id,
        name: entry.name,
        sub: entry.official ? t("官方登录") : `${protocolLabel(entry.protocol)} · ${presetHost(entry.baseUrl)}`,
        added: false,
        onClick: () => void finish("add_gateway_pool_providers", { ids: [entry.id] }, t("加入供应商池")),
      })),
    },
    {
      key: "claude",
      title: t("Claude Code 的供应商"),
      items: (payload?.claudeProviders ?? []).map((item) => ({
        id: item.id,
        name: item.name,
        sub: t("Anthropic 协议，原样转发"),
        added: inPool(item.name),
        onClick: () => void finish("import_claude_provider_to_gateway_pool", { id: item.id }, t("导入供应商池")),
      })),
    },
    {
      key: "grok",
      title: t("Grok 的供应商"),
      items: (payload?.grokProfiles ?? []).map((item) => ({
        id: item.id,
        name: item.name,
        sub: t("Grok 页里配置的供应商"),
        added: inPool(item.name),
        onClick: () => void finish("import_grok_profile_to_gateway_pool", { id: item.id }, t("导入供应商池")),
      })),
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
          {groups.map((group) => (
            <section key={group.key}>
              <h4 className="gw-import-title">{group.title}</h4>
              {group.items.length ? group.items.map((item) => (
                <button className="gw-import-item" disabled={busy !== null || item.added} key={item.id} onClick={item.onClick} type="button">
                  <Avatar name={item.name} size="sm" />
                  <span>
                    <strong>{item.name}</strong>
                    <small>{item.added ? t("已在供应商池里") : item.sub}</small>
                  </span>
                  {!item.added ? <Plus className="h-4 w-4" /> : null}
                </button>
              )) : <p className="gw-muted gw-import-empty">{t("这里没有可导入的供应商")}</p>}
            </section>
          ))}
        </div>
      </div>
    </div>,
    document.body,
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
