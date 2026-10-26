/** 本地网关 · 供应商：网关自己的供应商池（可开关、排序、编辑、删除），下面是「添加供应商」目录。
 *
 * 与直连模式完全独立：这里的增删改只动 gateway.json；「导入…」是把直连模式的供应商复制一份过来。
 */
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
  presetAdded,
  presetHost,
  protocolLabel,
  providerDraftFromEntry,
  providerDraftFromPreset,
  providerEnabled,
  providerInput,
  validateProviderDraft,
  type GatewayPayload,
  type GatewayPoolEntry,
  type GatewayPreset,
  type GatewayProtocol,
  type PresetSection,
  type ProviderDraft,
} from "./gateway-model";
import { AGENT_ICONS, Avatar, Switch, type Notify, type Run } from "./gateway-ui";

/** 表单弹窗的用途：按预设新建、自定义新建、编辑已有供应商。 */
type FormState = { mode: "preset"; preset: GatewayPreset } | { mode: "custom" } | { mode: "edit"; entry: GatewayPoolEntry };

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
  const [catalogOpen, setCatalogOpen] = useState(false);
  const [dragging, setDragging] = useState<string | null>(null);
  const [form, setForm] = useState<FormState | null>(null);
  const showCatalog = catalogOpen || (payload !== null && pool.length === 0);
  const needle = query.trim().toLowerCase();
  const visible = pool.filter((entry) => !needle || entry.name.toLowerCase().includes(needle) || entry.models.some((model) => model.toLowerCase().includes(needle)));

  const drop = (targetId: string) => {
    if (!dragging || dragging === targetId) return;
    const ids = pool.map((entry) => entry.id).filter((id) => id !== dragging);
    ids.splice(ids.indexOf(targetId), 0, dragging);
    setDragging(null);
    void run("reorder", "reorder_gateway_providers", { ids }, t("调整顺序"), true);
  };

  const submit = async (draft: ProviderDraft) => {
    if (!form) return;
    const input = providerInput(draft);
    const ok = form.mode === "edit"
      ? await run(`edit:${form.entry.id}`, "update_gateway_provider", { id: form.entry.id, input }, t("保存供应商"))
      : await run("add:create", "create_gateway_provider", { input, source: form.mode === "preset" ? `preset:${form.preset.key}` : "custom" }, t("添加供应商"));
    if (ok) {
      setForm(null);
      if (form.mode !== "edit" && pool.length) setCatalogOpen(false);
    }
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
              onEdit={() => setForm({ mode: "edit", entry })}
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
          notify={notify}
          onClose={() => setCatalogOpen(false)}
          onCustom={() => setForm({ mode: "custom" })}
          onPreset={(preset) => setForm({ mode: "preset", preset })}
          payload={payload}
          run={run}
        />
      ) : null}
      {form ? <ProviderFormDialog busy={busy} form={form} onCancel={() => setForm(null)} onSubmit={(draft) => void submit(draft)} /> : null}
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
  onEdit,
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
  onEdit: () => void;
}) {
  const enabled = providerEnabled(entry);
  const status = memberStatus(payload, entry.id);
  const failures = (payload?.health.keys ?? []).filter((key) => key.relayId === entry.id && key.state !== "ok");
  const breaker = payload?.health.providers.some((item) => item.relayId === entry.id && item.breakerOpen);
  const statusText = !enabled ? t("已关闭") : status.args.length ? tf(status.text, status.args) : t(status.text);
  const tone = !enabled ? "off" : status.state === "cooling" ? "error" : "on";
  const sub = (entry.official
    ? [t("订阅登录"), tf("{0} 个模型", [entry.models.length])]
    : [protocolLabel(entry.protocol), presetHost(entry.baseUrl), tf("{0} 个模型", [entry.models.length])]
  ).filter(Boolean).join(" · ");
  const remove = async () => {
    const ok = await confirm(tf("从网关删除「{0}」？直连模式里的供应商不受影响。", [entry.name]), { title: t("删除供应商"), kind: "warning" });
    if (ok) await run(`remove:${entry.id}`, "delete_gateway_provider", { id: entry.id }, t("删除供应商"));
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
        <Avatar baseUrl={entry.baseUrl} name={entry.name} size="lg" />
        <div className="gw-pool-main">
          <strong>{entry.name}</strong>
          <small>{sub}</small>
        </div>
        <span className="gw-pool-status"><i className="gw-dot" data-tone={tone} />{statusText}</span>
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
            <span className="gw-detail-label gw-detail-file">{t("只属于网关，直连模式里看不到")}</span>
            <div className="gw-detail-value">
              {entry.official ? (
                <button className="gw-text-button" disabled={busy !== null} onClick={() => void run("login:chatgpt", "login_gateway_chatgpt", undefined, t("登录 ChatGPT"))} type="button">
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
  closable,
  onClose,
  onPreset,
  onCustom,
}: {
  payload: GatewayPayload | null;
  busy: string | null;
  run: Run;
  notify: Notify;
  closable: boolean;
  onClose: () => void;
  onPreset: (preset: GatewayPreset) => void;
  onCustom: () => void;
}) {
  const [query, setQuery] = useState("");
  const [importing, setImporting] = useState(false);
  const presets = useMemo(() => gatewayPresets(), []);
  const needle = query.trim().toLowerCase();
  const hit = (...values: string[]) => !needle || values.some((value) => value.toLowerCase().includes(needle));
  const presetItems = (section: PresetSection): CatalogItem[] => presets
    .filter((preset) => preset.section === section && hit(preset.name, preset.baseUrl))
    .map((preset) => ({ key: preset.key, name: preset.name, added: presetAdded(payload, preset), onClick: () => onPreset(preset) }));
  const subscriptions: CatalogItem[] = hit("ChatGPT", t("订阅"))
    ? [{
        key: "chatgpt",
        name: "ChatGPT",
        icon: <img alt="" className="gw-catalog-logo" src={AGENT_ICONS.codex} />,
        added: Boolean(payload?.pool.some((entry) => entry.official)),
        onClick: () => void (async () => {
          void notify(t("登录 ChatGPT"), t("已在浏览器打开登录页，登录完成后回到这里。5 分钟内有效。"), "info");
          await run("login:chatgpt", "login_gateway_chatgpt", undefined, t("登录 ChatGPT"));
        })(),
      }]
    : [];
  const local: CatalogItem[] = [
    ...presetItems("local"),
    ...(hit(t("自定义")) ? [{ key: "custom", name: t("自定义"), icon: <span className="gw-avatar gw-avatar-plus" data-size="sm"><Plus className="h-3.5 w-3.5" /></span>, onClick: onCustom }] : []),
  ];
  const sections: Array<{ key: string; title: string; hint: string; items: CatalogItem[] }> = [
    { key: "subscription", title: t("订阅"), hint: t("登录即可，无需密钥；与 Codex 自己的登录互不影响"), items: subscriptions },
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
      {importing ? <ImportDialog busy={busy} onClose={() => setImporting(false)} payload={payload} run={run} /> : null}
    </div>
  );
}

/** 导入：把直连模式里 Codex、Claude Code、Grok 的供应商复制一份进网关，之后两边互不影响。 */
function ImportDialog({ payload, busy, run, onClose }: { payload: GatewayPayload | null; busy: string | null; run: Run; onClose: () => void }) {
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onClose]);
  const imported = (source: string, id: string) => Boolean(payload?.pool.some((entry) => entry.source === `${source}:${id}`));
  const finish = async (source: string, id: string) => {
    if (await run(`import:${source}:${id}`, "import_gateway_provider", { source, id }, t("导入供应商"))) onClose();
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
  ];
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onClose(); }}>
      <div className="gw-modal gw-import-modal">
        <header>
          <strong>{t("从直连模式导入")}</strong>
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
function ProviderFormDialog({
  form,
  busy,
  onSubmit,
  onCancel,
}: {
  form: FormState;
  busy: string | null;
  onSubmit: (draft: ProviderDraft) => void;
  onCancel: () => void;
}) {
  const editing = form.mode === "edit";
  const preset = form.mode === "preset" ? form.preset : null;
  const [draft, setDraft] = useState<ProviderDraft>(() => (form.mode === "edit" ? providerDraftFromEntry(form.entry) : providerDraftFromPreset(preset)));
  const update = (patch: Partial<ProviderDraft>) => setDraft((previous) => ({ ...previous, ...patch }));
  const validation = validateProviderDraft(draft, editing);
  useEffect(() => {
    const escape = (event: KeyboardEvent) => {
      if (event.key === "Escape") onCancel();
    };
    document.addEventListener("keydown", escape);
    return () => document.removeEventListener("keydown", escape);
  }, [onCancel]);
  const title = form.mode === "edit" ? tf("编辑 {0}", [form.entry.name]) : preset ? tf("添加 {0}", [preset.name]) : t("添加自定义供应商");
  const keyHint = editing
    ? tf("留空表示不改（现在有 {0} 个）", [form.mode === "edit" ? form.entry.keyCount : 0])
    : preset?.apiKeyUrl
      ? tf("在 {0} 获取；每行一个，多个自动轮换", [presetHost(preset.apiKeyUrl)])
      : preset?.defaultKey
        ? t("本机服务不校验 Key，保留默认值即可")
        : t("每行一个，多个自动轮换");
  return createPortal(
    <div className="gw-modal-backdrop gw-portal" onMouseDown={(event) => { if (event.target === event.currentTarget) onCancel(); }}>
      <form
        className="gw-modal"
        onSubmit={(event) => {
          event.preventDefault();
          if (!validation) onSubmit(draft);
        }}
      >
        <header>
          {preset || editing ? <Avatar name={preset?.name ?? draft.name} /> : <span className="gw-avatar gw-avatar-plus"><Plus className="h-4 w-4" /></span>}
          <strong>{title}</strong>
          <button aria-label={t("关闭")} className="gw-icon-button" onClick={onCancel} type="button"><X className="h-4 w-4" /></button>
        </header>
        <div className="gw-modal-body">
          <label className="gw-field">
            <span>{t("名称")}</span>
            <Input onChange={(event) => update({ name: event.currentTarget.value })} value={draft.name} />
          </label>
          <label className="gw-field">
            <span>{t("API Key")} <small>{keyHint}</small></span>
            <Textarea
              autoComplete="off"
              autoFocus={Boolean(preset && !preset.defaultKey)}
              className="gw-secret"
              onChange={(event) => update({ keys: event.currentTarget.value })}
              placeholder="sk-..."
              rows={2}
              spellCheck={false}
              value={draft.keys}
            />
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
          <div className="gw-field">
            <span>{t("价格")} <small>{t("每百万 token；填了才在「用量」里估算费用，可以不填")}</small></span>
            <div className="gw-price-row">
              <select aria-label={t("币种")} className="gw-select" onChange={(event) => update({ priceCurrency: event.currentTarget.value })} value={draft.priceCurrency}>
                <option value="CNY">¥ CNY</option>
                <option value="USD">$ USD</option>
              </select>
              <label><span>{t("输入")}</span><Input inputMode="decimal" onChange={(event) => update({ priceInput: event.currentTarget.value })} placeholder="0" value={draft.priceInput} /></label>
              <label><span>{t("缓存命中")}</span><Input inputMode="decimal" onChange={(event) => update({ priceCached: event.currentTarget.value })} placeholder="0" value={draft.priceCached} /></label>
              <label><span>{t("输出")}</span><Input inputMode="decimal" onChange={(event) => update({ priceOutput: event.currentTarget.value })} placeholder="0" value={draft.priceOutput} /></label>
            </div>
          </div>
        </div>
        <footer>
          <span className="gw-error">{validation ? t(validation) : ""}</span>
          <Button onClick={onCancel} size="sm" type="button" variant="ghost">{t("取消")}</Button>
          <Button disabled={busy !== null || Boolean(validation)} size="sm" type="submit">{busy ? t("保存中") : editing ? t("保存") : t("添加到供应商池")}</Button>
        </footer>
      </form>
    </div>,
    document.body,
  );
}
