/** Computer Use：复用 Codex app 的 Computer Use 与 Chrome 控制，通过 MCP 提供给 Claude Code、DeepSeek Harness、ZCode 等客户端。 */
import { useCallback, useEffect, useRef, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import { confirm, open } from "@tauri-apps/plugin-dialog";
import { AlertTriangle, AppWindow, CheckCircle2, FolderOpen, Globe, Loader2, MonitorSmartphone, MousePointerClick, Play, Plug, Plus, RefreshCw, ShieldCheck, Trash2, Unplug } from "lucide-react";
import { ComputerUsePreview } from "./ComputerUsePreview";
import { PageHeader } from "./ManagementPages";
import { t, tf } from "./i18n";
import "./ComputerUsePage.css";

type CuTargetId = "claudeCode" | "claudeDesktop" | "codex" | "dsh" | "zcode" | "workBuddy";
type CuTargetState = "registered" | "missing" | "needs_update" | "foreign" | "unsupported";
type CuTarget = { id: string; label: string; configPath: string; registered: boolean; state: CuTargetState; message: string };
type CuTargets = Record<CuTargetId, CuTarget>;
/** 自定义目标：同一种客户端换一个配置目录，id 形如 `custom-…`。 */
type CuCustomTarget = CuTarget & { kind: CuTargetId; path: string };
type CuStatus = {
  status: string;
  message: string;
  runtimeFound: boolean;
  claudeDesktopRunning?: boolean;
  appPath?: string;
  packageVersion?: string;
  servicePath?: string;
  configHint?: string;
  targets?: Partial<CuTargets>;
  customTargets?: CuCustomTarget[];
};
type CuMutation = { status: string; message: string; changed: boolean; backupPath?: string; targets?: Partial<CuTargets>; customTargets?: CuCustomTarget[] };
type CuFailureKind = "runtime_missing" | "untrusted_parent" | "permission_required" | "client_unsupported" | "service_unavailable" | "error";
type CuCheck = {
  status: string;
  message: string;
  ok: boolean;
  target: string;
  appCount?: number;
  browserCount?: number;
  sampleApps?: string[];
  durationMs?: number;
  failureKind?: CuFailureKind;
  failureDetail?: string;
};
type CuResult = { status: string; message: string };
type CuTab = "overview" | "preview" | "diagnose";
type CuApprovalMode = "auto" | "claude";
type CuApprovedApp = { decision: string; mode?: string; displayName?: string; decidedAt?: number };
type CuLastClient = { name?: string; version?: string; elicitation?: boolean; at?: number };
type CuApprovalState = {
  status?: string;
  message?: string;
  mode?: CuApprovalMode;
  approvals?: Array<[string, CuApprovedApp]>;
  lastClient?: CuLastClient | null;
  turnHooks?: { installed?: boolean; events?: string[]; settings?: string } | null;
};

// Codex 桌面版自己会把 [mcp_servers.computer-use] 写进 ~/.codex/config.toml，
// 再挂一份只会让同一个 CU 服务出现两条连接，所以不列 Codex。
const targetIds: CuTargetId[] = ["claudeCode", "claudeDesktop", "dsh", "zcode", "workBuddy"];
const targetFallback: Record<CuTargetId, { label: string; configPath: string }> = {
  claudeCode: { label: "Claude Code", configPath: "~/.claude.json" },
  claudeDesktop: { label: "Claude Desktop", configPath: "claude_desktop_config.json" },
  codex: { label: "Codex", configPath: "config.toml" },
  dsh: { label: "DeepSeek Harness", configPath: "~/.dsh/profiles/desktop/cordis.patch.yml" },
  zcode: { label: "ZCode", configPath: "~/.zcode/cli/config.json" },
  workBuddy: { label: "WorkBuddy AI", configPath: "~/.workbuddy-ai/mcp.json" },
};
const errorText = (error: unknown) => (error instanceof Error ? error.message : String(error));
const hasValue = (value?: string) => Boolean(value && value.trim());
// 配置路径通常很长，保留首尾两段比整体省略更能看清是哪个客户端。
const elidePath = (value: string, limit = 52) => {
  if (value.length <= limit) return value;
  const head = Math.ceil(limit * 0.45);
  const tail = limit - head - 1;
  return `${value.slice(0, head)}…${value.slice(-tail)}`;
};
// 自定义目标可选的类型，与上面列出的内置目标一致。
const customKinds: CuTargetId[] = targetIds;
// 输入框占位说明：告诉用户该填哪一层目录。渲染时再过 t()。
const customDirHint: Record<CuTargetId, string> = {
  claudeCode: "CLAUDE_CONFIG_DIR 指向的目录（里面有 .claude.json）",
  claudeDesktop: "包含 claude_desktop_config.json 的目录",
  codex: "包含 config.toml 的目录",
  dsh: "包含 cordis.patch.yml 的目录",
  zcode: "包含 config.json 的目录",
  workBuddy: "包含 mcp.json 的目录",
};
// 后端的状态说明里，这两句与徽标完全同义，再显示一行只会把列表拉长。
const redundantMessages = new Set(["已接入", "尚未接入"]);
const removable = (state?: CuTargetState) => state === "registered" || state === "needs_update" || state === "foreign";
const installable = (state?: CuTargetState) => state === "missing" || state === "needs_update" || state === "foreign";

// 语言可在运行中切换，状态文案必须渲染时取，不能缓存在模块级常量里。
function stateLabel(state: CuTargetState): string {
  switch (state) {
    case "registered": return t("已接入");
    case "needs_update": return t("需更新");
    case "foreign": return t("被占用");
    case "unsupported": return t("不支持");
    default: return t("未接入");
  }
}
function failureText(check: CuCheck): string {
  switch (check.failureKind) {
    case "runtime_missing": return t("没找到 ChatGPT.app，请先安装 Codex 桌面版");
    case "untrusted_parent": return t("Computer Use 拒绝了本次调用：ChatGPT.app 版本不兼容，请更新后再试");
    case "permission_required": return t("请在系统设置里给 ChatGPT 授予辅助功能/屏幕录制权限");
    case "client_unsupported": return t("当前 Claude Code 版本不支持审批表单（elicitation）");
    case "service_unavailable": return t("Computer Use 服务未就绪，先打开一次 Codex 的 Computer Use");
    case "error": return check.failureDetail || check.message || t("自检失败，请查看详情后重试。");
    default: return check.failureDetail || check.message || t("自检失败，请查看详情后重试。");
  }
}

export function ComputerUsePage() {
  const [status, setStatus] = useState<CuStatus | null>(null);
  const [check, setCheck] = useState<CuCheck | null>(null);
  const [backups, setBackups] = useState<Record<string, string>>({});
  const [newKind, setNewKind] = useState<CuTargetId>("claudeCode");
  const [newPath, setNewPath] = useState("");
  const [tab, setTab] = useState<CuTab>("overview");
  // 图下方「配置路径与自定义目录」折叠区：图里的「添加自定义配置目录」会展开并滚到这里。
  const [showTargets, setShowTargets] = useState(false);
  const targetsDetails = useRef<HTMLDetailsElement>(null);
  const [busy, setBusy] = useState("");
  const [readError, setReadError] = useState("");
  const [actionError, setActionError] = useState("");
  const [notice, setNotice] = useState("");
  const [approval, setApproval] = useState<CuApprovalState | null>(null);
  const [extStatus, setExtStatus] = useState<{ installed: boolean; storeUrl: string } | null>(null);
  const mounted = useRef(false);
  const epoch = useRef(0);
  const lock = useRef(false);

  const loadApproval = useCallback(async () => {
    try {
      const next = await invoke<CuApprovalState>("computer_use_approval_get");
      if (mounted.current) setApproval(next);
    } catch (reason) {
      if (mounted.current) setActionError(errorText(reason));
    }
  }, []);

  const changeApprovalMode = async (mode: CuApprovalMode) => {
    if (lock.current) return;
    lock.current = true;
    setBusy("approval");
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuResult>("computer_use_approval_set", { mode });
      if (!mounted.current) return;
      if (result.status === "ok") {
        setNotice(result.message || t("审批策略已保存。"));
        await loadApproval();
      } else {
        setActionError(result.message || t("保存审批策略失败。"));
      }
    } catch (reason) {
      if (mounted.current) setActionError(errorText(reason));
    } finally {
      if (mounted.current) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const forgetApproval = async (app: string) => {
    if (lock.current) return;
    lock.current = true;
    setBusy(`forget:${app}`);
    try {
      const result = await invoke<CuResult>("computer_use_approval_forget", { app });
      if (mounted.current && result.status === "ok") {
        setNotice(result.message || t("已撤销。"));
        await loadApproval();
      }
    } catch (reason) {
      if (mounted.current) setActionError(errorText(reason));
    } finally {
      if (mounted.current) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const clearApprovals = async () => {
    if (lock.current) return;
    lock.current = true;
    setBusy("approval:clear");
    try {
      const result = await invoke<CuResult>("computer_use_approval_clear");
      if (mounted.current && result.status === "ok") {
        setNotice(result.message || t("已清空。"));
        await loadApproval();
      }
    } catch (reason) {
      if (mounted.current) setActionError(errorText(reason));
    } finally {
      if (mounted.current) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const loadStatus = useCallback(async () => {
    if (lock.current) return;
    lock.current = true;
    const stamp = ++epoch.current;
    setBusy("refresh");
    setReadError("");
    setActionError("");
    setNotice("");
    try {
      const next = await invoke<CuStatus>("computer_use_status");
      if (!mounted.current || epoch.current !== stamp) return;
      setStatus(next);
      if (next.status !== "ok") setReadError(next.message || t("检测失败，请重试。"));
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setReadError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  }, []);

  useEffect(() => {
    mounted.current = true;
    void loadStatus();
    void loadApproval();
    void invoke<{ installed: boolean; storeUrl: string }>("computer_use_browser_extension_status")
      .then((r) => { if (mounted.current) setExtStatus(r); })
      .catch(() => { /* 非致命，静默忽略 */ });
    return () => {
      mounted.current = false;
      epoch.current += 1;
      lock.current = false;
    };
  }, [loadStatus]);

  const openChatGpt = async () => {
    if (lock.current) return;
    lock.current = true;
    const stamp = ++epoch.current;
    setBusy("open");
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuResult>("computer_use_open_app");
      if (!mounted.current || epoch.current !== stamp) return;
      if (result.status === "ok") setNotice(t("已请求打开 ChatGPT.app。"));
      else setActionError(result.message || t("打开 ChatGPT.app 失败。"));
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setActionError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  /** `target` 是内置目标 id 或自定义目标的 `custom-…` id。 */
  const mutateTarget = async (target: string, action: "install" | "uninstall") => {
    if (lock.current) return;
    lock.current = true;
    const stamp = ++epoch.current;
    const custom = status?.customTargets?.find((item) => item.id === target);
    const label = custom
      ? `${custom.label} · ${t("自定义")}`
      : status?.targets?.[target as CuTargetId]?.label || targetFallback[target as CuTargetId]?.label || target;
    setBusy(`${action}:${target}`);
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuMutation>(action === "install" ? "computer_use_install" : "computer_use_uninstall", { target });
      if (!mounted.current || epoch.current !== stamp) return;
      if (result.targets || result.customTargets) {
        setStatus((current) => (current ? {
          ...current,
          targets: { ...current.targets, ...result.targets },
          customTargets: result.customTargets ?? current.customTargets,
        } : current));
      }
      if (result.status === "ok") {
        setNotice(action === "install" ? `${label} · ${t("已接入，重启客户端后加载工具")}` : `${label} · ${t("已移除")}`);
        setBackups((current) => {
          const next = { ...current };
          if (action === "install" && result.backupPath) next[target] = result.backupPath;
          else delete next[target];
          return next;
        });
      } else {
        setActionError(result.message || t("操作失败，请重试。"));
      }
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setActionError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const pickCustomDir = async () => {
    try {
      const picked = await open({ directory: true, multiple: false, title: t("选择客户端实际读取的配置目录") });
      if (typeof picked === "string" && mounted.current) setNewPath(picked);
    } catch (reason) {
      if (mounted.current) setActionError(errorText(reason));
    }
  };

  const addCustomTarget = async () => {
    if (lock.current || !newPath.trim()) return;
    lock.current = true;
    const stamp = ++epoch.current;
    setBusy("custom:add");
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuStatus>("computer_use_custom_add", { kind: newKind, path: newPath.trim() });
      if (!mounted.current || epoch.current !== stamp) return;
      if (result.status === "ok") {
        setStatus(result);
        setNewPath("");
        setNotice(result.message || t("已添加自定义目标；点「接入」写入配置。"));
      } else {
        setActionError(result.message || t("添加失败，请重试。"));
      }
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setActionError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const removeCustomTarget = async (target: CuCustomTarget) => {
    if (lock.current) return;
    const confirmed = await confirm(
      `${t("删除后会先移除写入")} ${target.configPath} ${t("的 Codex++ 条目（别人写的不动），再从列表里去掉。")}`,
      { title: t("删除自定义目标"), kind: "warning" },
    );
    if (!confirmed || !mounted.current) return;
    lock.current = true;
    const stamp = ++epoch.current;
    setBusy(`custom:remove:${target.id}`);
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuStatus>("computer_use_custom_remove", { id: target.id });
      if (!mounted.current || epoch.current !== stamp) return;
      if (result.status === "ok") {
        setStatus(result);
        setBackups((current) => {
          const next = { ...current };
          delete next[target.id];
          return next;
        });
        setNotice(result.message || t("已删除自定义目标。"));
      } else {
        setActionError(result.message || t("删除失败，请重试。"));
      }
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setActionError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const runCheck = async () => {
    if (lock.current) return;
    lock.current = true;
    const stamp = ++epoch.current;
    setBusy("check");
    setActionError("");
    setNotice("");
    try {
      const result = await invoke<CuCheck>("computer_use_check");
      if (!mounted.current || epoch.current !== stamp) return;
      setCheck(result);
    } catch (reason) {
      if (mounted.current && epoch.current === stamp) setActionError(errorText(reason));
    } finally {
      if (mounted.current && epoch.current === stamp) {
        lock.current = false;
        setBusy("");
      }
    }
  };

  const locked = Boolean(busy);
  const loading = busy === "refresh" && !status;
  const found = status?.runtimeFound === true;
  const apps = (check?.sampleApps ?? []).slice(0, 8);

  // 已接入的客户端数：内置目标 + 自定义目标，「需更新」也算已接入。
  const connectedCount = [
    ...targetIds.map((id) => status?.targets?.[id]),
    ...(status?.customTargets ?? []),
  ].filter((target) => target?.registered).length;
  const activeTab = tab;
  const cuTabs: Array<{ id: CuTab; label: string }> = [
    { id: "overview", label: t("总览") },
    { id: "preview", label: t("窗口预览") },
    { id: "diagnose", label: t("诊断") },
  ];
  const approvalMode: CuApprovalMode = approval?.mode ?? "auto";

  /** 图里的一个客户端：状态点 + 名称 + 一键接入/移除。路径等细节放在悬停提示与下方折叠区。 */
  const flowClient = (id: string, label: string, target: CuTarget | undefined, custom?: CuCustomTarget) => {
    const state = target?.state ?? "missing";
    const working = busy === `install:${id}` || busy === `uninstall:${id}`;
    const tip = [target?.configPath, target?.message].filter(Boolean).join("\n");
    return <div className="cu-flow-client" data-state={status ? state : "unknown"} key={id} title={tip || undefined}>
      <span className="cu-flow-dot" aria-hidden="true" />
      <span className="cu-flow-client-name">{label}{custom && <em>{t("自定义")}</em>}</span>
      {!status
        ? <span className="cu-flow-client-state">{t("未检测")}</span>
        : state === "registered"
          ? <>
              <span className="cu-flow-client-state">{t("已接入")}</span>
              <button className="cu-flow-icon-button" type="button" disabled={locked} title={tf("从 {0} 移除", [label])} aria-label={tf("从 {0} 移除", [label])} onClick={() => void mutateTarget(id, "uninstall")}>
                {working ? <Loader2 size={12} className="cu-spin" /> : <Unplug size={12} />}
              </button>
            </>
          : state === "unsupported"
            ? <span className="cu-flow-client-state">{stateLabel(state)}</span>
            : <button className="cu-flow-client-action" type="button" disabled={locked || !found} title={!found ? t("先安装并打开一次 ChatGPT 后再接入") : target?.message} onClick={() => void mutateTarget(id, "install")}>
                {working ? <Loader2 size={12} className="cu-spin" /> : null}{state === "needs_update" ? t("更新") : t("接入")}
              </button>}
    </div>;
  };

  const flowNode = (icon: ReactNode, title: string, detail: ReactNode, tone?: string, extra?: ReactNode) => (
    <div className="cu-flow-node" data-tone={tone}>
      <div className="cu-flow-node-main">
        <span className="cu-flow-icon" aria-hidden="true">{icon}</span>
        <span className="cu-flow-text">
          <strong>{title}</strong>
          <small>{detail}</small>
        </span>
      </div>
      {extra}
    </div>
  );

  const openTargetDetails = () => {
    setShowTargets(true);
    // 等 <details> 展开后再定位：直接滚到添加表单并聚焦输入框，而不是停在列表开头。
    requestAnimationFrame(() => {
      const form = targetsDetails.current?.querySelector<HTMLElement>(".cu-custom-add");
      form?.scrollIntoView({ behavior: "smooth", block: "center" });
      form?.querySelector<HTMLInputElement>(".cu-custom-path")?.focus({ preventScroll: true });
    });
  };

  /**
   * 交互式工作原理图：客户端 → Codex++ 两个 MCP 服务 → Codex 运行时 → 被操作的对象。
   *
   * 接入与审批直接在图上操作：左列每个客户端一键接入/移除；审批方式开关放在 codex-plus-cu
   * 节点上，因为审批就发生在这一步。用 CSS grid 而不是 SVG，文字随窗口换行、跟随主题。
   */
  const renderFlow = () => <section className="cu-card cu-flow-card" aria-labelledby="cu-flow-title">
    <div className="cu-card-head">
      <div>
        <h2 id="cu-flow-title">{t("工作原理")}</h2>
        <p>{t("Codex++ 把 ChatGPT.app 里现成的 Computer Use 与 Chrome 扩展，通过 MCP 转给其它 AI 客户端。左边点「接入」，codex-plus-cu 上选审批方式。")}</p>
      </div>
    </div>
    <div className="cu-flow-scroll">
      <div className="cu-flow">
        <span className="cu-flow-col" style={{ gridColumn: 1 }}><b>1</b>{t("你的 AI 客户端")}{status ? <i>{tf("已接入 {0} 个", [connectedCount])}</i> : null}</span>
        <span className="cu-flow-col" style={{ gridColumn: 3 }}><b>2</b>{t("Codex++ MCP 服务")}</span>
        <span className="cu-flow-col" style={{ gridColumn: 5 }}><b>3</b>{t("Codex 运行时")}</span>
        <span className="cu-flow-col" style={{ gridColumn: 7 }}><b>4</b>{t("被操作的对象")}</span>

        <div className="cu-flow-clients">
          {targetIds.map((id) => {
            const target = status?.targets?.[id];
            return flowClient(id, target?.label || targetFallback[id].label, target);
          })}
          {(status?.customTargets ?? []).map((target) => flowClient(target.id, target.label, target, target))}
          <button className="cu-flow-add" type="button" onClick={openTargetDetails}><Plus size={12} aria-hidden="true" />{t("添加自定义配置目录")}</button>
        </div>
        <div className="cu-flow-link cu-flow-link-fan"><span>MCP</span></div>

        <div className="cu-flow-lane-a" style={{ gridColumn: 3 }}>
          {flowNode(<MousePointerClick size={16} />, "codex-plus-cu", t("桌面操控"), "cu",
            <div className="cu-flow-approval">
              <span>{t("首次接管某个应用时")}</span>
              <div className="mg-segmented" role="radiogroup" aria-label={t("审批策略")}>
                <button type="button" role="radio" aria-checked={approvalMode === "auto"} data-active={approvalMode === "auto"} disabled={locked}
                  title={t("自动放行所有应用，只记审计日志。")} onClick={() => void changeApprovalMode("auto")}>{t("自动放行")}</button>
                <button type="button" role="radio" aria-checked={approvalMode === "claude"} data-active={approvalMode === "claude"} disabled={locked}
                  title={t("把许可请求转给 MCP 客户端，由 Claude 侧弹卡确认；拒绝即不放行。")} onClick={() => void changeApprovalMode("claude")}>{t("交给 Claude")}</button>
              </div>
              {approvalMode === "claude" && approval?.lastClient?.elicitation === false
                ? <small className="cu-flow-warn">{t("最近连接的客户端不支持审批卡，会退回自动放行")}</small>
                : null}
            </div>)}
        </div>
        <div className="cu-flow-link cu-flow-lane-a" style={{ gridColumn: 4 }}><span><ShieldCheck size={12} aria-hidden="true" />{t("审批")}</span></div>
        <div className="cu-flow-lane-a" style={{ gridColumn: 5 }}>
          {flowNode(<MonitorSmartphone size={16} />, "ChatGPT.app",
            !status ? t("检测中…") : found ? t("原生 Computer Use · 已找到") : t("未找到，先安装 Codex 桌面版"),
            status && !found ? "warn" : undefined)}
        </div>
        <div className="cu-flow-link cu-flow-lane-a" style={{ gridColumn: 6 }} />
        <div className="cu-flow-lane-a" style={{ gridColumn: 7 }}>{flowNode(<AppWindow size={16} />, t("本机桌面应用"), t("Finder、微信、系统设置…"), "target")}</div>

        <div className="cu-flow-lane-b" style={{ gridColumn: 3 }}>{flowNode(<Globe size={16} />, "codex-plus-browser", t("浏览器控制"), "browser")}</div>
        <div className="cu-flow-link cu-flow-lane-b" style={{ gridColumn: 4 }} />
        <div className="cu-flow-lane-b" style={{ gridColumn: 5 }}>
          {flowNode(<Plug size={16} />, t("Chrome 扩展"),
            extStatus == null ? t("检测中…") : extStatus.installed ? t("ChatGPT for Chrome · 已安装") : t("未安装，浏览器工具不可用"),
            extStatus && !extStatus.installed ? "warn" : undefined,
            extStatus && !extStatus.installed
              ? <button className="cu-flow-client-action" type="button" disabled={locked} onClick={() => void invoke("computer_use_open_extension_store")}>{t("安装扩展")}</button>
              : null)}
        </div>
        <div className="cu-flow-link cu-flow-lane-b" style={{ gridColumn: 6 }} />
        <div className="cu-flow-lane-b" style={{ gridColumn: 7 }}>{flowNode(<Globe size={16} />, t("Chrome 标签页"), t("你已登录的真实浏览器"), "target")}</div>
      </div>
    </div>
    <p className="cu-flow-foot">{t("接入后重启对应客户端，工具名以 mcp__codex-plus-cu__ 与 mcp__codex-plus-browser__ 开头。")}</p>
  </section>;

  /** 一行接入目标。内置与自定义共用；传了 `custom` 才显示「自定义」标记和删除按钮。 */
  const renderTarget = (id: string, target: CuTarget | undefined, label: string, configPath: string, custom?: CuCustomTarget) => {
    const state = target?.state ?? "missing";
    const backup = backups[id];
    const installing = busy === `install:${id}`;
    const removing = busy === `uninstall:${id}`;
    const deleting = busy === `custom:remove:${id}`;
    return <div className="cu-target" key={id} data-custom={custom ? "true" : undefined}>
      <div className="cu-target-main">
        <div className="cu-target-title">
          <strong>{label}</strong>
          {custom && <span className="cu-badge cu-badge-custom">{t("自定义")}</span>}
          <span className="cu-badge" data-state={status ? state : "unknown"}>{status ? stateLabel(state) : t("未检测")}</span>
        </div>
        <code className="cu-target-path" title={configPath}>{elidePath(configPath)}</code>
        {target?.message && !redundantMessages.has(target.message) && <small className="cu-target-message">{target.message}</small>}
        {backup && <small className="cu-target-backup">{t("已备份到")}<code title={backup}>{elidePath(backup)}</code></small>}
      </div>
      <div className="cu-target-actions">
        {custom && <button className="mg-button" type="button" disabled={locked} title={t("移除写入的条目并删除这个自定义目标")} onClick={() => void removeCustomTarget(custom)}>
          {deleting ? <Loader2 size={14} className="cu-spin" /> : <Trash2 size={14} />}{deleting ? t("删除中…") : t("删除")}
        </button>}
        <button className="mg-button" type="button" disabled={locked || !status || !removable(state)} onClick={() => void mutateTarget(id, "uninstall")}>
          {removing ? <Loader2 size={14} className="cu-spin" /> : <Unplug size={14} />}{removing ? t("移除中…") : t("移除")}
        </button>
        <button className="mg-button mg-primary" type="button" disabled={locked || !status || !found || !installable(state)} title={status && !found ? t("先安装并打开一次 ChatGPT 后再接入") : state === "unsupported" ? t("该客户端不支持接入") : undefined} onClick={() => void mutateTarget(id, "install")}>
          {installing ? <Loader2 size={14} className="cu-spin" /> : <Plug size={14} />}{installing ? t("接入中…") : t("接入")}
        </button>
      </div>
    </div>;
  };

  return <div className="mg-page cu-page">
    <PageHeader icon={<MonitorSmartphone size={20} strokeWidth={1.5} />} title="Computer Use" actions={
      <button className="mg-button" type="button" onClick={() => void loadStatus()} disabled={locked} title={t("重新检测本机运行时与接入状态")}>
        <RefreshCw size={15} className={busy === "refresh" ? "cu-spin" : ""} />{busy === "refresh" ? t("检测中…") : t("重新检测")}
      </button>
    } />
    <div className="cu-content" aria-busy={loading}>
      {readError && <p className="cu-error" role="alert">{t("读取状态失败：")}{readError}</p>}
      {actionError && <p className="cu-error" role="alert">{t("操作失败：")}{actionError}</p>}
      {notice && <p className="cu-notice" role="status">{notice}</p>}
      {loading && <p className="cu-loading" role="status"><Loader2 size={15} className="cu-spin" />{t("正在检测本机运行时…")}</p>}

      {/* 运行时缺失会让所有分页都不可用，放在分页之外，在哪一页都看得到。 */}
      {status && !found && <div className="cu-runtime-missing" role="status">
        <AlertTriangle size={16} />
        <div>
          <strong>{t("没找到 ChatGPT.app")}</strong>
          <p>{status.configHint || t("请先安装 Codex 桌面版，安装后点「重新检测」。")}</p>
        </div>
        <button className="mg-button" type="button" onClick={() => void openChatGpt()} disabled={locked}>
          {busy === "open" ? <Loader2 size={14} className="cu-spin" /> : <Play size={14} />}{busy === "open" ? t("正在打开…") : t("打开 ChatGPT")}
        </button>
      </div>}

      <div className="mg-tabs cu-tabs" role="tablist" aria-label={t("Computer Use 分页")}>
        {cuTabs.map((item) => <button key={item.id} type="button" role="tab" aria-selected={activeTab === item.id} data-active={activeTab === item.id} onClick={() => setTab(item.id)}>{item.label}</button>)}
      </div>

      {activeTab === "overview" && <>
        {renderFlow()}
        {status?.claudeDesktopRunning ? (
          <p className="cu-note cu-note-warn">{t("Claude Desktop 正在运行：它退出时会重写自己的配置，接入前建议先完全退出 Claude Desktop，改完再打开。")}</p>
        ) : null}

        <details className="cu-more" ref={targetsDetails} open={showTargets} onToggle={(event) => setShowTargets(event.currentTarget.open)}>
          <summary><strong>{t("配置路径与自定义目录")}</strong><small>{t("每个客户端写入的配置文件与备份位置；客户端换了配置目录时在这里添加。")}</small></summary>
          <section className="cu-card" aria-labelledby="cu-targets-title">
            <div className="cu-card-head">
              <div>
                <h2 id="cu-targets-title">{t("接入目标")}</h2>
                <p>{t("登记两个 MCP 服务：桌面操控（直通 Codex 原生 CU）与 Chrome 浏览器控制。写入前会自动备份原配置，只改自己写入的条目。")}</p>
              </div>
            </div>
            <div className="cu-targets">
              {targetIds.map((id) => {
                const fallback = targetFallback[id];
                const target = status?.targets?.[id];
                return renderTarget(id, target, target?.label || fallback.label, target?.configPath || fallback.configPath);
              })}
              {(status?.customTargets ?? []).map((target) => renderTarget(target.id, target, target.label, target.configPath, target))}
            </div>

            <div className="cu-custom-add">
              <div className="cu-custom-add-head">
                <strong>{t("添加自定义配置目录")}</strong>
                <p className="cu-hint">{t("客户端改用了别的配置目录时（例如被包装器设置了 CLAUDE_CONFIG_DIR），上面的默认路径它读不到。在这里登记它实际读取的目录，可添加任意多个；也可以直接填配置文件路径。")}</p>
              </div>
              <div className="cu-custom-add-row">
                <select
                  className="cu-custom-kind"
                  value={newKind}
                  disabled={locked}
                  aria-label={t("客户端类型")}
                  onChange={(event) => setNewKind(event.target.value as CuTargetId)}
                >
                  {customKinds.map((kind) => <option key={kind} value={kind}>{targetFallback[kind].label}</option>)}
                </select>
                <input
                  className="cu-custom-path"
                  type="text"
                  value={newPath}
                  disabled={locked}
                  spellCheck={false}
                  placeholder={t(customDirHint[newKind])}
                  aria-label={t("配置目录")}
                  onChange={(event) => setNewPath(event.target.value)}
                  onKeyDown={(event) => { if (event.key === "Enter") void addCustomTarget(); }}
                />
                <button className="mg-button" type="button" disabled={locked} onClick={() => void pickCustomDir()}>
                  <FolderOpen size={14} />{t("选择目录")}
                </button>
                <button className="mg-button mg-primary" type="button" disabled={locked || !newPath.trim()} onClick={() => void addCustomTarget()}>
                  {busy === "custom:add" ? <Loader2 size={14} className="cu-spin" /> : <Plus size={14} />}{t("添加")}
                </button>
              </div>
            </div>
          </section>
        </details>

        <details className="cu-more">
          <summary><strong>{t("审批记录")}</strong><small>{t("最近连接的客户端是否支持审批卡、浏览器回合钩子、已记住的应用。")}</small></summary>
          <section className="cu-card" aria-labelledby="cu-approval-title">
            <div className="cu-card-head">
              <div>
                <h2 id="cu-approval-title">{t("审批记录")}</h2>
                <p>{t("审批方式在上图的 codex-plus-cu 节点里切换；这里是客户端能力与已记住的应用。")}</p>
              </div>
            </div>
            {approval?.lastClient && <p
              className={approval.mode === "claude" && approval.lastClient.elicitation === false ? "cu-note cu-note-warn" : "cu-hint"}
              role={approval.mode === "claude" && approval.lastClient.elicitation === false ? "alert" : undefined}
            >
              {t("最近连接的客户端：")}<code>{[approval.lastClient.name, approval.lastClient.version].filter(Boolean).join(" ") || t("未知")}</code>
              {approval.lastClient.elicitation
                ? t("，支持审批卡。")
                : approval.mode === "claude"
                  ? t("，未声明审批卡能力（elicitation）。在这个客户端里「交给 Claude 审批」会退回为自动放行，不会弹卡。")
                  : t("，未声明审批卡能力（elicitation）；如果切到「交给 Claude 审批」，在这个客户端里会退回为自动放行。")}
            </p>}
            {approval?.turnHooks && <p className={approval.turnHooks.installed ? "cu-hint" : "cu-note cu-note-warn"}>
              {approval.turnHooks.installed
                ? t("浏览器回合钩子已装好：") + (approval.turnHooks.events ?? []).join(" / ") + t("，回合结束时自动关闭未标记的标签页。")
                : t("未安装浏览器回合钩子。安装 Claude Code 接入后，回合结束时未标记的标签页不会自动关闭，会越堆越多。")}
            </p>}
            {(approval?.approvals?.length ?? 0) > 0 && <div className="cu-approvals">
              <div className="cu-approvals-head">
                <strong>{t("已记忆的应用")}</strong>
                <button className="mg-button" type="button" disabled={locked} onClick={() => void clearApprovals()}>
                  {busy === "approval:clear" ? <Loader2 size={14} className="cu-spin" /> : null}{t("全部清空")}
                </button>
              </div>
              <ul className="cu-approval-list">
                {approval!.approvals!.map(([key, entry]) => (
                  <li key={key}>
                    <span className="cu-approval-name">{entry.displayName || key}</span>
                    <code className="cu-approval-key" title={key}>{key}</code>
                    <span className="cu-approval-decision" data-decision={entry.decision}>{entry.decision === "allow" ? t("允许") : t("拒绝")}</span>
                    <button className="mg-button" type="button" disabled={locked} onClick={() => void forgetApproval(key)}>
                      {busy === `forget:${key}` ? <Loader2 size={13} className="cu-spin" /> : null}{t("撤销")}
                    </button>
                  </li>
                ))}
              </ul>
            </div>}
            {approval && (approval.approvals?.length ?? 0) === 0 && <p className="cu-hint">{t("还没有记住任何应用。选择「交给 Claude 审批」后，Claude 批准过的应用会出现在这里。")}</p>}
          </section>
        </details>
      </>}

      {activeTab === "preview" && <>
        <ComputerUsePreview />
      </>}

      {activeTab === "diagnose" && <>
        <section className="cu-card cu-runtime" data-found={found} aria-labelledby="cu-runtime-title">
          <div className="cu-card-head">
            <div>
              <h2 id="cu-runtime-title">{t("运行时")}</h2>
              <p>{t("Computer Use 的服务由本机 ChatGPT.app 提供，接入前先确认它能被找到。")}</p>
            </div>
            <span className="cu-badge" data-state={!status ? "unknown" : found ? "ok" : "missing"}>{!status ? t("未检测") : found ? t("已找到") : t("未找到")}</span>
          </div>
          <dl className="cu-meta">
            <dt>{t("应用路径")}</dt>
            <dd>{hasValue(status?.appPath) ? <code title={status?.appPath}>{status?.appPath}</code> : "—"}</dd>
            <dt>@oai/cua-repl</dt>
            <dd>{hasValue(status?.packageVersion) ? <code>{status?.packageVersion}</code> : "—"}</dd>
            <dt>{t("CU server")}</dt>
            <dd><code>SkyComputerUseClient</code></dd>
            <dt>{t("CU 服务路径")}</dt>
            <dd>{hasValue(status?.servicePath) ? <code title={status?.servicePath}>{status?.servicePath}</code> : "—"}</dd>
          </dl>
        </section>

        <section className="cu-card" aria-labelledby="cu-ext-title">
          <div className="cu-card-head">
            <div>
              <h2 id="cu-ext-title">{t("Chrome 浏览器扩展")}</h2>
              <p>{t("浏览器控制依赖 OpenAI 官方的 ChatGPT for Chrome 扩展。装好后无需额外配置，刷新页面即生效。")}</p>
            </div>
          </div>
          <div className="cu-ext-row">
            {extStatus == null
              ? <span className="cu-hint">{t("检测中…")}</span>
              : extStatus.installed
                ? <span className="cu-ext-ok"><CheckCircle2 size={14} />{t("扩展已安装")}</span>
                : <>
                    <span className="cu-note cu-note-warn">{t("未检测到扩展，浏览器工具不可用。")}</span>
                    <button
                      className="mg-button mg-primary"
                      type="button"
                      disabled={locked}
                      onClick={() => void invoke("computer_use_open_extension_store")}
                    >
                      {t("安装 Chrome 扩展 →")}
                    </button>
                  </>
            }
          </div>
        </section>

        <section className="cu-card" aria-labelledby="cu-check-title">
          <div className="cu-card-head">
            <div>
              <h2 id="cu-check-title">{t("连通性检查")}</h2>
              <p>{t("实际跑一次 MCP 握手并调用 sky.list_apps()，确认工具链可用。")}</p>
            </div>
            <button className="mg-button" type="button" onClick={() => void runCheck()} disabled={locked}>
              {busy === "check" ? <Loader2 size={15} className="cu-spin" /> : <Play size={15} />}{busy === "check" ? t("正在自检…") : t("运行自检")}
            </button>
          </div>
          {!check
            ? <p className="cu-hint">{t("还没有运行自检。")}</p>
            : <div className="cu-check" data-ok={check.ok} role="status">
              <div className="cu-check-head">
                {check.ok ? <CheckCircle2 size={16} /> : <AlertTriangle size={16} />}
                <strong>{check.ok ? t("自检通过") : t("自检未通过")}</strong>
              </div>
              <dl className="cu-meta">
                <dt>{t("目标平台")}</dt>
                <dd>{check.target || "—"}</dd>
                <dt>{t("应用数量")}</dt>
                <dd>{typeof check.appCount === "number" ? check.appCount : "—"}</dd>
                <dt>{t("浏览器")}</dt>
                <dd>{typeof check.browserCount === "number" ? check.browserCount : "—"}</dd>
                <dt>{t("耗时")}</dt>
                <dd>{typeof check.durationMs === "number" ? `${check.durationMs} ms` : "—"}</dd>
              </dl>
              {apps.length > 0 && <ul className="cu-app-list" aria-label={t("示例应用")}>{apps.map((app) => <li key={app}>{app}</li>)}</ul>}
              {!check.ok && <p className="cu-failure">{failureText(check)}</p>}
              {check.message && <p className="cu-hint">{check.message}</p>}
            </div>}
        </section>
        <p className="cu-note">{t("接入后需要重启对应客户端（Claude Code 会话、DeepSeek Harness、ZCode、WorkBuddy）才会加载工具，工具名以")}<code>mcp__codex-plus-cu__</code>{t("与")}<code>mcp__codex-plus-browser__</code>{t("前缀出现在工具列表中。")}</p>
        <p className="cu-note">{t("Codex 桌面版与终端 CLI 共用")}<code>~/.codex/config.toml</code>{t("，其中已由 Codex 自己写入")}<code>[mcp_servers.computer-use]</code>{t("，无需重复接入；仅当你要在没有该条目的环境里单独启用时，才使用")}<code>--codex-plus-cu-cli install codex</code>{t("。")}</p>
      </>}
    </div>
  </div>;
}
